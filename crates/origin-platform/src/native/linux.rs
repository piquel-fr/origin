//! Linux: `fdatasync` and `fsync`, `syncfs` for batches, `flock`, clones with
//! the `FICLONE` ioctl (Btrfs, XFS, bcachefs), and file systems told apart by
//! their `statfs` magic number.

use std::fs::File;
use std::io::{self, ErrorKind};
use std::path::Path;

use crate::{CloneFile, Durability, Error, FileLock, FileSystem, FsKind, Op};

/// The Linux implementation of [`Platform`](crate::Platform).
#[derive(Clone, Copy, Debug, Default)]
pub struct Linux;

impl Linux {
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
    /// as much as two `fdatasync`s however many files there are. A single
    /// file is synced on its own, which is cheaper.
    ///
    /// `syncfs` also waits for every other process's unwritten data on that
    /// file system, so a busy disk slows it down.
    fn sync_batch<'f>(&self, files: impl IntoIterator<Item = &'f Path>) -> Result<(), Error> {
        let mut files = files.into_iter();
        let Some(path) = files.next() else {
            return Ok(());
        };
        let file = File::open(path).map_err(|source| Error::new(Op::Open, path, source))?;
        let synced = if files.next().is_none() {
            self.sync_file(&file)
        } else {
            rustix::fs::syncfs(&file).map_err(io::Error::from)
        };
        synced.map_err(|source| Error::new(Op::Sync, path, source))
    }
}

impl FileLock for Linux {}

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
    fn classifies_file_systems_by_magic_number() {
        assert_eq!(Linux::classify(0xEF53), FsKind::Local, "ext4");
        assert_eq!(Linux::classify(0x9123_683E), FsKind::Local, "Btrfs");
        assert_eq!(Linux::classify(0x6969), FsKind::Network("NFS"));
        assert_eq!(Linux::classify(0xFE53_4D42), FsKind::Network("SMB2"));
        assert_eq!(Linux::classify(0x0102_1997), FsKind::Network("9p"));
        assert_eq!(Linux::classify(0x6573_5546), FsKind::Unknown("FUSE"));
    }
}
