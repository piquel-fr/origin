//! Linux: `fdatasync` and `fsync`, `syncfs` for large batches, `flock`,
//! clones with the `FICLONE` ioctl (Btrfs, XFS, bcachefs), `pread`, file
//! systems told apart by their `statfs` magic number, and the files a
//! process group holds read from `/proc`.

use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::os::unix::fs::MetadataExt;
use std::panic;
use std::path::Path;
use std::thread;

use crate::{CloneFile, Durability, Error, FileLock, FileSystem, FsKind, Op, Processes, ReadAt};

/// The Linux implementation of [`Platform`](crate::Platform).
#[derive(Clone, Copy, Debug, Default)]
pub struct Linux;

impl Linux {
    /// The most files a batch syncs each on its own, rather than with
    /// `syncfs`.
    const SYNC_EACH: usize = 2;

    /// The file system has no clone ioctl at all.
    const ENOTTY: i32 = 25;

    /// The magic number every FUSE file system reports.
    const FUSE: u32 = 0x6573_5546;

    /// Network file systems by magic number, from `linux/magic.h` and the
    /// file systems' own sources.
    const NETWORK: [(u32, &str); 11] = [
        (0x6969, "NFS"),
        (0x517B, "SMB"),
        (0xFF53_4D42, "CIFS"),
        (0xFE53_4D42, "SMB2"),
        (0x0102_1997, "9p"),
        (0x5346_414F, "AFS"),
        (0x6B41_4653, "kAFS"),
        (0x00C3_6400, "Ceph"),
        (0x7375_7245, "Coda"),
        (0x0BD0_0BD0, "Lustre"),
        (0x4750_4653, "GPFS"),
    ];

    /// Opens `path` and syncs it with `sync`.
    fn sync_path(path: &Path, sync: impl FnOnce(&File) -> io::Result<()>) -> Result<(), Error> {
        let file = File::open(path).map_err(|source| Error::new(Op::Open, path, source))?;
        sync(&file).map_err(|source| Error::new(Op::Sync, path, source))
    }

    /// Classifies a file system by its magic number. Any file system not
    /// known to be networked is local, except FUSE: sshfs and local disks
    /// served through it share one magic number.
    fn classify(magic: u32) -> FsKind {
        if magic == Self::FUSE {
            return FsKind::Unknown("FUSE");
        }
        Self::NETWORK
            .iter()
            .find(|(network, _)| *network == magic)
            .map_or(FsKind::Local, |(_, name)| FsKind::Network(name))
    }
}

impl Durability for Linux {
    /// One `syncfs` of the file system holding the files, which costs about
    /// as much as two `fdatasync`s however many files there are.
    ///
    /// `syncfs` also waits for every other process's unwritten data on that
    /// file system, so a busy disk slows it down. Batches of up to two
    /// files, such as a record and the index log, sync each on its own
    /// instead, all at once: a journaling file system commits them together,
    /// for about the cost of one `fdatasync`.
    fn sync_batch<'f>(&self, files: impl IntoIterator<Item = &'f Path>) -> Result<(), Error> {
        let first: Vec<_> = files.into_iter().take(Self::SYNC_EACH + 1).collect();
        let Some((&one, others)) = first.split_first() else {
            return Ok(());
        };
        if others.len() >= Self::SYNC_EACH {
            return Self::sync_path(one, |file| {
                rustix::fs::syncfs(file).map_err(io::Error::from)
            });
        }
        thread::scope(|scope| {
            let others: Vec<_> = others
                .iter()
                .map(|&other| {
                    scope.spawn(move || Self::sync_path(other, |file| self.sync_file(file)))
                })
                .collect();
            let one = Self::sync_path(one, |file| self.sync_file(file));
            for other in others {
                other
                    .join()
                    .unwrap_or_else(|panic| panic::resume_unwind(panic))?;
            }
            one
        })
    }
}

impl FileLock for Linux {}

impl Processes for Linux {
    /// Reads `/proc`: each process's group from its `stat`, then the files
    /// it holds from its `fd`. Processes that end meanwhile, or whose files
    /// may not be read, are skipped.
    fn group_holds(&self, id: u32, path: &Path) -> io::Result<bool> {
        let held = fs::metadata(path)?;
        for process in fs::read_dir("/proc")? {
            let process = process?.path();
            let pid = process
                .file_name()
                .and_then(|name| name.to_str()?.parse().ok());
            if pid.and_then(Self::group_of) != Some(id) {
                continue;
            }
            let Ok(files) = fs::read_dir(process.join("fd")) else {
                continue;
            };
            let holds = |file: fs::DirEntry| {
                fs::metadata(file.path())
                    .is_ok_and(|open| (open.dev(), open.ino()) == (held.dev(), held.ino()))
            };
            if files.flatten().any(holds) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl Linux {
    /// The process group of the process `pid`, unless it ended or may not
    /// be read: the fifth field of `/proc/<pid>/stat`, the third after its
    /// command's name, which is in parentheses and may hold any character.
    fn group_of(pid: u32) -> Option<u32> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, after) = stat.rsplit_once(')')?;
        after.split_whitespace().nth(2)?.parse().ok()
    }
}

impl ReadAt for Linux {}

impl CloneFile for Linux {
    /// `FICLONE` reports `EOPNOTSUPP` on file systems that cannot clone,
    /// `EXDEV` across file systems, `EINVAL` when the file system cannot
    /// clone these particular files, and `ENOTTY` without the ioctl.
    fn cannot_clone(&self, error: &io::Error) -> bool {
        matches!(
            error.kind(),
            ErrorKind::Unsupported | ErrorKind::CrossesDevices | ErrorKind::InvalidInput
        ) || error.raw_os_error() == Some(Self::ENOTTY)
    }
}

impl FileSystem for Linux {
    /// Magic numbers are 32 bits wide, in a field as wide as a pointer: one
    /// that doesn't fit is no file system origin knows.
    fn file_system(&self, path: &Path) -> io::Result<FsKind> {
        let stat = rustix::fs::statfs(path)?;
        Ok(u32::try_from(stat.f_type).map_or(FsKind::Unknown("unrecognized"), Self::classify))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_hold_the_files_their_processes_hold() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("held");
        fs::write(&path, "").unwrap();
        let mut holder = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .stdin(File::open(&path).unwrap())
            .spawn()
            .unwrap();
        let mut other = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        assert!(Linux.group_holds(holder.id(), &path).unwrap());
        assert!(
            !Linux.group_holds(other.id(), &path).unwrap(),
            "another group"
        );
        for child in [&mut holder, &mut other] {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert!(!Linux.group_holds(holder.id(), &path).unwrap(), "it ended");
    }

    #[test]
    fn classifies_file_systems_by_magic_number() {
        assert_eq!(Linux::classify(0xEF53), FsKind::Local, "ext4");
        assert_eq!(Linux::classify(0x9123_683E), FsKind::Local, "Btrfs");
        assert_eq!(Linux::classify(0x6969), FsKind::Network("NFS"));
        assert_eq!(Linux::classify(0xFE53_4D42), FsKind::Network("SMB2"));
        assert_eq!(Linux::classify(0x0102_1997), FsKind::Network("9p"));
        assert_eq!(Linux::classify(0x6573_5546), FsKind::Unknown("FUSE"));
    }
}
