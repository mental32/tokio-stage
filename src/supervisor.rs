use std::future::Future;
use std::ops::{ControlFlow, Deref};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, Stream, StreamExt};

use tokio::sync::oneshot;
use tracing::Instrument;

use crate::group::{GroupRef, MakePendingFut, RestartPolicy};
use crate::simple_supervisor::{SimpleSupervisorImpl, SimpleSupervisorMessage};
use crate::task::{TaskIdRepr, TaskKind};
use crate::{Group, MailboxReceiver, MailboxSender};

#[derive(Debug)]
enum Select {
    TaskExit(Option<crate::task::TaskIdRepr>),
    Message(SupervisorMessage),
    Inner(crate::simple_supervisor::Select),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct ChildRef(crate::task::TaskIdRepr);

#[derive(Debug)]
enum SupervisorMessage {
    AddChild(Child, oneshot::Sender<ChildRef>),
    TerminateChild(ChildRef, oneshot::Sender<Result<(), ()>>),
}

#[derive(Debug)]
pub enum Child {
    Worker(Group),
    Super(Supervisor),
}

impl Child {
    fn into_pid(self) -> crate::task::Pid<()> {
        match self {
            Child::Worker(gr) => gr.inner.into(),
            Child::Super(sup) => sup.sv_group.inner.into(),
        }
    }
}

impl Deref for Child {
    type Target = Group;

    fn deref(&self) -> &Self::Target {
        match self {
            Child::Worker(group) => group,
            Child::Super(supervisor) => &supervisor.sv_group,
        }
    }
}

impl From<Group> for Child {
    fn from(value: Group) -> Self {
        Self::Worker(value)
    }
}

impl From<Supervisor> for Child {
    fn from(value: Supervisor) -> Self {
        Self::Super(value)
    }
}

enum ChildState {
    Deleted,
    Active(Child),
}

impl Default for ChildState {
    fn default() -> Self {
        Self::Deleted
    }
}

struct SupervisedChild {
    task_id: crate::task::TaskIdRepr,
    slot: ChildState,
}

struct SupervisorImpl<F> {
    inner: SimpleSupervisorImpl<F>,
    children: Vec<SupervisedChild>,
    strategy: SupervisorStrategy,
    rx: MailboxReceiver<SupervisorMessage>,
}

fn map_to_task_id<T>(task_id: TaskIdRepr) -> impl FnOnce(T) -> TaskIdRepr {
    move |_: T| -> TaskIdRepr { task_id }
}

trait SupervisorFutures: Stream<Item = TaskIdRepr> + Send + Unpin {
    fn is_empty(&self) -> bool;
    fn clear(&mut self);
    fn push_group_ref(&self, group_ref: GroupRef);
}

impl SupervisorFutures for FuturesUnordered<BoxFuture<'static, TaskIdRepr>> {
    #[inline]
    fn is_empty(&self) -> bool {
        FuturesUnordered::is_empty(&self)
    }

    #[inline]
    fn clear(&mut self) {
        FuturesUnordered::clear(self)
    }

    #[inline]
    fn push_group_ref(&self, group_ref: GroupRef) {
        self.push(
            group_ref
                .wait_until_suspended()
                .map(map_to_task_id(group_ref.task_id()))
                .boxed(),
        )
    }
}

impl<F, Fut> SupervisorImpl<F>
where
    F: 'static + Clone + Send + Fn() -> Fut,
    Fut: 'static + Send + Future<Output = ()>,
    Fut::Output: 'static + Send,
{
    #[inline]
    #[track_caller]
    fn handle_task_exit(&mut self, task_id: TaskIdRepr, futs: &mut dyn SupervisorFutures) {
        let (ix, child, task_id) = match self
            .children
            .iter()
            .enumerate()
            .find(|ch| ch.1.task_id == task_id)
        {
            Some((
                ix,
                SupervisedChild {
                    task_id,
                    slot: ChildState::Active(child),
                },
            )) => (ix, child, *task_id),
            _ => return,
        };

        let span = tracing::trace_span!("task-exit", task_id, index = ?ix);
        let _guard = span.enter();

        tracing::trace!("task exit");

        match self.strategy {
            SupervisorStrategy::OneForOne => {
                let resume_result = child.resume();
                tracing::trace!(?resume_result, "resuming task");

                match resume_result {
                    Ok(()) => {
                        tracing::trace!("registering group-ref to observe task");
                        futs.push_group_ref(child.to_group_ref());
                    }
                    Err(()) => {
                        tracing::trace!("child has died abnormally. marking as deleted.");
                        std::mem::drop(child);
                        std::mem::take(&mut self.children[ix].slot);
                    }
                }
            }
            SupervisorStrategy::OneForAll => {
                futs.clear();

                for child in self.children.iter_mut().rev() {
                    let filter = if let ChildState::Active(child) = &child.slot {
                        child.suspend().is_err()
                    } else {
                        false
                    };

                    if filter {
                        child.slot = ChildState::Deleted;
                    }
                }

                for child in self.children.iter_mut() {
                    let filter = if let ChildState::Active(child) = &child.slot {
                        child.resume().is_err()
                    } else {
                        false
                    };

                    if filter {
                        child.slot = ChildState::Deleted;
                    }
                }

                for child in self.children.iter() {
                    if let ChildState::Active(child) = &child.slot {
                        futs.push_group_ref(child.to_group_ref());
                    }
                }
            }
            SupervisorStrategy::RestForAll => {
                futs.clear();

                let (lhs, rhs) = self.children.split_at_mut(ix);

                for child in rhs.iter_mut().skip(1).rev() {
                    let filter = if let ChildState::Active(child) = &child.slot {
                        child.suspend().is_err()
                    } else {
                        false
                    };

                    if filter {
                        child.slot = ChildState::Deleted;
                    }
                }

                for child in rhs.iter_mut() {
                    let filter = if let ChildState::Active(child) = &child.slot {
                        child.resume().is_err()
                    } else {
                        false
                    };

                    if filter {
                        child.slot = ChildState::Deleted;
                    }
                }

                for child in lhs.into_iter().chain(rhs.into_iter()) {
                    if let ChildState::Active(child) = &child.slot {
                        futs.push_group_ref(child.to_group_ref());
                    }
                }
            }
        }
    }

    #[cfg_attr(feature = "backtrace", async_backtrace::framed)]
    async fn select(&mut self, futs: &mut dyn SupervisorFutures) -> Select {
        tokio::select! {
            sel = self.inner.select() => { Select::Inner(sel) }
            fut = futs.next(), if !futs.is_empty() => Select::TaskExit(fut),
            msg = self.rx.recv() => Select::Message(msg),
        }
    }

    #[cfg_attr(feature = "backtrace", async_backtrace::framed)]
    async fn handle_select(
        &mut self,
        sel: Select,
        futs: &mut dyn SupervisorFutures,
    ) -> ControlFlow<()> {
        match sel {
            Select::Inner(crate::simple_supervisor::Select::Message(
                SimpleSupervisorMessage::Exit { timeout },
            )) => {
                self.shutdown_children(timeout).await;
                ControlFlow::Break(())
            }
            Select::Inner(sel) => {
                self.inner.poll(sel).await;
                ControlFlow::Continue(())
            }
            Select::TaskExit(None) => ControlFlow::Continue(()),
            Select::TaskExit(Some(task_id)) => {
                self.handle_task_exit(task_id, futs);
                ControlFlow::Continue(())
            }
            Select::Message(m) => {
                self.handle_supervisor_message(m, futs).await;
                ControlFlow::Continue(())
            }
        }
    }

    #[cfg_attr(feature = "backtrace", async_backtrace::framed)]
    async fn handle_supervisor_message(
        &mut self,
        message: SupervisorMessage,
        futs: &mut dyn SupervisorFutures,
    ) {
        match message {
            SupervisorMessage::TerminateChild(child_ref, tx) => {
                let span = tracing::trace_span!("terminate-child", task_id = child_ref.0);
                let _guard = span.enter();

                let _ = match self
                    .children
                    .iter()
                    .position(|child| child.task_id == child_ref.0)
                {
                    None => tx.send(Err(())),
                    Some(ix) => {
                        tracing::trace!("terminating child by reference");

                        let child = self.children.get_mut(ix).unwrap();

                        let slot = std::mem::take(&mut child.slot);

                        match slot {
                            ChildState::Deleted => tx.send(Ok(())),
                            ChildState::Active(group) => {
                                let _ = group.exit(Duration::from_secs(1)).await;
                                tx.send(Ok(()))
                            }
                        }
                    }
                };
            }
            SupervisorMessage::AddChild(group, tx) => {
                let task_id = group.task_id();

                let span = tracing::trace_span!("add-child", task_id);
                let _guard = span.enter();

                tracing::trace!("adding child to managed set");

                futs.push_group_ref(group.to_group_ref());

                self.children.push(SupervisedChild {
                    task_id,
                    slot: ChildState::Active(group),
                });

                // we dont care if the other side died
                // since we're now managing the child.
                let _ = tx.send(ChildRef(task_id));
            }
        }
    }

    #[cfg_attr(feature = "backtrace", async_backtrace::framed)]
    async fn shutdown_children(&mut self, timeout: Duration) {
        let mut futs = FuturesUnordered::new();

        for child in self
            .children
            .iter_mut()
            .map(|child| std::mem::take(&mut child.slot))
            .filter_map(|ch_state| match ch_state {
                ChildState::Deleted => None,
                ChildState::Active(child) => Some(child),
            })
        {
            if let Ok(()) = child.exit(timeout).await {
                futs.push(child.into_pid());
            }
        }

        crate::simple_supervisor::timeout_pids(
            Duration::from_millis(timeout.as_millis() as u64 + 100),
            &mut futs,
        )
        .await;
    }
}

#[cfg_attr(feature = "backtrace", async_backtrace::framed)]
async fn supervisor_impl<F, Fut>(
    sv: SimpleSupervisorImpl<F>,
    rx: MailboxReceiver<SupervisorMessage>,
    strategy: SupervisorStrategy,
) where
    F: 'static + Clone + Send + Fn() -> Fut,
    Fut: 'static + Send + std::future::Future<Output = ()>,
    Fut::Output: 'static + Send,
{
    let mut sv_impl = SupervisorImpl {
        rx,
        inner: sv,
        strategy,
        children: vec![],
    };

    let mut futs = FuturesUnordered::new();

    loop {
        let sel = sv_impl.select(&mut futs).await;

        match sv_impl.handle_select(sel, &mut futs).await {
            ControlFlow::Continue(()) => continue,
            ControlFlow::Break(()) => return,
        };
    }
}

/// An owned handle to the underlying supervisor
#[derive(Debug)]
pub struct Supervisor {
    sv_tx: MailboxSender<SupervisorMessage>,
    sv_group: Group,
}

impl Supervisor {
    /// send a child to be managed by this supervisor.
    #[cfg_attr(feature = "backtrace", async_backtrace::framed)]
    pub async fn add_child<T>(&self, child: T) -> ChildRef
    where
        T: Into<Child>,
    {
        let (tx, rx) = oneshot::channel();
        self.sv_tx
            .send(SupervisorMessage::AddChild(child.into(), tx))
            .await;

        rx.await.expect("could not add supervisor child")
    }

    /// terminate a child that is currently being supervised
    #[cfg_attr(feature = "backtrace", async_backtrace::framed)]
    pub async fn terminate_child(&self, child_ref: ChildRef) -> Result<(), ()> {
        let (tx, rx) = oneshot::channel();
        self.sv_tx
            .send(SupervisorMessage::TerminateChild(child_ref, tx))
            .await;

        rx.await.expect("could not add supervisor child")
    }

    /// signal to the supervisor to terminate its children recursively and then shutdown.
    #[cfg_attr(feature = "backtrace", async_backtrace::framed)]
    pub async fn exit(self, timeout: Duration) {
        match self.sv_group.exit(timeout).await {
            Ok(()) => (),
            Err(_) => (),
        }
    }
}

/// Supervisor strategies dictate how the supervisor restarts worker tasks
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorStrategy {
    /// If a child process terminates, only that process is restarted.
    OneForOne,
    /// If a child process terminates, all other child processes are terminated, and then all child processes, including the terminated one, are restarted.
    OneForAll,
    /// If a child process terminates, the rest of the child processes are terminated.
    ///
    /// Then the terminated child process and the rest of the child processes are restarted.
    RestForAll,
}

/// create and spawn a supervisor with the specified strategy.
pub fn supervisor(strategy: SupervisorStrategy) -> Supervisor {
    let (tx, rx) = crate::mailbox(1);
    let inner = crate::group()
        .spawn_at_least(1)
        .task_kind(TaskKind::Super)
        .restart_policy(RestartPolicy::OnPanic)
        .spawn_custom(move |sv, (sv_tx, sv_stat, sv_suspend)| {
            let fut = async move {
                supervisor_impl::<MakePendingFut, _>(sv, rx, strategy)
                    .instrument(tracing::trace_span!(
                        "supervisor",
                        current_task_id = crate::task::current_task_id()
                    ))
                    .await;
            };

            let pid = crate::task::spawn(fut, crate::task::TaskKind::Super);

            crate::group::Inner::new(sv_tx, sv_stat, pid, sv_suspend)
        });

    Supervisor {
        sv_tx: tx,
        sv_group: crate::group::Group { inner },
    }
}

#[cfg(test)]
mod test {
    use tokio::sync::Notify;

    #[tokio::test]
    async fn test_supervisor_spawn_example_task() {
        static NOTIFY: Notify = Notify::const_new();

        async fn example_task() {
            NOTIFY.notify_waiters();
            std::future::pending().await
        }

        let sup = super::supervisor(crate::SupervisorStrategy::OneForOne);

        let notfied = NOTIFY.notified();

        let group = crate::group().spawn(example_task);

        let child_ref = sup.add_child(group).await;

        notfied.await;

        sup.terminate_child(child_ref).await.unwrap();
    }

    #[tokio::test]
    async fn test_supervisor_spawn_nested_supervisor() {
        static NOTIFY: Notify = Notify::const_new();

        async fn example_task() {
            NOTIFY.notify_waiters();
            std::future::pending().await
        }

        let sup2 = super::supervisor(crate::SupervisorStrategy::OneForOne);

        let sup = super::supervisor(crate::SupervisorStrategy::OneForOne);

        let notfied = NOTIFY.notified();

        let group = crate::group().spawn(example_task);
        let _ = sup.add_child(group).await;

        let child_ref = sup2.add_child(sup).await;

        notfied.await;

        sup2.terminate_child(child_ref).await.unwrap();
    }
}
