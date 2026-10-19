//! macOS: `F_FULLFSYNC`, which also flushes the drive's cache, `flock`,
//! clones with `clonefile` (APFS), `pread`, and file systems told apart by
//! their `statfs` type name.

use std::fs::File;
use std::io::{self, ErrorKind};
use std::path::Path;

use crate::{CloneFile, Durability, Error, FileLock, FileSystem, FsKind, Op, ReadAt};

/// The macOS implementation of [`Platform`](crate::Platform).
#[derive(Clone, Copy, Debug, Default)]
pub struct MacOs;

impl MacOs {
    /// The file system cannot clone. The standard library leaves this code
    /// uncategorized, since it differs from `EOPNOTSUPP` on macOS.
    const ENOTSUP: i32 = 45;

    /// Network file systems by type name.
    const NETWORK: [(&str, &str); 4] = [
        ("nfs", "NFS"),
        ("smbfs", "SMB"),
        ("afpfs", "AFP"),
        ("webdav", "WebDAV"),
    ];

    /// The type names of macFUSE, which serves sshfs as well as local disks.
    const FUSE: [&str; 3] = ["macfuse", "osxfuse", "osxfusefs"];

    /// Classifies a file system by its type name. Any file system not known
    /// to be networked is local, except FUSE.
    fn classify(name: &[u8]) -> FsKind {
        if Self::FUSE.iter().any(|fuse| fuse.as_bytes() == name) {
            return FsKind::Unknown("macFUSE");
        }
        Self::NETWORK
            .iter()
            .find(|(network, _)| network.as_bytes() == name)
            .map_or(FsKind::Local, |(_, display)| FsKind::Network(display))
    }
}

impl Durability for MacOs {
    /// A plain `fsync` of each file, which hands its data to the drive
    /// without flushing the drive's cache, then a single `F_FULLFSYNC` that
    /// flushes the cache for all of them, as SQLite does.
    fn sync_batch<'f>(&self, files: impl IntoIterator<Item = &'f Path>) -> Result<(), Error> {
        let mut last = None;
        for path in files {
            let file = File::open(path).map_err(|source| Error::new(Op::Open, path, source))?;
            rustix::fs::fsync(&file).map_err(|errno| Error::new(Op::Sync, path, errno.into()))?;
            last = Some((file, path));
        }
        let Some((file, path)) = last else {
            return Ok(());
        };
        rustix::fs::fcntl_fullfsync(&file).map_err(|errno| Error::new(Op::Sync, path, errno.into()))
    }
}

impl FileLock for MacOs {}

impl ReadAt for MacOs {}

impl CloneFile for MacOs {
    /// `clonefile` reports `ENOTSUP` on file systems that cannot clone and
    /// `EXDEV` across volumes.
    fn cannot_clone(&self, error: &io::Error) -> bool {
        matches!(
            error.kind(),
            ErrorKind::Unsupported | ErrorKind::CrossesDevices
        ) || error.raw_os_error() == Some(Self::ENOTSUP)
    }
}

impl FileSystem for MacOs {
    /// The type name is a NUL-terminated C string.
    fn file_system(&self, path: &Path) -> io::Result<FsKind> {
        let stat = rustix::fs::statfs(path)?;
        let name = stat.f_fstypename.map(|c| c.to_ne_bytes()[0]);
        let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        Ok(Self::classify(&name[..len]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_file_systems_by_type_name() {
        assert_eq!(MacOs::classify(b"apfs"), FsKind::Local);
        assert_eq!(MacOs::classify(b"smbfs"), FsKind::Network("SMB"));
        assert_eq!(MacOs::classify(b"nfs"), FsKind::Network("NFS"));
        assert_eq!(MacOs::classify(b"macfuse"), FsKind::Unknown("macFUSE"));
    }
}
