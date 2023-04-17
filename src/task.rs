use std::future::Future;
use std::ops::{BitAnd, Deref};
use std::sync::atomic::{AtomicU64, Ordering};
use std::task;

use tokio::task::JoinHandle;
use tokio::task_local;

static MONO_TASK_ID: AtomicU64 = AtomicU64::new(2);

pub(crate) type TaskIdRepr = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct TaskId(TaskIdRepr);

task_local! {
    static CURRENT_TASK_ID: TaskIdRepr;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskKind {
    Worker,
    Super,
}

impl From<TaskIdRepr> for TaskKind {
    #[inline]
    fn from(value: TaskIdRepr) -> Self {
        if value.bitand(1) == 1 {
            Self::Super
        } else {
            Self::Worker
        }
    }
}

/// A [`Pid`] is a [`tokio::task::JoinHandle`] abstraction that associates a numerical id with the underlying task.
#[derive(Debug)]
#[pin_project::pin_project]
pub struct Pid<T> {
    #[pin]
    inner: JoinHandle<T>,
    pub(crate) task_id: TaskIdRepr,
}

impl<T> Pid<T> {
    /// The stage task id for this process.
    #[inline]
    pub fn task_id(&self) -> TaskId {
        TaskId(self.task_id)
    }
}

impl<T> Deref for Pid<T> {
    type Target = JoinHandle<T>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> Future for Pid<T> {
    type Output = Result<(T, TaskIdRepr), tokio::task::JoinError>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Self::Output> {
        let task_id = self.task_id;
        let t = futures::ready!(self.project().inner.poll(cx));
        task::Poll::Ready(t.map(|t| (t, task_id)))
    }
}

#[track_caller]
pub(crate) fn scope_with_task_id<T>(
    future: T,
    kind: TaskKind,
) -> (impl Future<Output = T::Output>, TaskIdRepr)
where
    T: Future + Send + 'static,
    T::Output: Send + 'static,
{
    let mut task_id = MONO_TASK_ID.fetch_add(2, Ordering::SeqCst);
    if let TaskKind::Super = kind {
        task_id -= 1;
    }

    let fut = CURRENT_TASK_ID.scope(task_id, future);

    (fut, task_id)
}

#[track_caller]
pub(crate) fn spawn<T>(future: T, kind: TaskKind) -> Pid<T::Output>
where
    T: Future + Send + 'static,
    T::Output: Send + 'static,
{
    let (future, task_id) = scope_with_task_id(future, kind);
    let inner = tokio::task::spawn(future);
    Pid { inner, task_id }
}

#[track_caller]
pub(crate) fn current_task_id() -> u64 {
    CURRENT_TASK_ID.get()
}

#[track_caller]
pub(crate) fn try_current_task_id() -> u64 {
    CURRENT_TASK_ID.try_with(|n| *n).unwrap_or(0)
}
