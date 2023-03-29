#![doc = include_str!("../README.md")]
//! ---
//!

mod graceful_shutdown;
pub use graceful_shutdown::{graceful_shutdown, shutdown_scope};

mod group;
pub use group::{Group, GroupBuilder};

mod task;

mod simple_supervisor;

mod supervisor;
pub use supervisor::{supervisor, Supervisor, SupervisorStrategy};

mod mailbox;
pub use mailbox::{mailbox, MailboxReceiver, MailboxSender};

/// build, configure, and spawn a task group.
#[inline]
#[track_caller]
pub fn group() -> group::GroupBuilder {
    group::GroupBuilder::default()
}

pub mod prelude {}
