//! `origin-ci`, origin's CI plugin: it runs the checks a revision's
//! `checks.toml` lists, and issues their results as statements, which the
//! landing queue takes as evidence.
//!
//! As a tool, `origin ci run` checks a revision of the repository it runs
//! in, as the user. As a worker, `origin ci worker` answers a remote's
//! `check.requested` events, with a token that annotates `ci.*`. Both talk
//! to origin only through its public API: the SDK over HTTP, or for a
//! repository on a local path, the same service in-process.

pub mod checks;
pub mod runner;
pub mod state;
pub mod worker;
