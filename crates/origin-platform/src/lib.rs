//! File operations that differ between platforms: durable writes, file
//! locks, copy-on-write clones, positioned reads, and telling local file
//! systems from network ones.
//!
//! Each operation is a trait. Every supported platform implements them in its
//! own module, and [`Native`] picks the one for the build target. That
//! selection is the only conditional compilation in origin. Windows is not
//! supported.
//!
//! Code that needs the platform takes a `P: Platform` (or one of its traits),
//! so tests can substitute an implementation that records or fails calls.

mod clone;
mod durability;
mod error;
mod file_system;
mod lock;
mod read;

pub use clone::{CloneError, CloneFile};
pub use durability::{Durability, DurableWrite, StagedFile};
pub use error::{Error, Op};
pub use file_system::{FileSystem, FsKind, WritableError};
pub use lock::{FileLock, LockMode, LockedFile};
pub use read::ReadAt;

/// Everything origin needs from the platform it runs on, shared by all the
/// threads of a process.
pub trait Platform: Durability + FileLock + CloneFile + FileSystem + ReadAt + Send + Sync {}

impl<T: Durability + FileLock + CloneFile + FileSystem + ReadAt + Send + Sync> Platform for T {}

// The platform origin is built for. Each module calls its platform's own
// system calls, so only that one is compiled.
cfg_select! {
    target_os = "linux" => {
        pub mod linux;
        /// The platform origin was built for.
        pub type Native = linux::Linux;
    }
    target_os = "macos" => {
        pub mod macos;
        /// The platform origin was built for.
        pub type Native = macos::MacOs;
    }
}
