use tower::ServiceExt;

use crate::task::TaskKind;
use crate::MailboxSender;

struct Actor<T, S> {
    state: T,
    service: S,
}

pub struct Context<'a, T, U> {
    state: &'a mut T,
    inner: U,
}

pub trait IntoContext<'a, T> {
    fn into_context(self, t: &'a mut T) -> Context<'a, T, Self>
    where
        Self: Sized;
}

impl<'a, T, U> IntoContext<'a, T> for U {
    fn into_context(self, t: &'a mut T) -> Context<'a, T, Self>
    where
        Self: Sized,
    {
        Context {
            state: t,
            inner: self,
        }
    }
}

pub struct Address<U> {
    pid: crate::task::Pid<()>,
    tx: MailboxSender<U>,
}

impl<U> Address<U> {
    pub async fn send(&self, u: U) {
        self.tx.send(u).await
    }
}

pub trait SendService<T>: tower::Service<T, Future = Self::SendFuture> {
    type SendFuture: Send;
}

impl<S: tower::Service<T>, T> SendService<T> for S
where
    S::Future: Send,
{
    type SendFuture = S::Future;
}

#[track_caller]
pub fn actor<T, U, S>(state: T, service: S) -> Address<U>
where
    S: Send + 'static + for<'a> SendService<Context<'a, T, U>>,
    U: Send + 'static + for<'a> IntoContext<'a, T>,
    T: Send + 'static,
{
    let (tx, rx) = crate::mailbox(1);

    async fn actor_impl<T, U, S>(state: T, rx: crate::MailboxReceiver<U>, mut svc: S)
    where
        T: 'static,
        S: for<'a> tower::Service<Context<'a, T, U>>,
    {
        let mut t = state;

        loop {
            let u = rx.recv().await;

            let cx = u.into_context(&mut t);

            let Ok(svc) = svc.ready().await else { unreachable!()};

            let _ = svc.call(cx).await;
        }
    }

    let pid = crate::task::spawn(actor_impl(state, rx, service), TaskKind::Worker);

    Address { pid, tx }
}

#[cfg(test)]
mod test {
    async fn handle<'a>(r: super::Context<'a, (), ()>) -> Result<(), ()> {
        Ok(())
    }

    #[tokio::test]
    async fn test() {
        let mut svc = tower::service_fn(handle);

        // let act = super::actor((), svc);
    }
}
