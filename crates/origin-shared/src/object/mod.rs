//! The object model: blobs, trees and the records built on them, and their
//! canonical encoding.
//!
//! Every object has exactly one encoding, and its id is the BLAKE3 hash of
//! that encoding. The first byte is the [`ObjectKind`], which gives each kind
//! its own hash domain: objects of different kinds never share an id, and a
//! record of one kind can never be mistaken for another.
//!
//! - Blob: the content, as is.
//! - Tree: its entries, see [`Tree`].
//! - Revision, statement and acceptance: their fields in declaration order.
//!   Integers are little endian, ids their raw bytes, names and paths a
//!   `u32` length and UTF-8, lists a `u32` count and the items, options and
//!   enums a tag byte and the value. An acceptance ends with its 64-byte
//!   ed25519 signature of everything before it.

use std::fmt;
use std::io::{self, Read};

use crate::ObjectId;

pub use acceptance::{
    Acceptance, AcceptanceBody, BadSignature, NodeKey, NodePublicKey, Signature, Source,
};
pub use encoding::Reader;
pub(crate) use encoding::{Field, fields, tagged};
pub use errors::{DecodeError, IdMismatch};
pub use revision::{Base, Revision};
pub use statement::{Inputs, Kind, RecordId, Statement, Verdict};
pub use tree::{EntryKind, EntryName, Tree, TreeEntry};

mod acceptance;
mod encoding;
mod errors;
mod revision;
mod statement;
mod tree;

/// The kind of an object, stored as the first byte of its encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ObjectKind {
    /// File contents, a description or a payload.
    Blob = 1,
    /// A directory listing.
    Tree = 2,
    /// One version of a change.
    Revision = 3,
    /// A claim about another record.
    Statement = 4,
    /// One landing on a target.
    Acceptance = 5,
}

impl ObjectKind {
    fn from_byte(byte: u8) -> Result<Self, DecodeError> {
        match byte {
            1 => Ok(Self::Blob),
            2 => Ok(Self::Tree),
            3 => Ok(Self::Revision),
            4 => Ok(Self::Statement),
            5 => Ok(Self::Acceptance),
            _ => Err(DecodeError::UnknownObjectKind(byte)),
        }
    }
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Revision => "revision",
            Self::Statement => "statement",
            Self::Acceptance => "acceptance",
        })
    }
}

/// An object with structure: every kind but blobs.
pub trait Record: Field {
    /// The kind every encoding of this record starts with.
    const KIND: ObjectKind;

    /// Encodes and hashes the record.
    fn encode(&self) -> EncodedObject {
        let mut bytes = vec![Self::KIND as u8];
        self.write(&mut bytes);
        EncodedObject::hash(bytes)
    }
}

/// Defines [`Object`], with a variant for each record type.
macro_rules! object {
    ($($record:ident),*) => {
        /// A decoded object.
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub enum Object {
            /// File contents, a description or a payload.
            Blob(Vec<u8>),
            $(#[doc = concat!("A ", stringify!($record), ".")] $record($record),)*
        }

        impl Object {
            /// The kind of this object.
            #[must_use]
            pub fn kind(&self) -> ObjectKind {
                match self {
                    Self::Blob(_) => ObjectKind::Blob,
                    $(Self::$record(_) => $record::KIND,)*
                }
            }

            /// Encodes and hashes the object.
            #[must_use]
            pub fn encode(&self) -> EncodedObject {
                match self {
                    Self::Blob(content) => EncodedObject::blob(content),
                    $(Self::$record(record) => record.encode(),)*
                }
            }
        }

        impl EncodedObject {
            /// Decodes the object.
            ///
            /// # Errors
            ///
            /// Fails if the bytes are not a canonical encoding.
            pub fn decode(&self) -> Result<Object, DecodeError> {
                match self.kind()? {
                    ObjectKind::Blob => Ok(Object::Blob(self.bytes[1..].to_vec())),
                    $(ObjectKind::$record => self.decode_as().map(Object::$record),)*
                }
            }
        }

        $(impl From<$record> for Object {
            fn from(record: $record) -> Self {
                Self::$record(record)
            }
        })*
    };
}

object!(Tree, Revision, Statement, Acceptance);

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

    /// Decodes the object as a `R`.
    ///
    /// # Errors
    ///
    /// Fails if it is another kind of object or is not a canonical encoding.
    pub fn decode_as<R: Record>(&self) -> Result<R, DecodeError> {
        self.expect_kind(R::KIND)?;
        let mut reader = Reader::new(&self.bytes[1..]);
        let record = R::read(&mut reader)?;
        reader.finish()?;
        Ok(record)
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{ChangeId, Digest, RepoPath, TargetName, TreeId};

    /// A tree with an entry of each kind, also used by the tree tests.
    pub(crate) fn sample_tree() -> Tree {
        [
            ("b.txt", EntryKind::File, EncodedObject::blob(b"b").id()),
            ("a", EntryKind::Tree, Tree::default().encode().id()),
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

    fn id<T: From<ObjectId>>(seed: &str) -> T {
        EncodedObject::blob(seed.as_bytes()).id().into()
    }

    fn sample_revision() -> Revision {
        Revision {
            change: ChangeId::from_bytes([7; 16]),
            predecessors: vec![id("old")],
            base: Base::Acceptance(id("main")),
            deps: vec![id("dep-1"), id("dep-2")],
            snapshot: TreeId::from(sample_tree().encode().id()),
            description: id("Add retries to fetch"),
            author: "user:alice".parse().unwrap(),
        }
    }

    fn sample_statement() -> Statement {
        Statement {
            subject: RecordId::Revision(sample_revision().encode().id().into()),
            kind: Kind::new("ci.test").unwrap(),
            verdict: Some(Verdict::Pass),
            issuer: "node:ci-1".parse().unwrap(),
            inputs: Some(Inputs {
                paths: vec![RepoPath::new("src").unwrap(), RepoPath::root()],
                definition: Digest::of(b"cargo test"),
                toolchain: Digest::of(b"rustc 1.99.0"),
                external: false,
            }),
            payload: Some(id("log")),
            supersedes: None,
            promoted_from: Some(id("original")),
        }
    }

    /// A key that signs the same way in every run.
    pub(crate) fn sample_key() -> NodeKey {
        NodeKey::from_bytes(&[42; 32])
    }

    pub(crate) fn sample_acceptance() -> Acceptance {
        AcceptanceBody {
            target: TargetName::MAIN.id(),
            seq: 1,
            prev: Some(id("genesis")),
            state: TreeId::from(sample_tree().encode().id()),
            landed: vec![
                Source::Revision(sample_revision().encode().id().into()),
                Source::Target(id("overhaul")),
            ],
            evidence: vec![sample_statement().encode().id().into()],
            policy: Some(id("policy")),
        }
        .sign(&sample_key())
    }

    fn samples() -> [Object; 7] {
        [
            Object::Blob(b"hello\n".to_vec()),
            Object::Blob(Vec::new()),
            Object::Tree(Tree::default()),
            Object::Tree(sample_tree()),
            Object::Revision(sample_revision()),
            Object::Statement(sample_statement()),
            Object::Acceptance(sample_acceptance()),
        ]
    }

    #[test]
    fn objects_round_trip() {
        for object in samples() {
            let encoded = object.encode();
            assert_eq!(encoded.kind().unwrap(), object.kind());
            assert_eq!(encoded.decode().unwrap(), object);
            let verified = EncodedObject::verify(encoded.id(), encoded.bytes().to_vec()).unwrap();
            assert_eq!(verified, encoded);
        }
    }

    /// Ids are the storage and wire format: changing the encoding changes
    /// every id, so it must never happen by accident.
    #[test]
    fn encoding_is_stable() {
        let ids = samples().map(|object| object.encode().id().short());
        assert_eq!(
            ids,
            [
                "60200926b98f",
                "48fc721fbbc1",
                "ab13bedf42e8",
                "d06087ffcfd2",
                "8783b1fca530",
                "b4f235b57ce9",
                "f0442b2f9f6c",
            ]
        );
    }

    #[test]
    fn acceptances_verify_only_against_their_signer() {
        let acceptance = sample_acceptance();
        acceptance.verify(&sample_key().public()).unwrap();
        let other = NodeKey::from_bytes(&[1; 32]);
        assert!(acceptance.verify(&other.public()).is_err());
        let public = sample_key().public();
        assert_eq!(public.to_string().parse::<NodePublicKey>().unwrap(), public);
        assert!("ab".parse::<NodePublicKey>().is_err());

        // Any change to the body breaks the signature: here the sequence
        // number, right after the kind and the target id.
        let mut bytes = acceptance.encode().into_bytes();
        bytes[1 + 32] += 1;
        let tampered: Acceptance = EncodedObject::hash(bytes).decode_as().unwrap();
        assert_eq!(tampered.body().seq, 2);
        assert!(tampered.verify(&sample_key().public()).is_err());
    }

    #[test]
    fn decode_rejects_non_canonical_fields() {
        let encoded = sample_statement().encode();
        let decode = |edit: &dyn Fn(&mut Vec<u8>)| {
            let mut bytes = encoded.bytes().to_vec();
            edit(&mut bytes);
            EncodedObject::hash(bytes).decode_as::<Statement>()
        };
        // The subject's tag is the byte after the kind.
        assert_eq!(
            decode(&|bytes| bytes[1] = 9),
            Err(DecodeError::UnknownTag {
                field: "RecordId",
                tag: 9
            })
        );
        // `external` is the last byte before the payload, supersedes and
        // the promoted statement.
        let external = encoded.bytes().len() - 2 * (1 + ObjectId::LEN) - 1 - 1;
        assert_eq!(
            decode(&|bytes| bytes[external] = 2),
            Err(DecodeError::UnknownTag {
                field: "bool",
                tag: 2
            })
        );
        assert_eq!(
            decode(&|bytes| bytes.push(0)),
            Err(DecodeError::TrailingBytes)
        );
        assert_eq!(
            decode(&|bytes| {
                bytes.pop();
            }),
            Err(DecodeError::Truncated)
        );
    }

    #[test]
    fn paths_decode_only_in_canonical_form() {
        let mut bytes = Vec::new();
        Reader::write_str("/src/", &mut bytes);
        assert_eq!(
            RepoPath::read(&mut Reader::new(&bytes)),
            Err(DecodeError::NonCanonical("path"))
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
            blob.decode_as::<Tree>(),
            Err(DecodeError::WrongKind {
                expected: ObjectKind::Tree,
                actual: ObjectKind::Blob,
            })
        );
        let statement = sample_statement().encode();
        assert!(statement.decode_as::<Statement>().is_ok());
        assert!(statement.decode_as::<Revision>().is_err());
    }
}
