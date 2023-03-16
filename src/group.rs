use std::future::Future;
use std::marker::PhantomData;
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
            interval_dur: Duration::from_millis(100),
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

    pub fn min_spawned(mut self, n: usize) -> Self {
        self.0.min_spawn = n;
        self
    }

    pub fn spawn<F, Fut>(self, f: F) -> Group<F>
    where
        F: Send + Clone + Fn() -> Fut + 'static,
        Fut: Send + Future<Output = ()> + 'static,
    {
        let Self(config) = self;
        let sv = Supervisor {
            make_fut: f,
            config,
        };

        let sv_handle = tokio::spawn(async move {
            sv.run_forever().await;
        });

        Group {
            _f: PhantomData,
            sv_handle: Arc::new(sv_handle),
        }
    }
}

pub struct Group<F> {
    _f: PhantomData<F>,
    sv_handle: Arc<tokio::task::JoinHandle<()>>,
}

impl<F> Group<F> {
    pub async fn scope<T, Fut>(&self, future: Fut) -> T
    where
        Fut: Future<Output = T> + Send + 'static,
        Fut::Output: Send + 'static,
    {
        let t = future.await;
        self.sv_handle.abort();
        t
    }
}

struct Supervisor<F> {
    make_fut: F,
    config: Config,
}

impl<F, Fut> Supervisor<F>
where
    F: Clone + Fn() -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
    Fut::Output: Send + 'static,
{
    async fn run_forever(self) {
        let mut futs: FuturesUnordered<_> = (0..self.config.min_spawn)
            .map(|_| tokio::spawn((self.make_fut)()))
            .collect();

        let mut interval = tokio::time::interval(self.config.interval_dur);

        while let Some(task_result) = futs.next().await {
            tracing::debug!(?task_result, "task exited");

            if self.config.restart_policy == RestartPolicy::UnlessAborted
                && matches!(&task_result, Err(err) if err.is_cancelled())
            {
                continue;
            }

            futs.push(tokio::spawn((self.make_fut)()));
            interval.tick().await;
        }
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use tokio::sync::Mutex;

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
