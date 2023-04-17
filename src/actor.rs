use std::future::Future;

use futures::FutureExt;
use tokio::sync::mpsc;
use tower::ServiceExt;

use crate::MailboxSender;

/// actor context parameter that is generated for the underlying service to handle
pub struct Context<'a, T, U> {
    state: &'a mut T,
    inner: U,
}

impl<'a, T, U> Context<'a, T, U> {
    /// access the internal state by a borrow
    #[inline]
    pub fn as_ref(&self) -> &T {
        &self.state
    }

    /// access the internal state by a mutable borrow
    #[inline]
    pub fn as_mut(&mut self) -> &mut T {
        self.state
    }

    /// destruct the current context into a tuple of the state and inner request
    #[inline]
    pub fn into_parts(self) -> (&'a mut T, U) {
        (self.state, self.inner)
    }
}

/// used to convert types into an instance of [`Context`]
pub trait IntoContext<'a, T> {
    /// cast self into a context
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

/// [`tower::Service`] with a [`Send`] requirement on the service future
pub trait SendService<T>: tower::Service<T, Future = Self::SendFuture> {
    /// [tower::Service::Future] but with a [Send] requirement
    type SendFuture: Send;
}

impl<S: tower::Service<T>, T> SendService<T> for S
where
    S::Future: Send,
{
    type SendFuture = S::Future;
}

/// wrapper type for the underlying actor future that stores a copy of the channel [`Self::sender`].
#[pin_project::pin_project]
pub struct Actor<Fut, U> {
    #[pin]
    fut: Fut,
    chan: mpsc::WeakSender<crate::mailbox::Message<U>>,
}

impl<Fut, U> Actor<Fut, U> {
    /// Acquire a sender to this actors mailbox.
    ///
    /// `None` will be returned if it has been closed.
    #[track_caller]
    pub fn sender(&self) -> Option<MailboxSender<U>> {
        self.chan.upgrade().map(MailboxSender::from_raw)
    }
}

impl<Fut, U> Future for Actor<Fut, U>
where
    Fut: Future<Output = U> + Send + 'static,
{
    type Output = Fut::Output;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.project().fut.poll_unpin(cx)
    }
}

/// Given some state and a service ([`tower::Service`]) construct an [`Actor`].
///
/// Actors in other frameworks are managed, spawning and lifecycle is handled by default.
/// however in stage actors must be explicitly constructed and inserted into supervision structures
/// like a [`crate::Supervisor`] or a [`crate::Group`].
///
/// # Example
///
/// ```
/// use tokio_stage::Context;
///
/// async fn handle<'a>(_r: Context<'a, (), ()>) -> Result<(), ()> {
///     Ok(())
/// }
///
/// #[tokio::main]
/// async fn main() {
///     let group = tokio_stage::group().spawn(|| {
///         let svc = tower::service_fn(handle);
///         tokio_stage::actor((), svc)
///     });
///     
///     group.scope(tokio::time::sleep(std::time::Duration::from_millis(10))).await;
/// }
/// ```
pub fn actor<T, U, S>(state: T, service: S) -> Actor<impl Future<Output = ()> + Send + 'static, U>
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

            let Ok(svc) = svc.ready().await else { break };

            let _ = svc.call(cx).await;
        }
    }

    #[cfg(feature = "backtrace")]
    let fut = async_backtrace::location!().frame(actor_impl(state, rx, service));

    #[cfg(not(feature = "backtrace"))]
    let fut = actor_impl(state, rx, service);

    Actor {
        fut,
        chan: tx.into_raw().downgrade(),
    }
}

#[cfg(test)]
mod test {
    async fn handle<'a>(_r: super::Context<'a, (), ()>) -> Result<(), ()> {
        Ok(())
    }

    #[tokio::test]
    async fn test() {
        let svc = tower::service_fn(handle);
        let act = super::actor((), svc);

        crate::task::spawn(act, crate::task::TaskKind::Worker);
    }
}
