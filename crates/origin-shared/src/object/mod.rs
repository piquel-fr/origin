//! The object model: blobs, trees, and commits, and their canonical encoding.
//!
//! Every object has exactly one encoding, and its id is the BLAKE3 hash of
//! that encoding. The first byte is the [`ObjectKind`], so objects of
//! different kinds never share an id. All integers are little endian.
//!
//! - Blob: the file content.
//! - Tree: entries sorted by name, each `kind:u8 name:bytes 0x00 id:[u8; 32]`.
//! - Commit: `tree:[u8; 32] parents:u32 parent:[u8; 32]...`, then the author
//!   (`name` and `email` as `len:u32 bytes`, `timestamp:i64` in seconds,
//!   `offset:i32` in seconds east of UTC), then the message as the
//!   remaining bytes.

use std::fmt;
use std::io::{self, Read};

use crate::ObjectId;

pub use commit::{Commit, Signature};
pub use errors::{DecodeError, IdMismatch, InvalidEntryName};
pub use tree::{EntryKind, EntryName, Tree, TreeEntry};

mod commit;
mod errors;
mod tree;

/// The kind of an object, stored as the first byte of its encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ObjectKind {
    /// File contents.
    Blob = 1,
    /// A directory listing.
    Tree = 2,
    /// A snapshot of a tree with its history.
    Commit = 3,
}

impl ObjectKind {
    fn from_byte(byte: u8) -> Result<Self, DecodeError> {
        match byte {
            1 => Ok(Self::Blob),
            2 => Ok(Self::Tree),
            3 => Ok(Self::Commit),
            _ => Err(DecodeError::UnknownObjectKind(byte)),
        }
    }
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Commit => "commit",
        })
    }
}

/// A decoded object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Object {
    /// File contents.
    Blob(Vec<u8>),
    /// A directory listing.
    Tree(Tree),
    /// A snapshot of a tree with its history.
    Commit(Commit),
}

impl Object {
    /// The kind of this object.
    #[must_use]
    pub fn kind(&self) -> ObjectKind {
        match self {
            Self::Blob(_) => ObjectKind::Blob,
            Self::Tree(_) => ObjectKind::Tree,
            Self::Commit(_) => ObjectKind::Commit,
        }
    }

    /// Encodes and hashes the object.
    #[must_use]
    pub fn encode(&self) -> EncodedObject {
        let mut out = vec![self.kind() as u8];
        match self {
            Self::Blob(content) => out.extend_from_slice(content),
            Self::Tree(tree) => tree.encode_into(&mut out),
            Self::Commit(commit) => commit.encode_into(&mut out),
        }
        EncodedObject::hash(out)
    }
}

impl From<Tree> for Object {
    fn from(tree: Tree) -> Self {
        Self::Tree(tree)
    }
}

impl From<Commit> for Object {
    fn from(commit: Commit) -> Self {
        Self::Commit(commit)
    }
}

/// An object's encoding together with its id.
///
/// The id is always the hash of the bytes: constructors either hash the bytes
/// or verify a claimed id, so holding an `EncodedObject` proves integrity.
#[derive(Clone, PartialEq, Eq)]
pub struct EncodedObject {
    id: ObjectId,
    bytes: Vec<u8>,
}

impl EncodedObject {
    fn hash(bytes: Vec<u8>) -> Self {
        Self {
            id: ObjectId::hash(&bytes),
            bytes,
        }
    }

    /// Encodes a blob, reading its content to the end. `len_hint` sizes the
    /// buffer up front, so passing the file size avoids reallocating.
    ///
    /// # Errors
    ///
    /// Fails if reading fails.
    pub fn read_blob(mut reader: impl Read, len_hint: usize) -> io::Result<Self> {
        let mut bytes = Vec::with_capacity(len_hint + 1);
        bytes.push(ObjectKind::Blob as u8);
        reader.read_to_end(&mut bytes)?;
        Ok(Self::hash(bytes))
    }

    /// Encodes a blob from its content.
    #[must_use]
    pub fn blob(content: &[u8]) -> Self {
        let mut bytes = Vec::with_capacity(content.len() + 1);
        bytes.push(ObjectKind::Blob as u8);
        bytes.extend_from_slice(content);
        Self::hash(bytes)
    }

    /// Accepts bytes claimed to encode the object `id`, such as bytes read
    /// from disk or the network.
    ///
    /// # Errors
    ///
    /// Fails if the bytes do not hash to `id`.
    pub fn verify(id: ObjectId, bytes: Vec<u8>) -> Result<Self, IdMismatch> {
        let actual = ObjectId::hash(&bytes);
        if actual == id {
            Ok(Self { id, bytes })
        } else {
            Err(IdMismatch {
                expected: id,
                actual,
            })
        }
    }

    /// The id of the object.
    #[must_use]
    pub fn id(&self) -> ObjectId {
        self.id
    }

    /// The canonical encoding.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The kind of the object.
    ///
    /// # Errors
    ///
    /// Fails if the encoding does not start with a known kind.
    pub fn kind(&self) -> Result<ObjectKind, DecodeError> {
        ObjectKind::from_byte(*self.bytes.first().ok_or(DecodeError::Truncated)?)
    }

    /// A blob's content, without copying it.
    ///
    /// # Errors
    ///
    /// Fails if the object is not a blob.
    pub fn blob_content(&self) -> Result<&[u8], DecodeError> {
        self.expect_kind(ObjectKind::Blob)?;
        Ok(&self.bytes[1..])
    }

    /// Decodes the object.
    ///
    /// # Errors
    ///
    /// Fails if the bytes are not a canonical encoding.
    pub fn decode(&self) -> Result<Object, DecodeError> {
        let kind = self.kind()?;
        let mut reader = Reader(&self.bytes[1..]);
        let object = match kind {
            ObjectKind::Blob => return Ok(Object::Blob(self.bytes[1..].to_vec())),
            ObjectKind::Tree => Object::Tree(Tree::decode(&mut reader)?),
            ObjectKind::Commit => Object::Commit(Commit::decode(&mut reader)?),
        };
        reader.finish()?;
        Ok(object)
    }

    /// Decodes the object, which must be a tree.
    ///
    /// # Errors
    ///
    /// Fails if it is not a tree or is not a canonical encoding.
    pub fn decode_tree(&self) -> Result<Tree, DecodeError> {
        self.expect_kind(ObjectKind::Tree)?;
        let mut reader = Reader(&self.bytes[1..]);
        let tree = Tree::decode(&mut reader)?;
        reader.finish()?;
        Ok(tree)
    }

    /// Decodes the object, which must be a commit.
    ///
    /// # Errors
    ///
    /// Fails if it is not a commit or is not a canonical encoding.
    pub fn decode_commit(&self) -> Result<Commit, DecodeError> {
        self.expect_kind(ObjectKind::Commit)?;
        let mut reader = Reader(&self.bytes[1..]);
        let commit = Commit::decode(&mut reader)?;
        reader.finish()?;
        Ok(commit)
    }

    fn expect_kind(&self, expected: ObjectKind) -> Result<(), DecodeError> {
        let actual = self.kind()?;
        if actual == expected {
            Ok(())
        } else {
            Err(DecodeError::WrongKind { expected, actual })
        }
    }

    /// Consumes the object, returning its encoding.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl fmt::Debug for EncodedObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncodedObject")
            .field("id", &self.id)
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// A cursor over an encoding that fails with [`DecodeError::Truncated`]
/// instead of panicking.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        if self.0.len() < len {
            return Err(DecodeError::Truncated);
        }
        let (head, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        Ok(self.take(N)?.try_into().expect("took exactly N bytes"))
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.array::<1>()?[0])
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn id(&mut self) -> Result<ObjectId, DecodeError> {
        Ok(ObjectId::from_bytes(self.array()?))
    }

    fn str_until_nul(&mut self) -> Result<String, DecodeError> {
        let len = self
            .0
            .iter()
            .position(|&byte| byte == 0)
            .ok_or(DecodeError::Truncated)?;
        let name = self.utf8(len)?;
        self.take(1)?;
        Ok(name)
    }

    fn prefixed_str(&mut self) -> Result<String, DecodeError> {
        let len = self.u32()? as usize;
        self.utf8(len)
    }

    fn utf8(&mut self, len: usize) -> Result<String, DecodeError> {
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::InvalidUtf8)
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }

    fn finish(self) -> Result<(), DecodeError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use jiff::tz::Offset;

    use super::*;

    /// A tree with an entry of each kind, also used by the tree tests.
    pub(super) fn sample_tree() -> Tree {
        [
            ("b.txt", EntryKind::File, EncodedObject::blob(b"b").id()),
            (
                "a",
                EntryKind::Tree,
                Object::Tree(Tree::default()).encode().id(),
            ),
            (
                "run.sh",
                EntryKind::Executable,
                EncodedObject::blob(b"#!").id(),
            ),
        ]
        .into_iter()
        .map(|(name, kind, id)| (EntryName::new(name).unwrap(), TreeEntry { kind, id }))
        .collect()
    }

    fn sample_commit() -> Commit {
        Commit {
            tree: Object::Tree(sample_tree()).encode().id(),
            parents: vec![EncodedObject::blob(b"parent").id()],
            author: Signature {
                name: "Ada".into(),
                email: "ada@example.com".into(),
                timestamp: Timestamp::from_second(1_700_000_000).unwrap(),
                offset: Offset::from_seconds(2 * 3600).unwrap(),
            },
            message: "Add things\n\nWith a body.".into(),
        }
    }

    #[test]
    fn objects_round_trip() {
        for object in [
            Object::Blob(b"hello\n".to_vec()),
            Object::Blob(Vec::new()),
            Object::Tree(Tree::default()),
            Object::Tree(sample_tree()),
            Object::Commit(sample_commit()),
        ] {
            let encoded = object.encode();
            assert_eq!(encoded.decode().unwrap(), object);
            let verified = EncodedObject::verify(encoded.id(), encoded.bytes().to_vec()).unwrap();
            assert_eq!(verified, encoded);
        }
    }

    /// Ids are the storage and wire format: changing the encoding changes
    /// every id, so it must never happen by accident.
    #[test]
    fn encoding_is_stable() {
        assert_eq!(
            EncodedObject::blob(b"").id().to_hex(),
            "48fc721fbbc172e0925fa27af1671de225ba927134802998b10a1568a188652b"
        );
        assert_eq!(
            Object::Commit(sample_commit()).encode().id().to_hex(),
            "9a41c0f3d89062bc1e8de74707a812bee196c73b36928f8f9e98ca2e559ba948"
        );
    }

    #[test]
    fn blob_from_reader_matches_blob() {
        let content = b"some file content";
        let read = EncodedObject::read_blob(&content[..], content.len()).unwrap();
        assert_eq!(read, EncodedObject::blob(content));
        assert_eq!(read.blob_content().unwrap(), content);
    }

    #[test]
    fn empty_encodings_are_errors() {
        let empty = EncodedObject::hash(Vec::new());
        assert_eq!(empty.decode(), Err(DecodeError::Truncated));
        assert_eq!(empty.blob_content(), Err(DecodeError::Truncated));
    }

    #[test]
    fn verify_rejects_wrong_id() {
        let blob = EncodedObject::blob(b"a");
        let other = EncodedObject::blob(b"b");
        assert!(EncodedObject::verify(blob.id(), other.into_bytes()).is_err());
    }

    #[test]
    fn typed_decoders_check_the_kind() {
        let blob = EncodedObject::blob(b"x");
        assert_eq!(
            blob.decode_tree(),
            Err(DecodeError::WrongKind {
                expected: ObjectKind::Tree,
                actual: ObjectKind::Blob,
            })
        );
        assert!(
            Object::Commit(sample_commit())
                .encode()
                .decode_commit()
                .is_ok()
        );
    }
}
