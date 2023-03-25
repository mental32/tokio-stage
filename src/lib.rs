#![doc = include_str!("../README.md")]
//! ---
//!

mod graceful_shutdown;
pub use graceful_shutdown::graceful_shutdown;

pub mod group;

mod supervisor;

mod mailbox;
pub use mailbox::{mailbox, MailboxReceiver, MailboxSender};

/// A newly created group builder configured with defaults.
#[inline]
#[track_caller]
pub fn group() -> group::GroupBuilder {
    group::GroupBuilder::default()
}

pub mod prelude {}
