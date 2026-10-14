//! Encoding and hashing throughput of the object model.

use divan::counter::BytesCount;
use divan::{Bencher, black_box};
use origin_shared::{EncodedObject, EntryKind, EntryName, Object, Tree, TreeEntry};

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
    bencher.bench(|| black_box(&encoded).decode_tree().unwrap());
}
