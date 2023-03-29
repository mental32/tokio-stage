use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::FuturesUnordered;
use futures::StreamExt;

use tokio::sync::{mpsc, watch, Notify};
use tracing::Instrument;

use crate::graceful_shutdown::SHUTDOWN_NOTIFY;
use crate::group::GroupConfig;

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

pub(crate) struct SimpleSupervisor<F> {
    pub stat: watch::Sender<SupervisorStat>,
    pub chan_rx: mpsc::Receiver<SimpleSupervisorMessage>,
    pub make_fut: F,
    pub config: GroupConfig,
    pub signal: SupervisorSignal,
}

struct SimpleSupervisorImpl<F> {
    pub stat: watch::Sender<SupervisorStat>,
    pub make_fut: F,
    pub config: GroupConfig,
    shutdown_notify: Arc<Notify>,
    running: FuturesUnordered<crate::task::Pid<()>>,
}

impl<F, Fut> SimpleSupervisorImpl<F>
where
    F: 'static + Clone + Send + Fn() -> Fut,
    Fut: 'static + Send + Future<Output = ()>,
    Fut::Output: 'static + Send,
{
    async fn shutdown_children(&mut self, timeout: Duration) {
        self.shutdown_notify.notify_waiters();
        timeout_pids(timeout, &mut self.running).await
    }

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

    fn spawn(&self, fut: Fut) -> crate::task::Pid<Fut::Output> {
        let pid = crate::task::spawn(
            SHUTDOWN_NOTIFY.scope(self.shutdown_notify.clone(), fut),
            self.config.task_kind,
        );

        tracing::trace!(task_id = ?pid.task_id, "spawning worker task");

        pid
    }
}

impl<F, Fut> SimpleSupervisor<F>
where
    F: 'static + Clone + Send + Fn() -> Fut,
    Fut: 'static + Send + Future<Output = ()>,
    Fut::Output: 'static + Send,
{
    #[track_caller]
    pub fn into_future(self) -> impl Future<Output = ()> + 'static + Send {
        let current_task_id: crate::task::TaskId = crate::task::current_task_id();
        let span = tracing::trace_span!("simple-supervisor", current_task_id);
        let _guard = span.enter();

        let spawn_at_least = self.config.spawn_at_least;
        tracing::trace!(target = ?spawn_at_least, "spawning inital minimum tasks");

        let Self {
            stat,
            chan_rx,
            make_fut,
            config,
            signal: mut suspend_signal,
        } = self;

        let mut rx = chan_rx;
        let mut this = SimpleSupervisorImpl {
            shutdown_notify: Arc::new(Notify::new()),
            running: FuturesUnordered::new(),
            stat,
            make_fut,
            config,
        };

        this.upscale_to_minimum();

        #[derive(Debug)]
        enum Select {
            Future(Option<Result<((), crate::task::TaskId), tokio::task::JoinError>>),
            Message(SimpleSupervisorMessage),
            Suspend,
        }

        async move {
            let mut this = this;

            loop {
                let sel = tokio::select! {
                    _ = suspend_signal.changed() => {
                        if matches!(*suspend_signal.borrow(), SupvervisorState::Suspend) {
                            Select::Suspend
                        } else {
                            continue;
                        }
                    }
                    res = this.running.next() => {
                        Select::Future(res)
                    },
                    Some(message) = rx.recv() => {
                        Select::Message(message)
                    },
                };

                tracing::trace!(?sel, "simple-supervisor event loop select");

                match sel {
                    Select::Suspend => {
                        tracing::trace!("supervisor shutdown notification");
                        let mut suspend_signal = suspend_signal.clone();
                        let _ = this.stat.send(SupervisorStat { state: SupvervisorState::Suspend });
                        this.suspend_until_resume(
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
                        let _ = this.stat.send(SupervisorStat { state: SupvervisorState::Active });
                        continue;
                    }
                    Select::Future(None) => continue,
                    Select::Future(Some(res)) => {
                        let task_id = res.as_ref().ok().map(|ab| ab.1);
                        let span = tracing::trace_span!("task-exit", task_id = task_id);
                        let _guard = span.enter();

                        tracing::trace!(result = ?res, is_err = res.is_err(), "task has exited");

                        if this.config.restart_policy.should_restart(&res) {
                            tracing::trace!("restart policy task restart");
                            let pid = this.spawn((this.make_fut)());
                            this.running.push(pid);
                        } else {
                            tracing::trace!("task will not be restarted");
                            if this.config.automatic_shutdown && this.running.is_empty() {
                                return;
                            }
                        }
                    }
                    Select::Message(SimpleSupervisorMessage::Exit { timeout }) => {
                        tracing::trace!(?timeout, "processing exit message");
                        this.shutdown_children(timeout).await;
                        return;
                    }
                    Select::Message(SimpleSupervisorMessage::Upscale { n }) => {
                        if this.running.len() <= n {
                            let diff = n - this.running.len();

                            for _ in 0..diff {
                                let pid = this.spawn((this.make_fut)());
                                this.running.push(pid);
                            }
                        }
                    }
                }
            }
        }.instrument(span.clone())
    }
}
