//! Packs: writing and reading a million statements loose and packed,
//! compacting them, and looking objects up across several packs. Run on a
//! real disk: syncs cost nothing on a RAM-backed file system.

use std::sync::LazyLock;

use divan::counter::ItemsCount;
use divan::{Bencher, black_box};
use origin_platform::Native;
use origin_shared::{EncodedObject, Kind, ObjectId, Record, RecordId, Statement, Verdict};
use origin_src::store::Store;

fn main() {
    divan::main();
}

/// How many statements the large stores hold.
const STATEMENTS: usize = 1_000_000;

/// `count` distinct statements, like a CI runner issues, from `seed` on.
fn statements(seed: usize, count: usize) -> Vec<EncodedObject> {
    (seed..seed + count)
        .map(|seed| {
            Statement {
                subject: RecordId::Revision(EncodedObject::blob(&seed.to_le_bytes()).id().into()),
                kind: Kind::new("ci.test").unwrap(),
                verdict: Some(Verdict::Pass),
                issuer: "node:ci-1".parse().unwrap(),
                inputs: None,
                payload: None,
                supersedes: None,
            }
            .encode()
        })
        .collect()
}

static STATEMENTS_1M: LazyLock<Vec<EncodedObject>> = LazyLock::new(|| statements(0, STATEMENTS));

/// An initialized store in a new temporary directory.
fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join(".origin"), Native::default());
    store.init().unwrap();
    (dir, store)
}

/// A store of the million statements, loose.
fn loose() -> (tempfile::TempDir, Store) {
    let (dir, store) = store();
    store.write_all(&STATEMENTS_1M).unwrap();
    (dir, store)
}

/// A store of the million statements in `packs` packs, halving in size so
/// compaction keeps them apart.
fn packed(packs: u32) -> (tempfile::TempDir, Store) {
    let (dir, store) = store();
    let mut written = 0;
    for pack in 0..packs {
        let count = if pack + 1 == packs {
            STATEMENTS - written
        } else {
            STATEMENTS >> (pack + 1)
        };
        store
            .write_all(&STATEMENTS_1M[written..written + count])
            .unwrap();
        store.compact().unwrap();
        written += count;
    }
    (dir, store)
}

static LOOSE: LazyLock<(tempfile::TempDir, Store)> = LazyLock::new(loose);
static PACKED: LazyLock<[(tempfile::TempDir, Store); 3]> =
    LazyLock::new(|| [packed(1), packed(4), packed(16)]);

/// The large packed store with `packs` packs.
fn packed_store(packs: u32) -> &'static Store {
    let index = match packs {
        1 => 0,
        4 => 1,
        16 => 2,
        _ => unreachable!("no fixture with {packs} packs"),
    };
    &PACKED[index].1
}

/// Writing a million statements as loose objects, in one batch.
#[divan::bench(sample_count = 3, sample_size = 1)]
fn write_loose(bencher: Bencher<'_, '_>) {
    bencher
        .counter(ItemsCount::new(STATEMENTS))
        .with_inputs(store)
        .bench_local_refs(|(_, store)| store.write_all(&STATEMENTS_1M).unwrap());
}

/// Compacting a million loose statements into one pack: what packing costs
/// on top of writing them loose.
#[divan::bench(sample_count = 3, sample_size = 1)]
fn compact(bencher: Bencher<'_, '_>) {
    bencher
        .counter(ItemsCount::new(STATEMENTS))
        .with_inputs(loose)
        .bench_local_refs(|(_, store)| store.compact().unwrap());
}

/// Reading back every statement of a million, in id order, from one
/// thread.
#[divan::bench(args = ["loose", "packed"], sample_count = 3, sample_size = 1)]
fn read_all(bencher: Bencher<'_, '_>, layout: &str) {
    let store = match layout {
        "loose" => &LOOSE.1,
        _ => packed_store(1),
    };
    let mut ids: Vec<ObjectId> = STATEMENTS_1M.iter().map(EncodedObject::id).collect();
    ids.sort_unstable();
    bencher.counter(ItemsCount::new(STATEMENTS)).bench(|| {
        for &id in &ids {
            black_box(store.read(id).unwrap());
        }
    });
}

/// Reading one statement of a million, loose or in the smallest of
/// `packs` packs, which is searched last.
#[divan::bench(args = [0, 1, 4, 16])]
fn read_one(bencher: Bencher<'_, '_>, packs: u32) {
    let store = if packs == 0 {
        &LOOSE.1
    } else {
        packed_store(packs)
    };
    let id = STATEMENTS_1M[STATEMENTS - 1].id();
    store.read(id).unwrap();
    bencher.bench(|| store.read(black_box(id)).unwrap());
}

/// Looking up an object the store doesn't have, among a million statements
/// loose or in `packs` packs: every pack is searched and the manifest
/// reread.
#[divan::bench(args = [0, 1, 4, 16])]
fn contains_absent(bencher: Bencher<'_, '_>, packs: u32) {
    let store = if packs == 0 {
        &LOOSE.1
    } else {
        packed_store(packs)
    };
    let id = EncodedObject::blob(b"absent").id();
    bencher.bench(|| assert!(!store.contains(black_box(id)).unwrap()));
}
