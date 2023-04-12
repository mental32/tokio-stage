use std::future::Future;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use futures::TryFutureExt;

use tokio::sync::{mpsc, watch};

use crate::simple_supervisor::{
    SimpleSupervisorImpl, SimpleSupervisorMessage, SupervisorSignalTx, SupervisorStat,
    SupvervisorState,
};
use crate::task::TaskKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    Always,
    OnPanic,
}

impl RestartPolicy {
    pub(crate) fn should_restart(&self, res: &Result<((), u64), tokio::task::JoinError>) -> bool {
        match (self, res) {
            (RestartPolicy::Always, Ok(_)) => true,
            (RestartPolicy::OnPanic, Ok(_)) => false,
            (RestartPolicy::Always | RestartPolicy::OnPanic, Err(err)) => err.is_panic(),
        }
    }
}

pub(crate) struct SupervisorConfig {
    pub restart_policy: RestartPolicy,
    pub interval_dur: Duration,
    pub spawn_at_least: usize,
    pub task_kind: TaskKind,
    pub automatic_shutdown: bool,
    pub shutdown_timeout: Duration,
}

/// configure group parameters before spawning the supervisor task.
pub struct GroupBuilder(pub(super) SupervisorConfig);

pub(crate) type MakePendingFut = fn() -> std::future::Pending<()>;

impl GroupBuilder {
    /// Specify the restart policy for this group supervisor
    pub fn restart_policy(mut self, policy: RestartPolicy) -> Self {
        self.0.restart_policy = policy;
        self
    }

    /// Specify how long to wait for a worker task to respond to a shutdown signal before aborting it
    pub fn shutdown_timeout(mut self, dur: Duration) -> Self {
        self.0.shutdown_timeout = dur;
        self
    }

    /// a
    pub fn spawn_interval(mut self, dur: Duration) -> Self {
        self.0.interval_dur = dur;
        self
    }

    /// The number of minimum worker tasks to spawn
    pub fn spawn_at_least(mut self, n: usize) -> Self {
        self.0.spawn_at_least = n;
        self
    }

    /// The type (kind) of this task, worker or supervisor.
    pub(crate) fn task_kind(mut self, kind: TaskKind) -> Self {
        self.0.task_kind = kind;
        self
    }

    pub(crate) fn spawn_custom<F, T>(self, f: F) -> T
    where
        F: FnOnce(
            SimpleSupervisorImpl<MakePendingFut>,
            (
                mpsc::Sender<SimpleSupervisorMessage>,
                watch::Receiver<SupervisorStat>,
                watch::Sender<SupvervisorState>,
            ),
        ) -> T,
    {
        let Self(config) = self;
        let (sv_tx, chan_rx) = mpsc::channel(1);
        let (stat, sv_stat) = watch::channel(SupervisorStat::new());
        let (sv_suspend, suspend_signal) = watch::channel(SupvervisorState::Active);

        let sv = SimpleSupervisorImpl::new(
            stat,
            { || std::future::pending() } as _,
            config,
            chan_rx,
            suspend_signal,
            Default::default(),
            Default::default(),
        );

        (f)(sv, (sv_tx, sv_stat, sv_suspend))
    }

    /// Spawn the group supervisor for workers using the provided future constructor
    pub fn spawn<F, Fut>(self, f: F) -> Group
    where
        F: Send + Clone + Fn() -> Fut + 'static,
        Fut: Send + Future<Output = ()> + 'static,
    {
        let Self(config) = self;
        let (sv_tx, rx) = mpsc::channel(1);
        let (stat, sv_stat) = watch::channel(SupervisorStat::new());

        let (sv_suspend, suspend_signal) = watch::channel(SupvervisorState::Active);

        let sv = SimpleSupervisorImpl::new(
            stat,
            f,
            config,
            rx,
            suspend_signal,
            Default::default(),
            Default::default(),
        );

        let sv_handle = crate::task::spawn(
            async move { std::future::IntoFuture::into_future(sv).await },
            crate::task::TaskKind::Super,
        );

        let inner = Inner {
            sv_tx,
            sv_stat,
            sv_handle,
            sv_suspend,
        };

        Group { inner }
    }
}

#[derive(Debug)]
pub(crate) struct Inner {
    sv_tx: mpsc::Sender<SimpleSupervisorMessage>,
    sv_stat: watch::Receiver<SupervisorStat>,
    sv_handle: crate::task::Pid<()>,
    sv_suspend: SupervisorSignalTx,
}

impl Inner {
    pub(crate) fn new(
        sv_tx: mpsc::Sender<SimpleSupervisorMessage>,
        sv_stat: watch::Receiver<SupervisorStat>,
        sv_handle: crate::task::Pid<()>,
        sv_suspend: SupervisorSignalTx,
    ) -> Self {
        Self {
            sv_tx,
            sv_stat,
            sv_handle,
            sv_suspend,
        }
    }
}

impl Into<crate::task::Pid<()>> for Inner {
    fn into(self) -> crate::task::Pid<()> {
        self.sv_handle
    }
}

/// A unique, owned, handle used to control the underlying syupervisor task.
#[derive(Debug)]
pub struct Group {
    pub(crate) inner: Inner,
}

impl Group {
    fn sv_send(
        &self,
        message: SimpleSupervisorMessage,
    ) -> impl Future<Output = Result<(), mpsc::error::SendError<()>>> + '_ {
        self.inner
            .sv_tx
            .send(message)
            .map_err(|_| mpsc::error::SendError(())) // erase the error data since SupervisorMessage is a private type.
    }

    pub(crate) fn suspend(&self) -> Result<(), ()> {
        self.inner
            .sv_suspend
            .send(SupvervisorState::Suspend)
            .map_err(|_| ())
    }

    pub(crate) fn resume(&self) -> Result<(), ()> {
        self.inner
            .sv_suspend
            .send(SupvervisorState::Active)
            .map_err(|_| ())
    }

    pub(crate) fn task_id(&self) -> crate::task::TaskId {
        self.inner.sv_handle.task_id
    }
}

impl Group {
    /// View information and statistics about the supervisor of this group
    ///
    /// Use `stat_borrow_and_update` to access the stat structure and update
    /// if a newer value was sent.
    pub fn stat_borrow(&self) -> impl Deref<Target = SupervisorStat> + '_ {
        self.inner.sv_stat.borrow()
    }

    /// View information and statistics about the supervisor of this group
    ///
    /// This method will update the stat structure to the most recently sent
    /// value sent by the supervisor task.
    pub fn stat_borrow_and_update(&mut self) -> impl Deref<Target = SupervisorStat> + '_ {
        self.inner.sv_stat.borrow_and_update()
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
        self.sv_send(SimpleSupervisorMessage::Upscale { n })
    }

    /// Abort the group shutting down the supervisor.
    ///
    /// This will not shut down any tasks that have been spawned and are currently running.
    #[inline]
    pub fn abort_unchecked(self) {
        self.inner.sv_handle.abort();
    }

    /// a read-only idempotent handle that allows observing the task
    #[inline]
    pub fn to_group_ref(&self) -> GroupRef {
        GroupRef {
            sv_task_id: self.inner.sv_handle.task_id,
            sv_stat: self.inner.sv_stat.clone(),
        }
    }

    /// Signal the supervisor task to shut down and stop respawning tasks.
    pub async fn exit(&self, timeout: Duration) -> Result<(), mpsc::error::SendError<()>> {
        tracing::trace!(timeout = ?timeout, "triggering simple-supervisor exit");
        let () = self
            .sv_send(SimpleSupervisorMessage::Exit { timeout })
            .await?;

        Ok(())
    }

    /// Scope the lifetime of the supervisor to the future provided.
    ///
    /// When the future finishes the task group will be shutdown permanently.
    /// See the documentation for [`Self::shutdown`] for more details.
    pub async fn scope<T, Fut>(self, future: Fut) -> T
    where
        Fut: Future<Output = T> + Send,
        Fut::Output: Send,
    {
        let t = future.await;
        tracing::trace!("scope exit");
        self.exit(Duration::from_secs(1))
            .await
            .expect("shutdown failure");
        self.inner.sv_tx.closed().await;
        t
    }
}

#[derive(Debug, Clone)]
pub struct GroupRef {
    sv_task_id: crate::task::TaskId,
    sv_stat: watch::Receiver<SupervisorStat>,
}

impl PartialEq for GroupRef {
    fn eq(&self, other: &Self) -> bool {
        self.sv_task_id == other.sv_task_id && self.sv_stat.same_channel(&other.sv_stat)
    }
}

impl GroupRef {
    pub(crate) fn wait_until(
        &self,
        state: SupvervisorState,
    ) -> impl Future<Output = crate::task::TaskId> + 'static {
        let mut sv_stat = self.sv_stat.clone();
        let sv_task_id = self.sv_task_id;

        async move {
            loop {
                if sv_stat.borrow().state == state {
                    return sv_task_id;
                }

                if sv_stat.changed().await.is_err() {
                    return sv_task_id;
                }
            }
        }
    }

    pub(crate) fn wait_until_suspended(
        &self,
    ) -> impl Future<Output = crate::task::TaskId> + 'static {
        self.wait_until(SupvervisorState::Suspend)
    }

    pub async fn wait_until_exited(&self) {
        let mut sv_stat = self.sv_stat.clone();
        loop {
            if sv_stat.changed().await.is_err() {
                return;
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

    use crate::group::RestartPolicy;

    #[tokio::test]
    async fn test_group_upscale() {
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
    async fn test_group_spawn_at_least_n() {
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
    async fn test_group_basic_usage() {
        let ctr = Arc::new(AtomicBool::new(false));
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let rx = Arc::new(Mutex::new(rx));

        let group = crate::group().restart_policy(RestartPolicy::Always).spawn({
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
                        return;
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
