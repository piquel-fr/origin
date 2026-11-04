use std::fmt;
use std::str::FromStr;

use crate::object::{Acceptance, DecodeError, Field, Reader, Record, Revision, Statement, Tree};

/// Defines fixed-size ids that display as lowercase hex.
macro_rules! ids {
    ($($(#[$meta:meta])* $name:ident($len:literal);)*) => {$(
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name([u8; $len]);

        impl $name {
            /// Length of the id in bytes.
            pub const LEN: usize = $len;

            /// Rebuilds an id from its raw bytes, as read from a binary format.
            #[must_use]
            pub const fn from_bytes(bytes: [u8; $len]) -> Self {
                Self(bytes)
            }

            /// The raw bytes of the id.
            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }

            /// The full id as lowercase hex.
            #[must_use]
            pub fn to_hex(&self) -> String {
                hex::string(&self.0)
            }

            /// An abbreviated id for display to humans.
            #[must_use]
            pub fn short(&self) -> String {
                let mut hex = self.to_hex();
                hex.truncate(hex::SHORT_LEN);
                hex
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_hex())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self)
            }
        }

        impl FromStr for $name {
            type Err = ParseIdError;

            fn from_str(input: &str) -> Result<Self, Self::Err> {
                hex::decode(input).map(Self).ok_or_else(|| ParseIdError {
                    what: stringify!($name),
                    input: input.to_owned(),
                })
            }
        }

        impl Field for $name {
            fn write(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.0);
            }

            fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
                reader.array().map(Self)
            }
        }
    )*};
}

/// Defines ids of objects of one kind. They convert to and from
/// [`ObjectId`]: converting from one vouches for the kind, which reading the
/// object checks.
macro_rules! object_ids {
    ($($(#[$meta:meta])* $name:ident $(=> $record:ty)?;)*) => {
        ids! { $($(#[$meta])* $name(32);)* }
        $(
            impl From<ObjectId> for $name {
                fn from(id: ObjectId) -> Self {
                    Self(id.0)
                }
            }

            impl From<$name> for ObjectId {
                fn from(id: $name) -> Self {
                    Self(id.0)
                }
            }

            $(impl TypedId for $name {
                type Record = $record;
            })?
        )*
    };
}

ids! {
    /// Identifies an object by the BLAKE3 hash of its encoding.
    ///
    /// The typed ids ([`TreeId`], [`RevisionId`], ...) name objects of one
    /// kind, and convert to and from this one.
    ObjectId(32);

    /// Identifies a change across all its revisions: random, so two changes
    /// never share one, whoever creates them and wherever.
    ChangeId(16);

    /// Identifies a target: the hash of its name, so user-chosen names never
    /// become file names.
    TargetId(32);

    /// A BLAKE3 hash of something that isn't an object, such as a check's
    /// definition or toolchain.
    Digest(32);
}

object_ids! {
    /// Identifies a blob: file contents, a description or a payload.
    BlobId;
    /// Identifies a tree, the root of which is a snapshot.
    TreeId => Tree;
    /// Identifies a revision.
    RevisionId => Revision;
    /// Identifies a statement.
    StatementId => Statement;
    /// Identifies an acceptance.
    AcceptanceId => Acceptance;
}

/// The id of a record of one type, which reading the record returns.
pub trait TypedId: Copy + Into<ObjectId> {
    /// The record the id names.
    type Record: Record;
}

impl ObjectId {
    /// Hashes an object encoding.
    pub(crate) fn hash(encoded: &[u8]) -> Self {
        Self(*blake3::hash(encoded).as_bytes())
    }
}

impl ChangeId {
    /// A new, random change id.
    ///
    /// # Errors
    ///
    /// Fails if the operating system has no randomness to give.
    pub fn random() -> Result<Self, getrandom::Error> {
        let mut bytes = [0; Self::LEN];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }
}

impl Digest {
    /// Hashes `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }
}

impl TargetId {
    /// The BLAKE3 key derivation context, which keeps target ids apart from
    /// every other hash.
    const CONTEXT: &str = "origin 2026-10 target name";

    /// The id of the target named `name`.
    pub(crate) fn of(name: &str) -> Self {
        Self(blake3::derive_key(Self::CONTEXT, name.as_bytes()))
    }
}

/// A string that is not a valid hex id.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid {what} `{input}`")]
pub struct ParseIdError {
    /// The type of id expected.
    pub what: &'static str,
    /// The string given.
    pub input: String,
}

/// Lowercase hex.
pub(crate) mod hex {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    /// Number of hex digits a short id has.
    pub const SHORT_LEN: usize = 12;

    /// `bytes` as a string of lowercase hex.
    pub fn string(bytes: &[u8]) -> String {
        let mut hex = vec![0; 2 * bytes.len()];
        encode(bytes, &mut hex);
        String::from_utf8(hex).expect("hex digits are ASCII")
    }

    /// Writes the two digits of each byte of `bytes` to `out`, which is
    /// twice as long.
    pub fn encode(bytes: &[u8], out: &mut [u8]) {
        for (byte, pair) in bytes.iter().zip(out.as_chunks_mut::<2>().0) {
            *pair = [
                DIGITS[usize::from(byte >> 4)],
                DIGITS[usize::from(byte & 0xf)],
            ];
        }
    }

    /// Parses exactly `2 * N` hex digits, in either case.
    pub fn decode<const N: usize>(input: &str) -> Option<[u8; N]> {
        let input = input.as_bytes();
        if input.len() != 2 * N {
            return None;
        }
        let mut bytes = [0; N];
        let digit = |c: u8| char::from(c).to_digit(16);
        for (byte, &[high, low]) in bytes.iter_mut().zip(input.as_chunks::<2>().0) {
            *byte = u8::try_from(digit(high)? << 4 | digit(low)?).ok()?;
        }
        Some(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let id = ObjectId::hash(b"hello");
        assert_eq!(id.to_hex().parse::<ObjectId>().unwrap(), id);
        assert_eq!(id.to_hex().to_uppercase().parse::<ObjectId>().unwrap(), id);
        assert_eq!(id.short().len(), hex::SHORT_LEN);
        assert!(id.to_hex().starts_with(&id.short()));

        let change = ChangeId::from_bytes([0xab; 16]);
        assert_eq!(change.to_string(), "ab".repeat(16));
        assert_eq!(change.to_string().parse::<ChangeId>().unwrap(), change);
    }

    #[test]
    fn rejects_invalid_hex() {
        for invalid in ["abc", &"zz".repeat(32), &"+f".repeat(32), &"é".repeat(32)] {
            assert!(invalid.parse::<ObjectId>().is_err(), "{invalid:?}");
        }
        assert!(
            "ab".repeat(32).parse::<ChangeId>().is_err(),
            "the length is the id's"
        );
    }

    #[test]
    fn random_change_ids_differ() {
        assert_ne!(ChangeId::random().unwrap(), ChangeId::random().unwrap());
    }
}
