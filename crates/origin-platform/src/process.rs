use std::fs::File;
use std::io;
use std::path::Path;

use rustix::io::FdFlags;
use rustix::process::{Pid, Signal};

/// What tells a command's processes apart, and stops them: process groups,
/// so a command started in a group of its own
/// (`CommandExt::process_group(0)`) is stopped whole, with whatever it
/// started and left running, which joins its group; and files they
/// inherit open, so a lock stays held while any of them lives.
///
/// The default methods use POSIX's `kill` and `fcntl`, which every
/// supported platform has.
pub trait Processes {
    /// Whether a process of the group `id` holds the file at `path` open:
    /// so a group that ended, whose number another group took since, is
    /// told apart from one that still runs what it started with the file.
    /// `false` where no reliable way to tell exists: then the group is left
    /// running rather than risk killing another.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be inspected, or the processes listed.
    fn group_holds(&self, id: u32, path: &Path) -> io::Result<bool>;

    /// Kills every process of the group `id`, if any is left.
    ///
    /// # Errors
    ///
    /// Fails if `id` names no group of its own, as 0 and 1 don't, or the
    /// group's processes may not be signalled.
    fn kill_group(&self, id: u32) -> io::Result<()> {
        // `kill(-1)` would signal every process.
        let pid = i32::try_from(id)
            .ok()
            .filter(|&id| id > 1)
            .and_then(Pid::from_raw)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a process group"))?;
        match rustix::process::kill_process_group(pid, Signal::KILL) {
            Err(rustix::io::Errno::SRCH) => Ok(()),
            killed => Ok(killed?),
        }
    }

    /// Has the commands started from now on inherit `file`, open, and their
    /// processes the ones they start, unless they close it: then a lock it
    /// holds stays held while any of them lives. Commands other threads
    /// start meanwhile inherit it too.
    ///
    /// # Errors
    ///
    /// Fails if the file's descriptor cannot be changed.
    fn inherit(&self, file: &File) -> io::Result<()> {
        let flags = rustix::io::fcntl_getfd(file)?;
        Ok(rustix::io::fcntl_setfd(file, flags - FdFlags::CLOEXEC)?)
    }
}
