//! The conformance suite every [`Platform`] must pass, run against the one
//! built for this target.

use origin_platform::Native;

/// Runs each test of the suite against a platform.
macro_rules! conformance {
    ($platform:expr; $($test:ident),* $(,)?) => {
        $(
            #[test]
            fn $test() {
                suite::$test(&$platform);
            }
        )*
    };
}

conformance!(
    Native::default();
    batches_survive_reopen,
    empty_batches_succeed,
    syncing_a_missing_directory_fails,
    syncing_a_missing_file_in_a_batch_fails,
    exclusive_locks_exclude_everyone,
    shared_locks_coexist,
    blocking_locks_wait_for_release,
    locks_are_released_when_the_holder_dies,
    clones_are_independent_or_unsupported,
    clone_failures_are_not_unsupported,
    temp_dirs_are_local,
    inspecting_a_missing_path_fails,
    positioned_reads_share_a_file,
);

mod suite {
    use std::env;
    use std::fs;
    use std::io::{self, BufRead, BufReader};
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use origin_platform::{
        CloneError, DurableWrite, Error, FsKind, LockMode, LockedFile, Op, Platform, WritableError,
    };

    /// Stages a batch from several threads, large enough to be renamed from
    /// several too, then reads it back through fresh opens once the write is
    /// gone.
    pub fn batches_survive_reopen(platform: &impl Platform) {
        let root = tempfile::tempdir().unwrap();
        let path = |file: usize| root.path().join(format!("objects/{:02x}/{file}", file % 4));
        let files: Vec<usize> = (0..512).collect();
        let write = DurableWrite::new(platform);
        thread::scope(|scope| {
            for chunk in files.chunks(128) {
                let write = &write;
                scope.spawn(move || {
                    for &file in chunk {
                        write
                            .stage(path(file), file.to_string().as_bytes())
                            .unwrap();
                    }
                });
            }
        });
        write.commit().unwrap();

        for &file in &files {
            assert_eq!(fs::read(path(file)).unwrap(), file.to_string().as_bytes());
        }
        let published = fs::read_dir(root.path().join("objects"))
            .unwrap()
            .flat_map(|shard| fs::read_dir(shard.unwrap().path()).unwrap())
            .count();
        assert_eq!(published, files.len(), "no temporary file is left");
    }

    pub fn empty_batches_succeed(platform: &impl Platform) {
        DurableWrite::new(platform).commit().unwrap();
    }

    pub fn syncing_a_missing_directory_fails(platform: &impl Platform) {
        let root = tempfile::tempdir().unwrap();
        let error = platform.sync_dir(&root.path().join("missing")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    pub fn syncing_a_missing_file_in_a_batch_fails(platform: &impl Platform) {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        let error = platform.sync_batch([missing.as_path()]).unwrap_err();
        assert_eq!(error.path, missing);
        assert_eq!(error.source.kind(), io::ErrorKind::NotFound);
    }

    pub fn exclusive_locks_exclude_everyone(platform: &impl Platform) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lock");
        let held = LockedFile::open(platform, &path, LockMode::Exclusive).unwrap();
        for mode in [LockMode::Exclusive, LockMode::Shared] {
            let other = LockedFile::try_open(platform, &path, mode).unwrap();
            assert!(other.is_none(), "{mode:?} lock taken while held");
        }
        drop(held);
        assert!(
            LockedFile::try_open(platform, &path, LockMode::Exclusive)
                .unwrap()
                .is_some()
        );
    }

    pub fn shared_locks_coexist(platform: &impl Platform) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lock");
        let _first = LockedFile::open(platform, &path, LockMode::Shared).unwrap();
        let second = LockedFile::try_open(platform, &path, LockMode::Shared).unwrap();
        assert!(second.is_some());
        let exclusive = LockedFile::try_open(platform, &path, LockMode::Exclusive).unwrap();
        assert!(exclusive.is_none());
    }

    pub fn blocking_locks_wait_for_release(platform: &impl Platform) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lock");
        let held = LockedFile::open(platform, &path, LockMode::Exclusive).unwrap();
        let (locked, waiter) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                let _lock = LockedFile::open(platform, &path, LockMode::Exclusive).unwrap();
                locked.send(()).unwrap();
            });
            assert!(
                waiter.recv_timeout(Duration::from_millis(100)).is_err(),
                "locked while held"
            );
            drop(held);
            waiter
                .recv_timeout(Duration::from_secs(10))
                .expect("still waiting after release");
        });
    }

    /// Set in the child process that holds the lock, to the lock's path.
    const HOLDER: &str = "ORIGIN_TEST_LOCK_HOLDER";
    /// What the child prints once it holds the lock.
    const HELD: &str = "origin: lock held";

    /// Reruns this test binary as a child that holds a lock, then kills it
    /// as a crash would.
    pub fn locks_are_released_when_the_holder_dies(platform: &impl Platform) {
        if let Some(path) = env::var_os(HOLDER) {
            let _held = LockedFile::open(platform, Path::new(&path), LockMode::Exclusive).unwrap();
            println!("{HELD}");
            // Bounded, so the child cannot outlive a failed parent for long.
            thread::sleep(Duration::from_secs(60));
            return;
        }

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lock");
        let mut child = Command::new(env::current_exe().unwrap())
            .args(["locks_are_released_when_the_holder_dies", "--nocapture"])
            .env(HOLDER, &path)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        assert!(
            output.lines().any(|line| line.unwrap() == HELD),
            "the child never held the lock"
        );
        let other = LockedFile::try_open(platform, &path, LockMode::Exclusive).unwrap();
        assert!(other.is_none(), "locked while the child holds it");

        child.kill().unwrap();
        child.wait().unwrap();
        let other = LockedFile::try_open(platform, &path, LockMode::Exclusive).unwrap();
        assert!(other.is_some(), "the lock outlived its holder");
    }

    /// Clones where the test's file system can, and reports it can't
    /// otherwise.
    pub fn clones_are_independent_or_unsupported(platform: &impl Platform) {
        let root = tempfile::tempdir().unwrap();
        let (from, to) = (root.path().join("from"), root.path().join("to"));
        fs::write(&from, b"original").unwrap();
        match platform.clone_file(&from, &to) {
            Ok(()) => {
                assert_eq!(fs::read(&to).unwrap(), b"original");
                fs::write(&to, b"changed").unwrap();
                assert_eq!(fs::read(&from).unwrap(), b"original");
            }
            Err(CloneError::Unsupported { .. }) => {
                assert!(!to.exists(), "a failed clone left a file behind");
            }
            Err(error) => panic!("{error}: {error:?}"),
        }
    }

    /// Callers copy on `Unsupported`, which must not hide real failures.
    pub fn clone_failures_are_not_unsupported(platform: &impl Platform) {
        let root = tempfile::tempdir().unwrap();
        let (from, to) = (root.path().join("from"), root.path().join("to"));
        let error = platform.clone_file(&from, &to).unwrap_err();
        assert!(
            matches!(error, CloneError::Io { .. }),
            "missing source: {error}"
        );

        fs::write(&from, b"new").unwrap();
        fs::write(&to, b"existing").unwrap();
        let error = platform.clone_file(&from, &to).unwrap_err();
        assert!(
            matches!(error, CloneError::Io { .. }),
            "existing target: {error}"
        );
        assert_eq!(fs::read(&to).unwrap(), b"existing");
    }

    pub fn temp_dirs_are_local(platform: &impl Platform) {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(platform.file_system(root.path()).unwrap(), FsKind::Local);
        platform.check_writable(root.path()).unwrap();
    }

    pub fn inspecting_a_missing_path_fails(platform: &impl Platform) {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        let error = platform.check_writable(&missing).unwrap_err();
        assert!(
            matches!(error, WritableError::Io(Error { op: Op::Inspect, ref path, .. }) if *path == missing),
            "{error:?}"
        );
    }

    pub fn positioned_reads_share_a_file(platform: &impl Platform) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("pack");
        let contents: Vec<u8> = (0..=u8::MAX).collect();
        fs::write(&path, &contents).unwrap();
        let file = fs::File::open(&path).unwrap();
        thread::scope(|scope| {
            for offset in [0_usize, 100, 200] {
                let (file, contents) = (&file, &contents);
                scope.spawn(move || {
                    let mut bytes = [0; 56];
                    let at = u64::try_from(offset).unwrap();
                    platform.read_exact_at(file, &mut bytes, at).unwrap();
                    assert_eq!(bytes, contents[offset..][..56]);
                });
            }
        });
        let mut past_the_end = [0; 2];
        let error = platform
            .read_exact_at(&file, &mut past_the_end, 255)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }
}
