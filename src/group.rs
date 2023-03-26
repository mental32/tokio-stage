use std::future::Future;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use futures::{StreamExt, TryFutureExt};

use futures::stream::FuturesUnordered;
use tokio::sync::{mpsc, oneshot, watch};

use crate::supervisor::{Supervisor, SupervisorMessage, SupervisorStat};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    Always,
    UnlessAborted,
}

pub(crate) struct Config {
    pub restart_policy: RestartPolicy,
    pub interval_dur: Duration,
    pub min_spawn: usize,
}

pub struct GroupBuilder(Config);

impl Default for GroupBuilder {
    fn default() -> Self {
        Self(Config {
            min_spawn: 1,
            restart_policy: RestartPolicy::UnlessAborted,
            interval_dur: Duration::from_millis(10),
        })
    }
}

impl GroupBuilder {
    pub fn restart_policy(mut self, policy: RestartPolicy) -> Self {
        self.0.restart_policy = policy;
        self
    }

    pub fn spawn_interval(mut self, dur: Duration) -> Self {
        self.0.interval_dur = dur;
        self
    }

    pub fn spawn_at_least(mut self, n: usize) -> Self {
        self.0.min_spawn = n;
        self
    }

    pub fn spawn<F, Fut>(self, f: F) -> Group
    where
        F: Send + Clone + Fn() -> Fut + 'static,
        Fut: Send + Future<Output = ()> + 'static,
    {
        let Self(config) = self;
        let (sv_tx, rx) = mpsc::channel(1);
        let (stat, sv_stat) = watch::channel(SupervisorStat::default());

        let sv_handle = tokio::spawn(async move {
            let sv = Supervisor {
                stat,
                chan_rx: rx,
                make_fut: f,
                config,
            };

            sv.into_future().await;
        });

        Group {
            sv_tx,
            sv_stat,
            sv_handle: Arc::new(sv_handle),
        }
    }
}

#[derive(Clone)]
pub struct Group {
    sv_tx: mpsc::Sender<SupervisorMessage>,
    sv_stat: watch::Receiver<SupervisorStat>,
    sv_handle: Arc<tokio::task::JoinHandle<()>>,
}

impl Group {
    fn sv_send(
        &self,
        message: SupervisorMessage,
    ) -> impl Future<Output = Result<(), mpsc::error::SendError<()>>> + '_ {
        self.sv_tx
            .send(message)
            .map_err(|_| mpsc::error::SendError(())) // erase the error data since SupervisorMessage is a private type.
    }
}

impl Group {
    /// View information and statistics about the supervisor of this group
    ///
    /// Use `stat_borrow_and_update` to access the stat structure and update
    /// if a newer value was sent.
    pub fn stat_borrow(&self) -> impl Deref<Target = SupervisorStat> + '_ {
        self.sv_stat.borrow()
    }

    /// View information and statistics about the supervisor of this group
    ///
    /// This method will update the stat structure to the most recently sent
    /// value sent by the supervisor task.
    pub fn stat_borrow_and_update(&mut self) -> impl Deref<Target = SupervisorStat> + '_ {
        self.sv_stat.borrow_and_update()
    }

    /// dynamically increase the number of running futures managed by the supervisor.
    ///
    /// Downscaling safely is not possible since futures do not have a native async_drop or
    /// graceful shutdown trigger. If you want to downscale you must implement this capability
    /// in your future directly.
    pub fn upscale(
        &self,
        n: usize,
    ) -> impl Future<Output = Result<(), mpsc::error::SendError<()>>> + '_ {
        self.sv_send(SupervisorMessage::Upscale { n })
    }

    /// Abort the group shutting down the supervisor.
    ///
    /// This will not shut down any tasks that have been spawned and are currently running.
    #[inline]
    pub fn abort_unchecked(self) {
        self.sv_handle.abort();
    }

    /// Signal the supervisor task to shut down and stop respawning tasks.
    ///
    /// The supervisor will hand over the [`tokio::task::JoinHandle`]s of the
    /// tasks that are running. The caller is responsible for aborting, awaiting
    /// or gracefully shutting down the spawned tasks.
    pub fn shutdown(
        &self,
    ) -> impl Future<Output = Result<Vec<tokio::task::JoinHandle<()>>, mpsc::error::SendError<()>>>
    {
        let this = self.clone();
        let (tx, rx) = oneshot::channel();

        async move {
            let () = this.sv_send(SupervisorMessage::Shutdown { tx }).await?;

            let vec = rx
                .unwrap_or_else(|_| {
                    panic!("supervisor task dropped sender before submitting results")
                })
                .await;

            Ok(vec)
        }
    }

    /// Scope the lifetime of the supervisor to the future provided.
    ///
    /// When the future finishes the task group will be shutdown.
    /// See the documentation for [`Self::shutdown`] for more details.
    pub async fn scope<T, Fut>(self, future: Fut) -> T
    where
        Fut: Future<Output = T> + Send,
        Fut::Output: Send,
    {
        let t = future.await;
        let handles = self.shutdown().await.expect("shutdown failure");

        let abort_handles = handles
            .into_iter()
            .map(|mut handle| {
                let sleep = tokio::time::sleep(Duration::from_secs(1));

                async move {
                    tokio::select! {
                        _ = sleep => {
                            handle.abort();
                        },
                        _ = &mut handle => {
                            return;
                        }
                    };
                }
            })
            .collect::<FuturesUnordered<_>>();

        tokio::spawn(async move {
            let n = abort_handles.count().await;
            tracing::trace!(n_handles = ?n, "group shutdown complete")
        });

        t
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::Mutex;

    #[tokio::test]
    async fn test_pos_upscale() {
        let n = 10;
        let delay = 100;

        let ctr = Arc::new(AtomicUsize::new(0));
        let ctr2 = Arc::clone(&ctr);

        let group = crate::group().spawn_at_least(n).spawn({
            move || {
                let ctr = Arc::clone(&ctr2);

                async move {
                    ctr.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(delay + 1)).await;
                }
            }
        });

        tokio::time::sleep(Duration::from_millis(delay / 10)).await;
        assert_eq!(ctr.load(Ordering::SeqCst), n);

        group.upscale(n * 2).await.expect("failed to upscale");

        tokio::time::sleep(Duration::from_millis(delay / 10)).await;
        assert_eq!(ctr.load(Ordering::SeqCst), n * 2);
    }

    #[tokio::test]
    async fn test_pos_shutdown() {
        async fn expect_min_spawn_n(n: usize, delay: u64) {
            let ctr = Arc::new(AtomicUsize::new(0));
            let ctr2 = Arc::clone(&ctr);

            let group = crate::group().spawn_at_least(n).spawn({
                move || {
                    let ctr = Arc::clone(&ctr2);

                    async move {
                        ctr.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(delay + 1)).await;
                    }
                }
            });

            tokio::time::sleep(Duration::from_millis(delay / 10)).await;

            let fut = group.shutdown();
            let vec = tokio::time::timeout(Duration::from_millis(delay), fut)
                .await
                .expect("timeout")
                .expect("shutdown failure");

            assert_eq!(vec.len(), n);
            assert_eq!(ctr.load(Ordering::SeqCst), n);
        }

        expect_min_spawn_n(100, 10).await;
    }

    #[tokio::test]
    async fn test_pos_spawn_at_least_n() {
        async fn expect_min_spawn_n(n: usize, delay: u64) {
            let ctr = Arc::new(AtomicUsize::new(0));
            let ctr2 = Arc::clone(&ctr);

            let group = crate::group().spawn_at_least(n).spawn({
                move || {
                    let ctr = Arc::clone(&ctr2);

                    async move {
                        ctr.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(delay * 2)).await;
                    }
                }
            });

            group
                .scope(async move {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                })
                .await;

            assert_eq!(ctr.load(Ordering::SeqCst), n);
        }

        expect_min_spawn_n(5, 10).await;
        expect_min_spawn_n(10, 10).await;
        expect_min_spawn_n(100, 10).await;
        expect_min_spawn_n(1000, 10).await;
        expect_min_spawn_n(10000, 100).await;
        expect_min_spawn_n(100000, 300).await;
    }

    #[tokio::test]
    async fn test() {
        let ctr = Arc::new(AtomicBool::new(false));
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let rx = Arc::new(Mutex::new(rx));

        let group = crate::group().spawn({
            move || {
                let ctr = Arc::clone(&ctr);
                let rx = Arc::clone(&rx);
                async move {
                    let mut rx = rx.lock().await;
                    let tx: tokio::sync::oneshot::Sender<_> = rx.recv().await.unwrap();

                    if ctr.load(Ordering::SeqCst) {
                        tx.send(3).unwrap();
                    } else {
                        ctr.store(true, Ordering::SeqCst);
                        panic!("induced failure");
                    }
                }
            }
        });

        group
            .scope(async move {
                loop {
                    let (ttx, trx) = tokio::sync::oneshot::channel();
                    tx.send(ttx).await.unwrap();

                    if let Ok(res) = trx.await {
                        assert_eq!(res, 3);
                        break;
                    }
                }
            })
            .await;
    }
}
