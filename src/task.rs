use std::future::Future;
use std::ops::{BitAnd, Deref};
use std::sync::atomic::{AtomicU64, Ordering};
use std::task;

use tokio::task::JoinHandle;
use tokio::task_local;

static MONO_TASK_ID: AtomicU64 = AtomicU64::new(2);

pub type TaskId = u64;

task_local! {
    static CURRENT_TASK_ID: TaskId;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Worker,
    Super,
}

impl From<TaskId> for TaskKind {
    #[inline]
    fn from(value: TaskId) -> Self {
        if value.bitand(1) == 1 {
            Self::Super
        } else {
            Self::Worker
        }
    }
}

#[derive(Debug)]
#[pin_project::pin_project]
pub struct Pid<T> {
    #[pin]
    inner: JoinHandle<T>,
    pub(crate) task_id: TaskId,
}

impl<T> Deref for Pid<T> {
    type Target = JoinHandle<T>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> Future for Pid<T> {
    type Output = Result<(T, TaskId), tokio::task::JoinError>;

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
pub fn spawn<T>(future: T, kind: TaskKind) -> Pid<T::Output>
where
    T: Future + Send + 'static,
    T::Output: Send + 'static,
{
    let mut task_id = MONO_TASK_ID.fetch_add(2, Ordering::SeqCst);
    if let TaskKind::Super = kind {
        task_id -= 1;
    }

    let inner = tokio::task::spawn(CURRENT_TASK_ID.scope(task_id, future));

    Pid { inner, task_id }
}

#[track_caller]
pub(crate) fn current_task_id() -> u64 {
    CURRENT_TASK_ID.get()
}
