use super::ObjectKind;
use crate::{InvalidName, ObjectId};

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
    /// A tag byte (an enum variant, an option or a boolean) has no meaning.
    #[error("unknown {field} tag {tag}")]
    UnknownTag {
        /// What the tag encodes.
        field: &'static str,
        /// The tag byte.
        tag: u8,
    },
    /// A name is invalid.
    #[error(transparent)]
    InvalidName(#[from] InvalidName),
    /// A value is valid but not in the one form its type encodes it in.
    #[error("{0} is not in canonical form")]
    NonCanonical(&'static str),
    /// Tree entries are not strictly sorted by name.
    #[error("tree entries are not sorted")]
    UnsortedTree,
    /// A string is not UTF-8.
    #[error("object contains invalid UTF-8")]
    InvalidUtf8,
}
