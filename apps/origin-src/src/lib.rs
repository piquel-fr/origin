//! The origin command line and server. Both share the repository logic in
//! this library; `main.rs` only parses arguments and dispatches.

pub mod cli;
pub mod config;
#[cfg(feature = "fixture")]
pub mod fixture;
pub mod index;
pub mod output;
pub mod store;
pub mod targets;
pub mod tree;
pub mod workspace;
pub mod worktree;
