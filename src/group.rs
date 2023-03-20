use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::FuturesUnordered;
use futures::StreamExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    Always,
    UnlessAborted,
}

struct Config {
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

        let sv_handle = tokio::spawn(async move {
            let sv = Supervisor {
                make_fut: f,
                config,
            };

            sv.into_future().await;
        });

        Group {
            sv_handle: Arc::new(sv_handle),
        }
    }
}

pub struct Group {
    sv_handle: Arc<tokio::task::JoinHandle<()>>,
}

impl Group {
    /// Abort the group shutting down the supervisor.
    ///
    /// This will not shut down any tasks that have been spawned and are currently running.
    #[inline]
    pub fn abort(self) {
        self.sv_handle.abort();
    }

    /// Scope the lifetime of the supervisor to the future provided.
    ///
    /// When the future finishes the task group will be aborted.
    /// See the documentation for [`abort`] for more details.
    pub async fn scope<T, Fut>(self, future: Fut) -> T
    where
        Fut: Future<Output = T> + Send + 'static,
        Fut::Output: Send + 'static,
    {
        let t = future.await;
        self.abort();
        t
    }
}

struct Supervisor<F> {
    make_fut: F,
    config: Config,
}

impl<F, Fut> Supervisor<F>
where
    F: 'static + Clone + Send + Fn() -> Fut,
    Fut: 'static + Send + Future<Output = ()>,
    Fut::Output: 'static + Send,
{
    fn into_future(self) -> impl Future<Output = ()> + 'static + Send {
        tracing::trace!(n = ?self.config.min_spawn, "spawning inital minimum tasks");
        let mut futs: FuturesUnordered<_> = (0..self.config.min_spawn)
            .map(|_| tokio::spawn((self.make_fut)()))
            .collect();

        let mut interval = tokio::time::interval(self.config.interval_dur);

        async move {
            while let Some(task_result) = futs.next().await {
                tracing::debug!(?task_result, "task exited");

                if self.config.restart_policy == RestartPolicy::UnlessAborted
                    && matches!(&task_result, Err(err) if err.is_cancelled())
                {
                    continue;
                }

                tracing::trace!("task failure, restarting with replica.");

                futs.push(tokio::spawn((self.make_fut)()));
                interval.tick().await;
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::Mutex;

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
                        tokio::time::sleep(Duration::from_millis(delay + 1)).await;
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

        expect_min_spawn_n(5, 100).await;
        expect_min_spawn_n(10, 100).await;
        expect_min_spawn_n(100, 100).await;
        expect_min_spawn_n(1000, 100).await;
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
