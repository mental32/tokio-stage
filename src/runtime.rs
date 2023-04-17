use std::future::Future;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::FutureExt;

use crate::task::TaskKind;
use crate::Supervisor;

enum Task {
    Fut(BoxFuture<'static, ()>),
    Spawned(crate::task::Pid<()>),
}

impl Task {
    async fn abort(&mut self) {
        match self {
            Task::Fut(_) => (),
            Task::Spawned(pid) => pid.abort(),
        }
    }
}

struct Inner {
    root: Supervisor,
    tasks: Vec<Task>,
}

struct RuntimeGuard {
    inner: Inner,
}

impl RuntimeGuard {
    fn spawn_task_futures(&mut self) {
        let tasks = std::mem::take(&mut self.inner.tasks);
        self.inner.tasks = tasks
            .into_iter()
            .map(|task| match task {
                Task::Fut(fut) => Task::Spawned(crate::task::spawn(fut, TaskKind::Worker)),
                Task::Spawned(_) => task,
            })
            .collect();
    }

    #[cfg_attr(feature = "backtrace", async_backtrace::framed)]
    async fn exit(mut self) {
        for mut task in self.inner.tasks.drain(..) {
            task.abort().await;
        }

        self.inner.root.exit(Duration::from_secs(1)).await;
    }
}

pub struct Runtime {
    inner: Option<Inner>,
}

impl Runtime {
    pub(crate) fn new() -> Self {
        Self {
            inner: Some(Inner {
                root: crate::supervisor(crate::SupervisorStrategy::OneForOne),
                tasks: vec![],
            }),
        }
    }

    #[track_caller]
    fn into_guard(mut self) -> RuntimeGuard {
        let inner = self
            .inner
            .take()
            .expect("unreachable: runtime is currently running");

        RuntimeGuard { inner }
    }
}

impl Runtime {
    /// create a future to be spawned when [`Runtime::block_on`] is called.
    #[track_caller]
    pub fn spawn_with<Fut, F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut Self) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let fut = (f)(&mut self).boxed();
        self.inner
            .as_mut()
            .expect("unreachable: runtime is currently running")
            .tasks
            .push(Task::Fut(fut));

        self
    }

    /// enter the runtime scope and run the provided future to completion.
    #[cfg_attr(feature = "backtrace", async_backtrace::framed)]
    pub async fn block_on<Fut, T>(self, fut: Fut) -> T
    where
        Fut: Future<Output = T>,
    {
        let mut guard = self.into_guard();
        guard.spawn_task_futures();
        let output = fut.await;
        guard.exit().await;
        output
    }
}
