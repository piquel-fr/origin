//! The origin command line and server. Both share the repository logic in
//! this library; `main.rs` only parses arguments and dispatches.

pub mod cli;
#[cfg(feature = "fixture")]
pub mod fixture;
pub mod http;
pub mod output;
pub mod refs;
pub mod repository;
pub mod service;
pub mod store;
pub mod tree;
pub mod workspace;
pub mod worktree;
