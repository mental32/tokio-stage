use std::future::Future;

use futures::stream::FuturesUnordered;
use futures::StreamExt;

use tokio::sync::{mpsc, oneshot, watch};
use tracing::Instrument;

use crate::group::Config;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SupervisorStat {
    ticks: usize,
    running: usize,
    failed: usize,
}

pub(crate) enum SupervisorMessage {
    Upscale {
        n: usize,
    },
    Shutdown {
        tx: oneshot::Sender<Vec<tokio::task::JoinHandle<()>>>,
    },
}

pub(crate) struct Supervisor<F> {
    pub stat: watch::Sender<SupervisorStat>,
    pub chan_rx: mpsc::Receiver<SupervisorMessage>,
    pub make_fut: F,
    pub config: Config,
}

impl<F, Fut> Supervisor<F>
where
    F: 'static + Clone + Send + Fn() -> Fut,
    Fut: 'static + Send + Future<Output = ()>,
    Fut::Output: 'static + Send,
{
    pub fn into_future(self) -> impl Future<Output = ()> + 'static + Send {
        tracing::trace!(n = ?self.config.min_spawn, "spawning inital minimum tasks");

        let span = self.config.span;
        let spawn = move |future: Fut| {
            if let Some(ref span) = span {
                tokio::spawn(future.instrument(span.clone()))
            } else {
                tokio::spawn(future)
            }
        };

        let mut futs: FuturesUnordered<_> = (0..self.config.min_spawn)
            .map(|_| {
                let future = (self.make_fut)();
                spawn(future)
            })
            .collect();

        let mut interval = tokio::time::interval(self.config.interval_dur);
        let mut rx = self.chan_rx;

        async move {
            loop {
                tokio::select! {
                    task_result = futs.next() => {
                        match task_result {
                            Some(task_result) => {
                                tracing::debug!(?task_result, "task exited");

                                if self.config.restart_policy == crate::group::RestartPolicy::UnlessAborted
                                    && matches!(&task_result, Err(err) if err.is_cancelled())
                                {
                                    continue;
                                }

                                tracing::trace!("task failure, restarting with replica.");

                                futs.push(spawn((self.make_fut)()));
                                interval.tick().await;

                                self.stat.send_modify(|stat| {
                                    stat.running = futs.len();
                                    stat.failed += 1;
                                });
                            },
                            None => break,
                        }
                    },
                    Some(message) = rx.recv() => {
                        match message {
                            SupervisorMessage::Shutdown { tx} => {
                                tx.send(Vec::from_iter(futs.into_iter())).expect("shutdown failure");
                                break;
                            }
                            SupervisorMessage::Upscale { n }=> {
                                if futs.len() <= n {
                                    let diff = n - futs.len();

                                    futs.extend((0..diff).map(|_| {
                                        let future = (self.make_fut)();
                                        spawn(future)
                                    }));

                                    self.stat.send_if_modified(|stat| {
                                        let prev = stat.running;
                                        stat.running = futs.len();
                                        prev != futs.len()
                                    });
                                }
                            }
                        }
                    },
                };

                self.stat.send_if_modified(|stat| {
                    stat.ticks = stat.ticks.saturating_add(1);
                    false
                });
            }
        }
    }
}
