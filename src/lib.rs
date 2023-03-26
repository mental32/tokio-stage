#![doc = include_str!("../README.md")]
//! ---
//!

mod graceful_shutdown;
pub use graceful_shutdown::graceful_shutdown;

mod group;
pub use group::{Group, GroupBuilder};

mod supervisor;

mod mailbox;
pub use mailbox::{mailbox, MailboxReceiver, MailboxSender};

/// build, configure, and spawn a task group.
#[inline]
#[track_caller]
pub fn group() -> group::GroupBuilder {
    group::GroupBuilder::default()
}

pub mod prelude {}
