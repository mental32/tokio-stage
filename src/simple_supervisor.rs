use std::future::{Future, IntoFuture};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};

use tokio::sync::{mpsc, watch, Notify};
use tracing::Instrument;

use crate::graceful_shutdown::SHUTDOWN_NOTIFY;
use crate::group::SupervisorConfig;

pub(crate) async fn timeout_pids<T: std::fmt::Debug>(
    timeout: Duration,
    futs: &mut FuturesUnordered<crate::task::Pid<T>>,
) {
    let time_til_abort = tokio::time::sleep_until(
        tokio::time::Instant::now()
            .checked_add(timeout)
            .expect("time error"),
    );

    tokio::pin!(time_til_abort);

    loop {
        tokio::select! {
            res = futs.next() => {
                match res {
                    None => break,
                    Some(res) => tracing::trace!(?res, "task exit"),
                }
            }, // drain the running set of the futures as they exit
            () = &mut time_til_abort => {
                tracing::trace!("shutdown timeout, aborting remaining tasks");
                for pid in futs.iter() {
                    tracing::trace!("forceably aborting task");
                    pid.abort();
                }

                futs.clear();
                break;
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SupervisorStat {
    pub(crate) state: SupvervisorState,
}
impl SupervisorStat {
    pub(crate) fn new() -> SupervisorStat {
        Self {
            state: SupvervisorState::Active,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SupvervisorState {
    Suspend,
    Active,
}

pub(crate) type SupervisorSignal = watch::Receiver<SupvervisorState>;
pub(crate) type SupervisorSignalTx = watch::Sender<SupvervisorState>;

#[derive(Debug)]
pub(crate) enum SimpleSupervisorMessage {
    Upscale { n: usize },
    Exit { timeout: Duration },
}

pub(crate) struct SimpleSupervisorImpl<F> {
    pub stat: watch::Sender<SupervisorStat>,
    pub make_fut: F,
    pub config: SupervisorConfig,
    pub chan_rx: mpsc::Receiver<SimpleSupervisorMessage>,
    pub suspend_signal: SupervisorSignal,
    shutdown_notify: Arc<Notify>,
    running: FuturesUnordered<crate::task::Pid<()>>,
}

#[derive(Debug)]
pub(crate) enum Select {
    Future(Option<Result<((), crate::task::TaskId), tokio::task::JoinError>>),
    Message(SimpleSupervisorMessage),
    Suspend,
    Nop,
}

impl<F> SimpleSupervisorImpl<F> {
    pub(crate) fn new(
        stat: watch::Sender<SupervisorStat>,
        make_fut: F,
        config: SupervisorConfig,
        chan_rx: mpsc::Receiver<SimpleSupervisorMessage>,
        suspend_signal: SupervisorSignal,
        shutdown_notify: Arc<Notify>,
        running: FuturesUnordered<crate::task::Pid<()>>,
    ) -> Self {
        Self {
            stat,
            make_fut,
            config,
            chan_rx,
            suspend_signal,
            shutdown_notify,
            running,
        }
    }

    async fn shutdown_children(&mut self, timeout: Duration) {
        self.shutdown_notify.notify_waiters();
        timeout_pids(timeout, &mut self.running).await
    }

    pub(crate) async fn select(&mut self) -> Select {
        let suspend_signal = &mut self.suspend_signal;

        tokio::select! {
            _ = suspend_signal.changed() => {
                if matches!(*suspend_signal.borrow(), SupvervisorState::Suspend) {
                    Select::Suspend
                } else {
                    return Select::Nop;
                }
            }
            res = self.running.next() => {
                Select::Future(res)
            },
            Some(message) = self.chan_rx.recv() => {
                Select::Message(message)
            },
        }
    }
}

impl<F, Fut> SimpleSupervisorImpl<F>
where
    F: 'static + Clone + Send + Fn() -> Fut,
    Fut: 'static + Send + Future<Output = ()>,
    Fut::Output: 'static + Send,
{
    async fn suspend_until_resume<ResumeFut>(&mut self, resume_signal: ResumeFut)
    where
        ResumeFut: Future,
    {
        self.shutdown_notify.notify_waiters();

        self.shutdown_children(Duration::from_secs(1)).await;

        // wait for the shutdown signal to be notified again, this time it means "resume"
        resume_signal.await;
    }

    fn upscale_to_minimum(&mut self) {
        for _ in 0..self.config.spawn_at_least {
            let pid = self.spawn((self.make_fut)());
            self.running.push(pid);
        }
    }

    fn try_upscale(&mut self, n: usize) {
        if self.running.len() <= n {
            let diff = n - self.running.len();

            for _ in 0..diff {
                let pid = self.spawn((self.make_fut)());
                self.running.push(pid);
            }
        }
    }

    fn spawn(&self, fut: Fut) -> crate::task::Pid<Fut::Output> {
        let pid = crate::task::spawn(
            SHUTDOWN_NOTIFY.scope(self.shutdown_notify.clone(), fut),
            self.config.task_kind,
        );

        tracing::trace!(task_id = ?pid.task_id, "spawning worker task");

        pid
    }

    fn handle_task_exit(&mut self, res: Result<((), crate::task::TaskId), tokio::task::JoinError>) {
        let task_id = res.as_ref().ok().map(|ab| ab.1);
        let span = tracing::trace_span!("task-exit", task_id = task_id);
        let _guard = span.enter();

        tracing::trace!(result = ?res, is_err = res.is_err(), "task has exited");

        if self.config.restart_policy.should_restart(&res) {
            tracing::trace!("restart policy task restart");
            let pid = self.spawn((self.make_fut)());
            self.running.push(pid);
        } else {
            tracing::trace!("task will not be restarted");
            if self.config.automatic_shutdown && self.running.is_empty() {
                return;
            }
        }
    }

    async fn suspend(&mut self) {
        tracing::trace!("supervisor shutdown notification");
        let mut suspend_signal = self.suspend_signal.clone();
        let _ = self.stat.send(SupervisorStat {
            state: SupvervisorState::Suspend,
        });
        self.suspend_until_resume(
            async move {
                loop {
                    tracing::trace!(status = ?suspend_signal.borrow(), "resume checking signal status");
                    if matches!(*suspend_signal.borrow(), SupvervisorState::Active)
                    {
                        break;
                    }
                    if suspend_signal.changed().await.is_err() {
                        break;
                    }
                }
            }
            .instrument(tracing::trace_span!("resume")),
        )
        .await;
        let _ = self.stat.send(SupervisorStat {
            state: SupvervisorState::Active,
        });
    }

    pub(crate) async fn poll(&mut self, sel: Select) {
        match sel {
            Select::Nop => return,
            Select::Suspend => {
                self.suspend().await;
            }
            Select::Future(None) => return,
            Select::Future(Some(res)) => self.handle_task_exit(res),
            Select::Message(SimpleSupervisorMessage::Exit { timeout }) => {
                tracing::trace!(?timeout, "processing exit message");
                self.shutdown_children(timeout).await;
            }
            Select::Message(SimpleSupervisorMessage::Upscale { n }) => self.try_upscale(n),
        }
    }
}

impl<F, Fut> IntoFuture for SimpleSupervisorImpl<F>
where
    F: 'static + Clone + Send + Fn() -> Fut,
    Fut: 'static + Send + Future<Output = ()>,
    Fut::Output: 'static + Send,
{
    type Output = ();

    type IntoFuture = BoxFuture<'static, Self::Output>;

    fn into_future(mut self) -> Self::IntoFuture {
        let current_task_id = crate::task::try_current_task_id();
        let span = tracing::trace_span!("simple-supervisor", current_task_id);

        async move {
            let spawn_at_least = self.config.spawn_at_least;
            tracing::trace!(target = ?spawn_at_least, "spawning inital minimum tasks");

            self.upscale_to_minimum();

            loop {
                let sel = self.select().await;
                tracing::trace!(?sel, "simple-supervisor event loop select");
                self.poll(sel).await;
            }
        }
        .instrument(span)
        .boxed()
    }
}
