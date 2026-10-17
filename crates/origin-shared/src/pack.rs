//! The binary format objects travel in.
//!
//! A pack is a sequence of entries, each
//! `id:[u8; 32] len:u64 bytes:[u8; len]` with `bytes` the object's encoding.
//! Decoding verifies every object against its id, so a pack can come from an
//! untrusted peer.

use crate::object::{DecodeError, IdMismatch, Reader};
use crate::{EncodedObject, ObjectId};

/// A batch of objects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pack {
    /// The objects, in the order they were added.
    pub objects: Vec<EncodedObject>,
}

impl Pack {
    const HEADER_LEN: usize = ObjectId::LEN + 8;

    /// Encodes the pack.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let len = self
            .objects
            .iter()
            .map(|object| Self::HEADER_LEN + object.bytes().len())
            .sum();
        let mut out = Vec::with_capacity(len);
        for object in &self.objects {
            out.extend_from_slice(object.id().as_bytes());
            out.extend_from_slice(&(object.bytes().len() as u64).to_le_bytes());
            out.extend_from_slice(object.bytes());
        }
        out
    }

    /// Decodes and verifies a pack.
    ///
    /// # Errors
    ///
    /// Fails if the pack is truncated or an object does not match its id.
    pub fn decode(bytes: &[u8]) -> Result<Self, PackError> {
        let mut reader = Reader::new(bytes);
        let mut objects = Vec::new();
        while reader.remaining() > 0 {
            let id = ObjectId::from_bytes(reader.array()?);
            // A length past the address space is past the end of the pack.
            let len = usize::try_from(reader.u64()?).unwrap_or(usize::MAX);
            objects.push(EncodedObject::verify(id, reader.take(len)?.to_vec())?);
        }
        Ok(Self { objects })
    }
}

/// Bytes that are not a valid pack.
#[derive(Debug, thiserror::Error)]
pub enum PackError {
    /// The pack is not a sequence of whole entries: it ended in the middle
    /// of one.
    #[error("cannot decode pack")]
    Malformed(#[from] DecodeError),
    /// An object does not match its id.
    #[error(transparent)]
    Corrupt(#[from] IdMismatch),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let pack = Pack {
            objects: vec![EncodedObject::blob(b"a"), EncodedObject::blob(b"")],
        };
        assert_eq!(Pack::decode(&pack.encode()).unwrap(), pack);
        assert_eq!(Pack::decode(&[]).unwrap(), Pack::default());
    }

    #[test]
    fn rejects_truncated_and_corrupt_packs() {
        let mut bytes = Pack {
            objects: vec![EncodedObject::blob(b"content")],
        }
        .encode();
        assert!(matches!(
            Pack::decode(&bytes[..bytes.len() - 1]),
            Err(PackError::Malformed(DecodeError::Truncated))
        ));
        *bytes.last_mut().unwrap() ^= 1;
        assert!(matches!(Pack::decode(&bytes), Err(PackError::Corrupt(_))));
    }
}
