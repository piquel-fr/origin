//! The origin command line and server. Both share the repository logic in
//! this library; `main.rs` only parses arguments and dispatches.

pub mod ancestry;
pub mod annotations;
pub mod cli;
pub mod config;
pub mod conflict;
pub mod events;
#[cfg(feature = "fixture")]
pub mod fixture;
mod frames;
pub mod http;
pub mod index;
pub mod oplog;
pub mod output;
pub mod policy;
pub mod queue;
pub mod remote;
pub mod server;
pub mod service;
pub mod store;
pub mod sync;
pub mod targets;
pub mod tree;
pub mod workspace;
pub mod worktree;
