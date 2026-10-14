use std::fmt;
use std::str::FromStr;

/// Identifies an object by the BLAKE3 hash of its encoding.
///
/// Ids are only produced by hashing ([`crate::EncodedObject`]) or by parsing
/// one that was produced that way, so an id always names some encoding.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId([u8; Self::LEN]);

impl ObjectId {
    /// Length of an id in bytes.
    pub const LEN: usize = 32;
    /// Number of hex digits [`ObjectId::short`] displays.
    pub const SHORT_LEN: usize = 12;

    /// Hashes an object encoding.
    pub(crate) fn hash(encoded: &[u8]) -> Self {
        Self(*blake3::hash(encoded).as_bytes())
    }

    /// Rebuilds an id from its raw bytes, as read from a binary format.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; Self::LEN]) -> Self {
        Self(bytes)
    }

    /// The raw bytes of the id.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; Self::LEN] {
        &self.0
    }

    /// The full id as lowercase hex.
    #[must_use]
    pub fn to_hex(&self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
    }

    /// An abbreviated id for display to humans.
    #[must_use]
    pub fn short(&self) -> String {
        let mut hex = self.to_hex();
        hex.truncate(Self::SHORT_LEN);
        hex
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&blake3::Hash::from_bytes(self.0).to_hex())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({self})")
    }
}

/// A string that is not a 64 digit hex object id.
#[derive(Debug, thiserror::Error)]
#[error("invalid object id `{input}`")]
pub struct ParseObjectIdError {
    input: String,
    #[source]
    source: blake3::HexError,
}

impl FromStr for ObjectId {
    type Err = ParseObjectIdError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        blake3::Hash::from_hex(input)
            .map(|hash| Self(*hash.as_bytes()))
            .map_err(|source| ParseObjectIdError {
                input: input.to_owned(),
                source,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let id = ObjectId::hash(b"hello");
        assert_eq!(id.to_hex().parse::<ObjectId>().unwrap(), id);
        assert_eq!(id.short().len(), ObjectId::SHORT_LEN);
        assert!(id.to_hex().starts_with(&id.short()));
    }

    #[test]
    fn rejects_invalid_hex() {
        assert!("abc".parse::<ObjectId>().is_err());
        assert!("zz".repeat(32).parse::<ObjectId>().is_err());
    }
}
