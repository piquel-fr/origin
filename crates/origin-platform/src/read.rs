use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

/// Reads at an offset without moving a file's cursor, so any number of
/// threads can read one open file at once.
///
/// The default method uses `pread`, which every supported platform has.
pub trait ReadAt {
    /// Fills `buf` with the bytes of `file` that start at `offset`.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read, or ends before `buf` is full.
    fn read_exact_at(&self, file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
        file.read_exact_at(buf, offset)
    }
}
