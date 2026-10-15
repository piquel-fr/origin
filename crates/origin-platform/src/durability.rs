use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::num::NonZero;
use std::panic;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;

use crate::{Error, Op};

/// Flushes written data to stable storage, so it survives a crash or a power
/// loss.
///
/// The default methods use the standard library, which issues each
/// platform's strongest sync. Each platform syncs batches its own way.
pub trait Durability {
    /// Flushes a file's contents, and the metadata needed to read them back.
    ///
    /// # Errors
    ///
    /// Fails if the file system reports a write error.
    fn sync_file(&self, file: &File) -> io::Result<()> {
        file.sync_data()
    }

    /// Flushes a directory's entries, making the files created in, renamed
    /// into or removed from it durable.
    ///
    /// # Errors
    ///
    /// Fails if the directory cannot be opened or the file system reports a
    /// write error.
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        File::open(dir)?.sync_all()
    }

    /// Flushes the contents of many files at once, more cheaply than
    /// syncing each. The files must be on one file system.
    ///
    /// # Errors
    ///
    /// Fails if a file cannot be opened or the file system reports a write
    /// error, naming the file involved.
    fn sync_batch<'f>(&self, files: impl IntoIterator<Item = &'f Path>) -> Result<(), Error>;
}

/// Files written durably as a group, at the cost of one batch sync and one
/// sync per directory involved.
///
/// [`DurableWrite::stage`] writes each file under a temporary name in its
/// destination's directory, which [`DurableWrite::is_temporary`] recognizes.
/// [`DurableWrite::commit`] then makes them all durable with
/// [`Durability::sync_batch`], renames them into place from several threads
/// and syncs their directories. Readers see either the previous file or the
/// complete new one, and once `commit` returns a crash loses none of them.
/// Dropping the write before committing removes its temporary files; a crash
/// leaves them behind, so code that lists a directory written this way skips
/// them, and removes the stale ones.
///
/// Staging beside the destination keeps each rename within one directory,
/// so writes to different directories never contend on a shared one.
///
/// [`DurableWrite::sync_with`] adds a file written in place, such as an
/// append-only log, to the batch sync, so it becomes durable no later than
/// the batch is published, at no extra sync.
///
/// Files can be staged from several threads at once.
#[derive(Debug)]
pub struct DurableWrite<'a, P> {
    platform: &'a P,
    staged: Mutex<Staged>,
}

/// The files staged so far.
#[derive(Debug, Default)]
struct Staged {
    /// Each temporary file and the path it's published to.
    files: Vec<(PathBuf, PathBuf)>,
    /// The directories whose entries must be durable: the published files'
    /// parents, and those added with [`DurableWrite::sync_dir_with`].
    dirs: BTreeSet<PathBuf>,
    /// Files written in place, synced with the batch but never renamed.
    in_place: Vec<PathBuf>,
}

/// Distinguishes the temporary files of one process.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

impl<P> DurableWrite<'_, P> {
    /// How temporary file names start: `.tmp-<pid>-<n>`.
    const TEMP_PREFIX: &str = ".tmp-";

    /// The fewest renames worth a thread of their own in
    /// [`DurableWrite::commit`]: below that, spawning costs more than it
    /// saves.
    const RENAMES_PER_THREAD: usize = 64;

    /// Whether `name` is the name of a temporary file: one being written, or
    /// left behind by a write that crashed.
    #[must_use]
    pub fn is_temporary(name: &OsStr) -> bool {
        name.as_encoded_bytes()
            .starts_with(Self::TEMP_PREFIX.as_bytes())
    }
}

impl<'a, P: Durability> DurableWrite<'a, P> {
    /// Starts a write on `platform`.
    pub fn new(platform: &'a P) -> Self {
        Self {
            platform,
            staged: Mutex::default(),
        }
    }

    /// Writes `contents` as the next version of `path`, and creates its
    /// missing parent directories. The file appears at `path` on
    /// [`DurableWrite::commit`], replacing any file there.
    ///
    /// Each directory created is durable at once, whatever becomes of the
    /// write, so a later write into it never relies on this one committing.
    /// Only a creator that dies between creating a directory and syncing
    /// its parent leaves one that isn't, so create shared directories up
    /// front.
    ///
    /// # Errors
    ///
    /// Fails if a directory or the temporary file cannot be created or
    /// written.
    pub fn stage(&self, path: impl Into<PathBuf>, contents: &[u8]) -> Result<(), Error> {
        self.stage_with(path, |file| file.write_all(contents))
    }

    /// Stages `path` like [`DurableWrite::stage`], with the contents `write`
    /// writes through a buffered [`StagedFile`], so a file never needs to
    /// fit in memory. If `write` fails, nothing is staged.
    ///
    /// # Errors
    ///
    /// Fails like [`DurableWrite::stage`], or with the error of `write`.
    pub fn stage_with<E: From<Error>>(
        &self,
        path: impl Into<PathBuf>,
        write: impl FnOnce(&mut StagedFile) -> Result<(), E>,
    ) -> Result<(), E> {
        let path = path.into();
        self.create_parents(&path)?;
        let temp = Self::parent(&path).join(format!(
            "{}{}-{}",
            Self::TEMP_PREFIX,
            process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(error) = StagedFile::write(&temp, write) {
            // Best effort: the file is unusable either way.
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        let mut staged = self.staged.lock().unwrap_or_else(PoisonError::into_inner);
        staged.dirs.insert(Self::parent(&path).to_owned());
        staged.files.push((temp, path));
        Ok(())
    }

    /// Makes the entries of `dir` durable with the batch, as for a file
    /// staged in it: for a file another write published, which this one
    /// relies on, and may not have synced yet.
    pub fn sync_dir_with(&self, dir: impl Into<PathBuf>) {
        let mut staged = self.staged.lock().unwrap_or_else(PoisonError::into_inner);
        staged.dirs.insert(dir.into());
    }

    /// Makes `path`, a file already written in place, durable with the
    /// batch: [`DurableWrite::commit`] syncs it with the staged files, before
    /// publishing any of them.
    pub fn sync_with(&self, path: impl Into<PathBuf>) {
        let mut staged = self.staged.lock().unwrap_or_else(PoisonError::into_inner);
        staged.in_place.push(path.into());
    }

    /// Creates the missing parent directories of `path`, outermost first,
    /// syncing the directory each is created in right after creating it.
    fn create_parents(&self, path: &Path) -> Result<(), Error> {
        let mut missing = Vec::new();
        let mut dir = Self::parent(path);
        while !dir.is_dir() {
            missing.push(dir);
            dir = Self::parent(dir);
        }
        for dir in missing.into_iter().rev() {
            match fs::create_dir(dir) {
                // Another write created it first, and may not have synced
                // its parent yet.
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                result => result.map_err(|source| Error::new(Op::CreateDir, dir, source))?,
            }
            let parent = Self::parent(dir);
            self.platform
                .sync_dir(parent)
                .map_err(|source| Error::new(Op::Sync, parent, source))?;
        }
        Ok(())
    }

    /// The directory containing `path`, which is `.` for a bare file name.
    fn parent(path: &Path) -> &Path {
        match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        }
    }

    /// Syncs every staged file and every file added with
    /// [`DurableWrite::sync_with`], renames the staged files into place, then
    /// syncs each directory involved once.
    ///
    /// # Errors
    ///
    /// Fails if the files cannot be synced, in which case none is published,
    /// or if a file cannot be renamed or a directory cannot be synced. Files
    /// renamed before such a failure are visible but may not be durable.
    pub fn commit(mut self) -> Result<(), Error> {
        let staged = self
            .staged
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        let temps = staged.files.iter().map(|(temp, _)| temp.as_path());
        self.platform
            .sync_batch(temps.chain(staged.in_place.iter().map(PathBuf::as_path)))?;
        Self::rename_all(&staged.files)?;
        // Every file is published, so dropping the write removes none.
        staged.files.clear();
        for dir in &staged.dirs {
            self.platform
                .sync_dir(dir)
                .map_err(|source| Error::new(Op::Sync, dir, source))?;
        }
        Ok(())
    }

    /// Renames each temporary file to its path, spread over the cores. Each
    /// rename locks its directory, and a batch usually spans many.
    fn rename_all(files: &[(PathBuf, PathBuf)]) -> Result<(), Error> {
        let rename = |files: &[(PathBuf, PathBuf)]| {
            files.iter().try_for_each(|(temp, path)| {
                fs::rename(temp, path).map_err(|source| Error::new(Op::Publish, path, source))
            })
        };
        let most = files.len() / Self::RENAMES_PER_THREAD;
        // Checked first: asking for the cores reads the cgroup's quota.
        if most <= 1 {
            return rename(files);
        }
        let threads = thread::available_parallelism()
            .map_or(1, NonZero::get)
            .min(most);
        thread::scope(|scope| {
            files
                .chunks(files.len().div_ceil(threads))
                .map(|chunk| scope.spawn(move || rename(chunk)))
                .collect::<Vec<_>>()
                .into_iter()
                .try_for_each(|thread| {
                    thread
                        .join()
                        .unwrap_or_else(|panic| panic::resume_unwind(panic))
                })
        })
    }
}

impl<P> Drop for DurableWrite<'_, P> {
    /// Removes the temporary files that were not published.
    fn drop(&mut self) {
        let staged = self
            .staged
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        for (temp, _) in &staged.files {
            // Best effort: a leftover is garbage that readers skip. After a
            // failed rename, some of these are already published.
            let _ = fs::remove_file(temp);
        }
    }
}

/// A temporary file being written by [`DurableWrite::stage_with`].
#[derive(Debug)]
pub struct StagedFile {
    file: BufWriter<File>,
    temp: PathBuf,
}

impl StagedFile {
    /// Creates `temp` and fills it with `write`.
    fn write<E: From<Error>>(
        temp: &Path,
        write: impl FnOnce(&mut Self) -> Result<(), E>,
    ) -> Result<(), E> {
        let file = File::create(temp).map_err(|source| Error::new(Op::Write, temp, source))?;
        let mut staged = Self {
            file: BufWriter::new(file),
            temp: temp.to_owned(),
        };
        write(&mut staged)?;
        staged
            .file
            .flush()
            .map_err(|source| Error::new(Op::Write, temp, source))?;
        Ok(())
    }

    /// Appends `bytes` to the file.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be written.
    pub fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.file
            .write_all(bytes)
            .map_err(|source| Error::new(Op::Write, &self.temp, source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records syncs instead of performing them, and fails batches when
    /// asked to.
    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<Call>>,
        fail_batches: bool,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Dir(PathBuf),
        /// A batch sync, with how many of its files were still unpublished
        /// temporary files.
        Batch(usize),
    }

    impl Recorder {
        fn record(&self, call: Call) {
            self.calls.lock().unwrap().push(call);
        }

        fn take(&self) -> Vec<Call> {
            std::mem::take(&mut self.calls.lock().unwrap())
        }
    }

    impl Durability for Recorder {
        fn sync_dir(&self, dir: &Path) -> io::Result<()> {
            self.record(Call::Dir(dir.to_owned()));
            Ok(())
        }

        fn sync_batch<'f>(&self, files: impl IntoIterator<Item = &'f Path>) -> Result<(), Error> {
            if self.fail_batches {
                let source = io::Error::other("batch failed");
                return Err(Error::new(Op::Sync, Path::new("batch"), source));
            }
            let unpublished = files.into_iter().filter(|temp| temp.exists()).count();
            self.record(Call::Batch(unpublished));
            Ok(())
        }
    }

    /// The names in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn syncs_the_batch_before_publishing_and_each_directory_once() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        fs::create_dir(root.join("a")).unwrap();
        let recorder = Recorder::default();
        let write = DurableWrite::new(&recorder);

        write.stage(root.join("a/1"), b"one").unwrap();
        write.stage(root.join("a/2"), b"two").unwrap();
        assert_eq!(recorder.take(), [], "staging syncs nothing");
        write.stage(root.join("b/c/3"), b"three").unwrap();
        assert_eq!(
            recorder.take(),
            [Call::Dir(root.to_owned()), Call::Dir(root.join("b"))],
            "but the directories it creates, at once"
        );
        let staged = names(&root.join("a"));
        assert_eq!(staged.len(), 2);
        assert!(
            staged
                .iter()
                .all(|name| DurableWrite::<Recorder>::is_temporary(name.as_ref())),
            "files are staged beside their destination, under temporary names: {staged:?}"
        );

        write.commit().unwrap();
        assert_eq!(
            recorder.take(),
            [
                Call::Batch(3),
                Call::Dir(root.join("a")),
                Call::Dir(root.join("b/c")),
            ],
            "the whole batch is synced before any file is renamed, then each \
             changed directory once"
        );
        assert_eq!(fs::read(root.join("a/1")).unwrap(), b"one");
        assert_eq!(fs::read(root.join("b/c/3")).unwrap(), b"three");
        assert_eq!(
            names(&root.join("a")),
            ["1", "2"],
            "no temporary file is left"
        );
        assert_eq!(names(&root.join("b/c")), ["3"]);
    }

    #[test]
    fn directories_stay_durable_when_their_write_is_abandoned() {
        let root = tempfile::tempdir().unwrap();
        let recorder = Recorder::default();
        let abandoned = DurableWrite::new(&recorder);
        abandoned
            .stage(root.path().join("new/file"), b"old")
            .unwrap();
        drop(abandoned);
        assert_eq!(recorder.take(), [Call::Dir(root.path().to_owned())]);

        // A later write into the directory relies on it.
        let retry = DurableWrite::new(&recorder);
        retry.stage(root.path().join("new/file"), b"new").unwrap();
        retry.commit().unwrap();
        assert_eq!(
            recorder.take(),
            [Call::Batch(1), Call::Dir(root.path().join("new"))]
        );
    }

    #[test]
    fn syncs_the_directories_of_files_relied_on() {
        let root = tempfile::tempdir().unwrap();
        let recorder = Recorder::default();
        let write = DurableWrite::new(&recorder);
        write.sync_dir_with(root.path().join("other"));
        write.stage(root.path().join("file"), b"contents").unwrap();
        write.commit().unwrap();
        assert_eq!(
            recorder.take(),
            [
                Call::Batch(1),
                Call::Dir(root.path().to_owned()),
                Call::Dir(root.path().join("other")),
            ]
        );
    }

    #[test]
    fn a_failed_batch_sync_publishes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let recorder = Recorder {
            fail_batches: true,
            ..Recorder::default()
        };
        let write = DurableWrite::new(&recorder);
        write.stage(root.path().join("file"), b"contents").unwrap();

        assert_eq!(write.commit().unwrap_err().op, Op::Sync);
        assert_eq!(names(root.path()), Vec::<String>::new(), "nothing is left");
        assert_eq!(recorder.take(), [], "no directory is synced");
    }

    #[test]
    fn files_written_in_place_are_synced_with_the_batch() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("log");
        fs::write(&log, b"appended").unwrap();
        let recorder = Recorder::default();
        let write = DurableWrite::new(&recorder);
        write.stage(root.path().join("file"), b"contents").unwrap();
        write.sync_with(&log);
        write.commit().unwrap();

        assert_eq!(
            recorder.take(),
            [Call::Batch(2), Call::Dir(root.path().to_owned())],
            "the log is synced in the batch, and its directory isn't"
        );
        assert_eq!(fs::read(&log).unwrap(), b"appended", "it stays in place");
    }

    #[test]
    fn replaces_existing_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("head");
        fs::write(&path, b"old").unwrap();
        let recorder = Recorder::default();
        let write = DurableWrite::new(&recorder);
        write.stage(&path, b"new").unwrap();
        write.commit().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn streamed_files_are_staged_whole_or_not_at_all() {
        let root = tempfile::tempdir().unwrap();
        let recorder = Recorder::default();
        let write = DurableWrite::new(&recorder);
        write
            .stage_with(root.path().join("whole"), |file| {
                file.write_all(b"one ")?;
                file.write_all(b"two")
            })
            .unwrap();
        let failed = write.stage_with(root.path().join("partial"), |file| {
            file.write_all(b"one ")?;
            Err(Error::new(
                Op::Write,
                Path::new("source"),
                io::Error::other("gone"),
            ))
        });
        assert_eq!(failed.unwrap_err().path, Path::new("source"));
        write.commit().unwrap();
        assert_eq!(names(root.path()), ["whole"], "the failed file is removed");
        assert_eq!(fs::read(root.path().join("whole")).unwrap(), b"one two");
    }

    #[test]
    fn dropping_an_uncommitted_write_publishes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let recorder = Recorder::default();
        let write = DurableWrite::new(&recorder);
        write.stage(root.path().join("file"), b"contents").unwrap();
        drop(write);

        assert_eq!(names(root.path()), Vec::<String>::new(), "nothing is left");
        assert_eq!(recorder.take(), []);
    }
}
