use std::ops::Deref;
use std::time::Duration;

use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};

use tokio::sync::oneshot;
use tracing::Instrument;

use crate::group::{MakePendingFut, RestartPolicy};
use crate::simple_supervisor::{SimpleSupervisorImpl, SimpleSupervisorMessage};
use crate::task::{TaskId, TaskKind};
use crate::{Group, MailboxReceiver, MailboxSender};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct ChildRef(crate::task::TaskId);

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
    task_id: crate::task::TaskId,
    slot: ChildState,
}

struct SupervisorImpl {
    children: Vec<SupervisedChild>,
}

impl SupervisorImpl {
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

async fn supervisor_impl<F, Fut>(
    mut sv: SimpleSupervisorImpl<F>,
    rx: MailboxReceiver<SupervisorMessage>,
    strategy: SupervisorStrategy,
) where
    F: 'static + Clone + Send + Fn() -> Fut,
    Fut: 'static + Send + std::future::Future<Output = ()>,
    Fut::Output: 'static + Send,
{
    let mut sv_impl = SupervisorImpl { children: vec![] };
    let mut futs = FuturesUnordered::new();

    #[derive(Debug)]
    enum Select {
        Shutdown,
        TaskExit(Option<crate::task::TaskId>),
        Message(SupervisorMessage),
        Inner(crate::simple_supervisor::Select),
    }

    fn map_to_task_id<T>(task_id: TaskId) -> impl FnOnce(T) -> TaskId {
        move |_: T| -> TaskId { task_id }
    }

    // let notify = crate::graceful_shutdown::SHUTDOWN_NOTIFY.try_with(|n| Arc::clone(&n));

    loop {
        let sel = tokio::select! {
            sel = sv.select() => { Select::Inner(sel) }
            fut = futs.next(), if !futs.is_empty() => Select::TaskExit(fut),
            msg = rx.recv() => Select::Message(msg),
        };

        let m = match sel {
            Select::Inner(crate::simple_supervisor::Select::Message(
                SimpleSupervisorMessage::Exit { timeout },
            )) => {
                sv_impl.shutdown_children(timeout).await;
                return;
            }
            Select::Inner(sel) => {
                sv.poll(sel).await;
                continue;
            }
            Select::Shutdown => {
                sv_impl.shutdown_children(Duration::from_secs(1)).await;
                return;
            }
            Select::TaskExit(None) => continue,
            Select::TaskExit(Some(task_id)) => {
                let (ix, child, task_id) = match sv_impl
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
                    _ => continue,
                };

                let span = tracing::trace_span!("task-exit", task_id, index = ?ix);
                let _guard = span.enter();

                tracing::trace!("task exit");

                match strategy {
                    SupervisorStrategy::OneForOne => {
                        let resume_result = child.resume();
                        tracing::trace!(?resume_result, "resuming task");

                        match resume_result {
                            Ok(()) => {
                                tracing::trace!("registering group-ref to observe task");
                                futs.push(
                                    child
                                        .to_group_ref()
                                        .wait_until_suspended()
                                        .map(map_to_task_id(task_id)),
                                );
                            }
                            Err(()) => {
                                tracing::trace!("child has died abnormally. marking as deleted.");
                                std::mem::drop(child);
                                std::mem::take(&mut sv_impl.children[ix].slot);
                                continue;
                            }
                        }

                        continue;
                    }
                    SupervisorStrategy::OneForAll => {
                        futs.clear();

                        for child in sv_impl.children.iter_mut().rev() {
                            let filter = if let ChildState::Active(child) = &child.slot {
                                child.suspend().is_err()
                            } else {
                                false
                            };

                            if filter {
                                child.slot = ChildState::Deleted;
                            }
                        }

                        for child in sv_impl.children.iter_mut() {
                            let filter = if let ChildState::Active(child) = &child.slot {
                                child.resume().is_err()
                            } else {
                                false
                            };

                            if filter {
                                child.slot = ChildState::Deleted;
                            }
                        }

                        for child in sv_impl.children.iter() {
                            if let ChildState::Active(child) = &child.slot {
                                futs.push(
                                    child
                                        .to_group_ref()
                                        .wait_until_suspended()
                                        .map(map_to_task_id(task_id)),
                                );
                            }
                        }

                        continue;
                    }
                    SupervisorStrategy::RestForAll => {
                        futs.clear();

                        let (lhs, rhs) = sv_impl.children.split_at_mut(ix);

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
                                futs.push(
                                    child
                                        .to_group_ref()
                                        .wait_until_suspended()
                                        .map(map_to_task_id(task_id)),
                                );
                            }
                        }
                        continue;
                    }
                }
            }
            Select::Message(m) => m,
        };

        match m {
            SupervisorMessage::TerminateChild(child_ref, tx) => {
                let span = tracing::trace_span!("terminate-child", task_id = child_ref.0);
                let _guard = span.enter();

                let _ = match sv_impl
                    .children
                    .iter()
                    .position(|child| child.task_id == child_ref.0)
                {
                    None => tx.send(Err(())),
                    Some(ix) => {
                        tracing::trace!("terminating child by reference");

                        let child = sv_impl.children.get_mut(ix).unwrap();

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

                futs.push(
                    group
                        .to_group_ref()
                        .wait_until_suspended()
                        .map(map_to_task_id(task_id)),
                );

                sv_impl.children.push(SupervisedChild {
                    task_id,
                    slot: ChildState::Active(group),
                });

                // we dont care if the other side died
                // since we're now managing the child.
                let _ = tx.send(ChildRef(task_id));
            }
        }
    }
}

#[derive(Debug)]
pub struct Supervisor {
    sv_tx: MailboxSender<SupervisorMessage>,
    sv_group: Group,
}

impl Supervisor {
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

    pub async fn terminate_child(&self, child_ref: ChildRef) -> Result<(), ()> {
        let (tx, rx) = oneshot::channel();
        self.sv_tx
            .send(SupervisorMessage::TerminateChild(child_ref, tx))
            .await;

        rx.await.expect("could not add supervisor child")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorStrategy {
    OneForOne,
    OneForAll,
    RestForAll,
}

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
