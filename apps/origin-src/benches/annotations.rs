//! Statements: input digests over a large snapshot, and the promotion
//! search over many statements, with their digests cached by the index and
//! without.

use std::fs;

use divan::Bencher;
use divan::counter::ItemsCount;
use origin_platform::Native;
use origin_shared::{
    Base, ChangeId, Digest, EncodedObject, EntryKind, EntryName, Inputs, Kind, ObjectId, Principal,
    Record, RecordId, RepoPath, Revision, RevisionId, Statement, Tree, TreeEntry, TreeId, Verdict,
};
use origin_src::annotations::Annotations;
use origin_src::fixture::FixtureTree;
use origin_src::index::Index;
use origin_src::store::Store;
use origin_src::tree::Snapshots;
use origin_src::worktree::{StatCache, Worktree};

fn main() {
    divan::main();
}

/// An initialized store in a new temporary directory.
fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join(".origin"), Native::default());
    store.init().unwrap();
    (dir, store)
}

fn inputs(paths: &[&str], definition: &str) -> Inputs {
    Inputs {
        paths: paths
            .iter()
            .map(|path| RepoPath::new(path).unwrap())
            .collect(),
        definition: Digest::of(definition.as_bytes()),
        toolchain: Digest::of(b"rustc 1.99.0"),
        external: false,
    }
}

/// The input digest of the 20k-file fixture, for inputs naming two
/// directories, a file and a missing path. Each run reads the trees anew.
#[divan::bench(sample_count = 50)]
fn input_digest(bencher: Bencher<'_, '_>) {
    let dir = tempfile::tempdir().unwrap();
    FixtureTree::LARGE.write(dir.path()).unwrap();
    let store = Store::new(dir.path().join(".origin"), Native::default());
    store.init().unwrap();
    let (snapshot, _) = Worktree::new(dir.path(), &store)
        .snapshot(&StatCache::default())
        .unwrap();
    let inputs = inputs(
        &["dir0", "dir5/sub3", "dir12/sub7/file7820.rs", "missing"],
        "cargo test",
    );
    bencher.bench(|| {
        Snapshots::new(&store)
            .input_digest(snapshot, &inputs)
            .unwrap()
    });
}

/// How many statements the promotion search runs over.
const STATEMENTS: usize = 20_000;

/// The checks each revision has results of, with the inputs they declare.
const CHECKS: [(&str, &[&str]); 10] = [
    ("ci.lint", &["src", "Cargo.toml"]),
    ("ci.build", &["src", "Cargo.toml"]),
    ("ci.test", &["src", "tests", "Cargo.toml"]),
    ("ci.docs", &["src", "docs"]),
    ("ci.links", &["docs"]),
    ("ci.fmt", &["src", "tests"]),
    ("ci.bench", &["src", "benches", "Cargo.toml"]),
    ("ci.audit", &["Cargo.toml"]),
    ("ci.spell", &["docs", "README.md"]),
    ("ci.unit", &["src"]),
];

/// A snapshot of a small crate, at version `version` of its sources and
/// `docs` of its documentation.
fn snapshot(version: usize, docs: usize) -> Vec<EncodedObject> {
    let file = |content: String| {
        let blob = EncodedObject::blob(content.as_bytes());
        let entry = TreeEntry {
            kind: EntryKind::File,
            id: blob.id(),
        };
        (blob, entry)
    };
    let mut objects = Vec::new();
    let mut root = Vec::new();
    for (dir, content) in [
        ("src", format!("fn v{version}() {{}}")),
        ("tests", format!("#[test] fn t{}() {{}}", version / 2)),
        ("benches", "fn bench() {}".to_owned()),
        ("docs", format!("# Guide {docs}")),
    ] {
        let (blob, entry) = file(content);
        let tree: Tree = [(EntryName::new("a").unwrap(), entry)]
            .into_iter()
            .collect();
        let tree = tree.encode();
        root.push((
            EntryName::new(dir).unwrap(),
            TreeEntry {
                kind: EntryKind::Tree,
                id: tree.id(),
            },
        ));
        objects.extend([blob, tree]);
    }
    for (name, content) in [("Cargo.toml", "[package]"), ("README.md", "# Readme")] {
        let (blob, entry) = file(content.to_owned());
        root.push((EntryName::new(name).unwrap(), entry));
        objects.push(blob);
    }
    objects.push(root.into_iter().collect::<Tree>().encode());
    objects
}

/// A store with a revision of its own change for each version, each with a
/// result of every check, 20k statements in all, and a revision whose
/// sources match one of them.
fn promotion_store() -> (tempfile::TempDir, Store, RevisionId) {
    let (dir, store) = store();
    let revision = |store: &Store, change: usize, version: usize, docs: usize| {
        let objects = snapshot(version, docs);
        let root = TreeId::from(objects.last().unwrap().id());
        store.write_all(&objects).unwrap();
        let mut id = [0; 16];
        id[..8].copy_from_slice(&(change as u64).to_le_bytes());
        let encoded = Revision {
            change: ChangeId::from_bytes(id),
            predecessors: Vec::new(),
            base: Base::Acceptance(ObjectId::from_bytes([0; 32]).into()),
            deps: Vec::new(),
            snapshot: root,
            description: EncodedObject::blob(b"").id().into(),
            author: "user:bench".parse().unwrap(),
        }
        .encode();
        store.write_all(std::slice::from_ref(&encoded)).unwrap();
        RevisionId::from(encoded.id())
    };
    let revisions = STATEMENTS / CHECKS.len();
    let mut statements = Vec::new();
    for version in 0..revisions {
        let revision = revision(&store, version, version, version % 7);
        statements.extend(CHECKS.map(|(kind, paths)| {
            Statement {
                subject: RecordId::Revision(revision),
                kind: Kind::new(kind).unwrap(),
                verdict: Some(Verdict::Pass),
                issuer: "node:ci-1".parse().unwrap(),
                inputs: Some(inputs(paths, kind)),
                payload: None,
                supersedes: None,
                promoted_from: None,
            }
            .encode()
        }));
    }
    store.write_all(&statements).unwrap();
    // Version 1000's sources with the docs of other versions: the seven
    // checks that don't read docs promote from version 1000, the two that
    // only read docs from the latest version with those docs.
    let target = revision(&store, revisions, 1000, 3);
    (dir, store, target)
}

/// Searching 20k statements for those to promote to a revision, with every
/// digest cached: the cost of each search after the first.
#[divan::bench(sample_count = 20)]
fn promotions_cached(bencher: Bencher<'_, '_>) {
    let (_dir, store, target) = promotion_store();
    let issuer: Principal = "node:queue".parse().unwrap();
    let mut annotations = Annotations::open(&store).unwrap();
    assert_eq!(
        annotations
            .promotions(RecordId::Revision(target), &issuer, |_| Ok(true))
            .unwrap()
            .len(),
        9
    );
    let index = Index::open(&store).unwrap();
    bencher
        .counter(ItemsCount::new(STATEMENTS))
        .with_inputs(|| Annotations::new(&store, index.clone()))
        .bench_local_refs(|annotations| {
            annotations
                .promotions(RecordId::Revision(target), &issuer, |_| Ok(true))
                .unwrap()
        });
}

/// The first search, which computes and remembers the digest of each of
/// the 20k statements, reading its subject's trees.
#[divan::bench(sample_count = 10, sample_size = 1)]
fn promotions_uncached(bencher: Bencher<'_, '_>) {
    let (dir, store, target) = promotion_store();
    let issuer: Principal = "node:queue".parse().unwrap();
    let log = dir.path().join(".origin/index/log");
    bencher
        .counter(ItemsCount::new(STATEMENTS))
        .with_inputs(|| {
            // Rebuilding without the old index forgets every digest.
            fs::remove_file(&log).unwrap();
            Annotations::open(&store).unwrap()
        })
        .bench_local_refs(|annotations| {
            annotations
                .promotions(RecordId::Revision(target), &issuer, |_| Ok(true))
                .unwrap()
        });
}
