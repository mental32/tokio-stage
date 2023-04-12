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

/// build, configure, and spawn a task group.
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

pub mod prelude {
    //! prelude module
}
