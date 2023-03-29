use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Notify;

tokio::task_local! {
    pub(crate) static SHUTDOWN_NOTIFY: Arc<tokio::sync::Notify>;
}

pub async fn shutdown_scope<T, Fut>(signal: Arc<Notify>, fut: Fut) -> T
where
    Fut: Future<Output = T>,
{
    SHUTDOWN_NOTIFY.scope(signal, fut).await
}

pub fn graceful_shutdown<F, Fut, ShutdownFut>(
    fut: Fut,
    f: F,
) -> impl Future<Output = Option<Fut::Output>>
where
    F: FnOnce(Pin<&mut Fut>) -> ShutdownFut,
    ShutdownFut: Future<Output = ()>,
    Fut: Future,
{
    async move {
        let notify = SHUTDOWN_NOTIFY.try_with(|n| Arc::clone(n));

        match notify {
            Ok(notify) => {
                let notify = notify.notified();

                loop {
                    tokio::pin!(fut);

                    tokio::select! {
                        res = &mut fut => { return Some(res) }
                        () = notify => {
                            f(fut).await;
                            return None;
                        }
                    };
                }
            }
            Err(_) => Some(fut.await),
        }
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::Notify;

    #[tokio::test]
    async fn test_pos_graceful_shutdown_timeout() {
        let n = Arc::new(Notify::new());

        tokio::spawn({
            let n = Arc::clone(&n);
            async move {
                tokio::time::sleep(Duration::from_nanos(1)).await;
                n.notify_waiters();
            }
        });

        let mut flag = false;
        let setter = &mut flag;

        super::SHUTDOWN_NOTIFY
            .scope(
                n,
                super::graceful_shutdown(std::future::pending::<()>(), |_fut| async {
                    *setter = true;
                }),
            )
            .await;

        assert!(flag);
    }
}
