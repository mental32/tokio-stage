#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

mod graceful_shutdown;
pub use graceful_shutdown::graceful_shutdown;

mod group;
pub use group::{Group, GroupBuilder};

mod task;

mod simple_supervisor;

mod actor;
pub use actor::{actor, Address, Context, IntoContext, SendService};

mod supervisor;
pub use supervisor::{supervisor, Supervisor, SupervisorStrategy};

mod mailbox;
pub use mailbox::{mailbox, MailboxReceiver, MailboxSender};

/// Build and configure a task group.
///
/// Task groups allow you to perform task-level orchestration using
/// erlang-style supervisors. Simply it a task group will always restart
/// a future which panics its task but task groups may be dynamically upscaled,
/// suspended/resumed, and introspected.
///
/// This function is the intended way of users acquiring a
/// [`group::GroupBuilder`].
///
/// default values for the group configuration:
///
/// * shutdown timeout = `1 second`
/// * automatic shutdown = `true`
/// * "spawn at least" = `1`
/// * restart policy = `on-panic`
///
#[inline]
#[track_caller]
pub fn group() -> group::GroupBuilder {
    group::GroupBuilder(group::SupervisorConfig {
        shutdown_timeout: std::time::Duration::from_secs(1),
        automatic_shutdown: true,
        spawn_at_least: 1,
        restart_policy: group::RestartPolicy::OnPanic,
        interval_dur: std::time::Duration::from_millis(10),
        task_kind: task::TaskKind::Worker,
    })
}

/// Spawns a new asynchronous task, returning a [`Pid`](crate::task::Pid) for it.
///
/// This helper function is shorthand for using [`group()`], it is intentionally
/// written to look like [`tokio::spawn`].
///
/// The future returned by the provided function is not only spawned as a task
/// but it is managed by a supervisor with a simple one-for-one strategy. This
/// allows the task to be restarted if the future panics.
///
/// # Examples
///
/// ```
/// # #[tokio::main]
/// # async fn example() {
/// let pid = stage::spawn(|| async { println!("hi"); });
/// pid.abort();
/// # }
/// ```
#[track_caller]
#[inline]
pub fn spawn<F, Fut>(f: F) -> task::Pid<()>
where
    F: Send + Clone + Fn() -> Fut + 'static,
    Fut: Send + std::future::Future<Output = ()> + 'static,
{
    group().spawn_at_least(1).spawn(f).inner.into()
}
