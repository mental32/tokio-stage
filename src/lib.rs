#![doc = include_str!("../README.md")]
//! ---
//!

pub mod group;

/// A newly created group builder configured with defaults.
#[inline]
#[track_caller]
pub fn group() -> group::GroupBuilder {
    group::GroupBuilder::default()
}

pub mod prelude {}
