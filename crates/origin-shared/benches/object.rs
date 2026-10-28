//! Encoding, hashing and signing throughput of the object model.

use divan::counter::BytesCount;
use divan::{Bencher, black_box};
use origin_shared::{
    AcceptanceBody, Base, ChangeId, Digest, EncodedObject, EntryKind, EntryName, Inputs, Kind,
    NodeKey, Object, ObjectId, Record, RecordId, RepoPath, Revision, Source, Statement, TargetName,
    Tree, TreeEntry, Verdict,
};

fn main() {
    divan::main();
}

/// Hashing a blob: the cost of snapshotting a changed file.
#[divan::bench(args = [4 * 1024, 1024 * 1024, 64 * 1024 * 1024])]
fn hash_blob(bencher: Bencher<'_, '_>, len: usize) {
    let content = vec![0xa5_u8; len];
    bencher
        .counter(BytesCount::new(len))
        .bench(|| EncodedObject::blob(black_box(&content)));
}

fn tree(len: usize) -> Tree {
    (0..len)
        .map(|i| {
            let name = EntryName::new(format!("file-{i:06}.rs")).unwrap();
            let id = EncodedObject::blob(&i.to_le_bytes()).id();
            let entry = TreeEntry {
                kind: EntryKind::File,
                id,
            };
            (name, entry)
        })
        .collect()
}

/// Encoding and hashing a directory listing.
#[divan::bench(args = [16, 1024])]
fn encode_tree(bencher: Bencher<'_, '_>, len: usize) {
    let tree = Object::Tree(tree(len));
    bencher.bench(|| black_box(&tree).encode());
}

/// Decoding a directory listing: the cost of every tree read.
#[divan::bench(args = [16, 1024])]
fn decode_tree(bencher: Bencher<'_, '_>, len: usize) {
    let encoded = Object::Tree(tree(len)).encode();
    bencher.bench(|| black_box(&encoded).decode_as::<Tree>().unwrap());
}

/// An id of any type, from a seed.
fn id<T: From<ObjectId>>(seed: &str) -> T {
    EncodedObject::blob(seed.as_bytes()).id().into()
}

/// A CI result with declared inputs: the most common record.
fn statement() -> Statement {
    Statement {
        subject: RecordId::Revision(id("revision")),
        kind: Kind::new("ci.test").unwrap(),
        verdict: Some(Verdict::Pass),
        issuer: "node:ci-1".parse().unwrap(),
        inputs: Some(Inputs {
            paths: ["src", "Cargo.toml", "Cargo.lock"]
                .map(|path| RepoPath::new(path).unwrap())
                .into(),
            definition: Digest::of(b"cargo test"),
            toolchain: Digest::of(b"rustc 1.99.0"),
            external: false,
        }),
        payload: Some(id("log")),
        supersedes: None,
    }
}

/// Encoding and hashing a statement.
#[divan::bench]
fn encode_statement(bencher: Bencher<'_, '_>) {
    let statement = statement();
    bencher.bench(|| black_box(&statement).encode());
}

/// Decoding a statement: the cost of every statement read.
#[divan::bench]
fn decode_statement(bencher: Bencher<'_, '_>) {
    let encoded = statement().encode();
    bencher.bench(|| black_box(&encoded).decode_as::<Statement>().unwrap());
}

/// Encoding and hashing a revision.
#[divan::bench]
fn encode_revision(bencher: Bencher<'_, '_>) {
    let revision = Revision {
        change: ChangeId::from_bytes([7; 16]),
        predecessors: vec![id("old")],
        base: Base::Acceptance(id("main")),
        deps: Vec::new(),
        snapshot: id("tree"),
        description: id("description"),
        author: "user:alice".parse().unwrap(),
    };
    bencher.bench(|| black_box(&revision).encode());
}

fn acceptance() -> AcceptanceBody {
    AcceptanceBody {
        target: TargetName::MAIN.id(),
        seq: 41,
        prev: Some(id("prev")),
        state: id("tree"),
        landed: vec![Source::Revision(id("revision"))],
        evidence: vec![statement().encode().id().into()],
        policy: Some(id("policy")),
    }
}

/// Signing an acceptance: once per landing, by the target's authority.
#[divan::bench]
fn sign_acceptance(bencher: Bencher<'_, '_>) {
    let key = NodeKey::from_bytes(&[42; 32]);
    let body = acceptance();
    bencher
        .with_inputs(|| body.clone())
        .bench_values(|body| body.sign(&key));
}

/// Verifying an acceptance: once per acceptance of every chain fetched.
#[divan::bench]
fn verify_acceptance(bencher: Bencher<'_, '_>) {
    let key = NodeKey::from_bytes(&[42; 32]);
    let public = key.public();
    let acceptance = acceptance().sign(&key);
    bencher.bench(|| black_box(&acceptance).verify(&public).unwrap());
}
