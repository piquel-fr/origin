//! Deterministic synthetic working trees, for benchmarks and tests. The same
//! shape always produces byte-identical files, so results are comparable
//! across runs and machines.
//!
//! Behind the `fixture` feature, so the `origin` binary never contains them.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The shape of a synthetic tree: `files` files of about `file_size` bytes,
/// `files_per_dir` per directory, in directories nested two levels deep.
#[derive(Clone, Copy, Debug)]
pub struct FixtureTree {
    /// Number of files.
    pub files: usize,
    /// Files in each directory.
    pub files_per_dir: usize,
    /// Approximate size of each file in bytes.
    pub file_size: usize,
}

impl FixtureTree {
    /// Roughly the shape of a large application repository.
    pub const LARGE: Self = Self {
        files: 20_000,
        files_per_dir: 20,
        file_size: 4 * 1024,
    };

    fn dir(&self, root: &Path, index: usize) -> PathBuf {
        let dir = index / self.files_per_dir;
        root.join(format!("dir{}/sub{}", dir / 32, dir % 32))
    }

    fn path(&self, root: &Path, index: usize) -> PathBuf {
        self.dir(root, index).join(format!("file{index}.rs"))
    }

    /// Source-code-like content: unique per file and per `version`.
    fn content(&self, index: usize, version: u64) -> String {
        let mut state = (index as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ version;
        let mut content = String::with_capacity(self.file_size + 64);
        while content.len() < self.file_size {
            // xorshift64: cheap, deterministic variety.
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            writeln!(
                content,
                "    let value_{:x} = compute({}, {});",
                state >> 40,
                state % 977,
                state % 13
            )
            .expect("writing to a String cannot fail");
        }
        content
    }

    /// Writes the tree under `root`.
    ///
    /// # Errors
    ///
    /// Fails if a file cannot be written.
    pub fn write(&self, root: &Path) -> io::Result<()> {
        for index in 0..self.files {
            if index % self.files_per_dir == 0 {
                fs::create_dir_all(self.dir(root, index))?;
            }
            fs::write(self.path(root, index), self.content(index, 0))?;
        }
        Ok(())
    }

    /// Rewrites `count` files spread across the tree with new content,
    /// numbered by `version` so successive edits differ.
    ///
    /// # Errors
    ///
    /// Fails if a file cannot be written.
    pub fn edit(&self, root: &Path, count: usize, version: u64) -> io::Result<()> {
        let step = (self.files / count.max(1)).max(1);
        for index in (0..self.files).step_by(step).take(count) {
            fs::write(self.path(root, index), self.content(index, version))?;
        }
        Ok(())
    }
}
