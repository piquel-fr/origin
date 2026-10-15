//! The cost of durability and locking on the native platform: what every
//! store write and target landing pays.

use std::path::PathBuf;
use std::thread;

use divan::Bencher;
use divan::counter::ItemsCount;
use origin_platform::{DurableWrite, LockMode, LockedFile, Native};

fn main() {
    divan::main();
}

/// The size of a typical small record.
const CONTENTS: [u8; 512] = [b'x'; 512];

/// Fresh paths for one durable write of `files` files, spread over
/// `shards` directories like loose objects.
fn paths(dir: &tempfile::TempDir, files: usize, shards: usize) -> impl FnMut() -> Vec<PathBuf> {
    let objects = dir.path().join("objects");
    let mut write = 0;
    move || {
        write += 1;
        (0..files)
            .map(|file| objects.join(format!("{:02x}/{write}-{file}", file % shards)))
            .collect()
    }
}

/// Each file written durably on its own: a batch sync and a directory sync
/// per file.
#[divan::bench(args = [1, 16, 128], sample_count = 10, sample_size = 1)]
fn one_at_a_time(bencher: Bencher<'_, '_>, files: usize) {
    let dir = tempfile::tempdir().unwrap();
    let platform = Native::default();
    bencher
        .counter(ItemsCount::new(files))
        .with_inputs(paths(&dir, files, 1))
        .bench_local_values(|paths| {
            for path in paths {
                let write = DurableWrite::new(&platform);
                write.stage(path, &CONTENTS).unwrap();
                write.commit().unwrap();
            }
        });
}

/// One group commit: a batch sync, and one for the directory.
#[divan::bench(args = [1, 16, 128], sample_count = 10, sample_size = 1)]
fn batched(bencher: Bencher<'_, '_>, files: usize) {
    let dir = tempfile::tempdir().unwrap();
    let platform = Native::default();
    bencher
        .counter(ItemsCount::new(files))
        .with_inputs(paths(&dir, files, 1))
        .bench_local_values(|paths| {
            let write = DurableWrite::new(&platform);
            for path in paths {
                write.stage(path, &CONTENTS).unwrap();
            }
            write.commit().unwrap();
        });
}

/// One group commit of files spread over 32 directories, like loose
/// objects.
#[divan::bench(args = [128], sample_count = 10, sample_size = 1)]
fn batched_sharded(bencher: Bencher<'_, '_>, files: usize) {
    let dir = tempfile::tempdir().unwrap();
    let platform = Native::default();
    bencher
        .counter(ItemsCount::new(files))
        .with_inputs(paths(&dir, files, 32))
        .bench_local_values(|paths| {
            let write = DurableWrite::new(&platform);
            for path in paths {
                write.stage(path, &CONTENTS).unwrap();
            }
            write.commit().unwrap();
        });
}

/// One group commit staged from 8 threads, which overlaps their writes.
#[divan::bench(args = [16, 128], sample_count = 10, sample_size = 1)]
fn batched_parallel(bencher: Bencher<'_, '_>, files: usize) {
    let dir = tempfile::tempdir().unwrap();
    let platform = Native::default();
    bencher
        .counter(ItemsCount::new(files))
        .with_inputs(paths(&dir, files, 1))
        .bench_local_values(|paths| {
            let write = DurableWrite::new(&platform);
            thread::scope(|scope| {
                for chunk in paths.chunks(files.div_ceil(8)) {
                    let write = &write;
                    scope.spawn(move || {
                        for path in chunk {
                            write.stage(path, &CONTENTS).unwrap();
                        }
                    });
                }
            });
            write.commit().unwrap();
        });
}

/// Taking and releasing an uncontended lock, as every landing does.
#[divan::bench]
fn lock(bencher: Bencher<'_, '_>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lock");
    let platform = Native::default();
    bencher.bench(|| drop(LockedFile::open(&platform, &path, LockMode::Exclusive).unwrap()));
}
