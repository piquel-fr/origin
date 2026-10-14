use super::ObjectKind;
use crate::ObjectId;

/// Bytes that do not hash to the id they were claimed to have.
#[derive(Debug, thiserror::Error)]
#[error("object {expected} is corrupt: its content hashes to {actual}")]
pub struct IdMismatch {
    /// The claimed id.
    pub expected: ObjectId,
    /// The hash of the bytes.
    pub actual: ObjectId,
}

/// Bytes that are not a canonical object encoding.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The encoding ended early.
    #[error("object encoding is truncated")]
    Truncated,
    /// Bytes remained after the object was decoded.
    #[error("object encoding has trailing bytes")]
    TrailingBytes,
    /// The first byte is not an object kind.
    #[error("unknown object kind {0}")]
    UnknownObjectKind(u8),
    /// The object is not of the expected kind.
    #[error("expected a {expected}, found a {actual}")]
    WrongKind {
        /// The kind the caller asked for.
        expected: ObjectKind,
        /// The kind of the object.
        actual: ObjectKind,
    },
    /// A tree entry has an unknown kind.
    #[error("unknown tree entry kind {0}")]
    UnknownEntryKind(u8),
    /// A tree entry has an invalid name.
    #[error(transparent)]
    InvalidName(#[from] InvalidEntryName),
    /// Tree entries are not strictly sorted by name.
    #[error("tree entries are not sorted")]
    UnsortedTree,
    /// A commit's time or time zone offset is out of range.
    #[error("commit time is out of range")]
    InvalidTime,
    /// A string is not UTF-8.
    #[error("object contains invalid UTF-8")]
    InvalidUtf8,
}

/// A tree entry name that [`EntryName::new`] rejected.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid file name {0:?}")]
pub struct InvalidEntryName(pub String);
