//! File operations that differ between platforms: durable writes, file
//! locks, copy-on-write clones, and telling local file systems from network
//! ones.
//!
//! Each operation is a trait. Every supported platform implements them in its
//! own module under [`native`], and [`Native`] picks the one for the build
//! target. That selection is the only conditional compilation in origin.
//! Windows is not supported.
//!
//! Code that needs the platform takes a `P: Platform` (or one of its traits),
//! so tests can substitute an implementation that records or fails calls.

mod clone;
mod durability;
mod error;
mod file_system;
mod lock;
pub mod native;

pub use clone::{CloneError, CloneFile};
pub use durability::{Durability, DurableWrite};
pub use error::{Error, Op};
pub use file_system::{FileSystem, FsKind, WritableError};
pub use lock::{FileLock, LockMode, LockedFile};
pub use native::Native;

/// Everything origin needs from the platform it runs on, shared by all the
/// threads of a process.
pub trait Platform: Durability + FileLock + CloneFile + FileSystem + Send + Sync {}

impl<T: Durability + FileLock + CloneFile + FileSystem + Send + Sync> Platform for T {}
