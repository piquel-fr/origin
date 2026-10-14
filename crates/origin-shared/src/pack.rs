//! The binary formats objects travel in.
//!
//! A pack is a sequence of entries, each
//! `id:[u8; 32] len:u64 bytes:[u8; len]` with `bytes` the object's encoding.
//! Decoding verifies every object against its id, so a pack can come from an
//! untrusted peer. A withheld object, which the receiver may not read, is
//! `id:[u8; 32] u64::MAX`, without bytes.
//!
//! A push is `len:u32 updates:[u8; len] pack`, with `updates` the JSON list
//! of branch updates.

use crate::object::IdMismatch;
use crate::{EncodedObject, ObjectId, RefUpdate};

/// A batch of objects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pack {
    /// The objects, in the order they were added.
    pub objects: Vec<EncodedObject>,
    /// Objects that were asked for but that the receiver may not read.
    pub withheld: Vec<ObjectId>,
}

impl Pack {
    const HEADER_LEN: usize = ObjectId::LEN + 8;
    /// The length that marks a withheld object.
    const WITHHELD: u64 = u64::MAX;

    /// Encodes the pack.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let len = self
            .objects
            .iter()
            .map(|object| Self::HEADER_LEN + object.bytes().len())
            .sum::<usize>()
            + self.withheld.len() * Self::HEADER_LEN;
        let mut out = Vec::with_capacity(len);
        for object in &self.objects {
            out.extend_from_slice(object.id().as_bytes());
            out.extend_from_slice(&(object.bytes().len() as u64).to_le_bytes());
            out.extend_from_slice(object.bytes());
        }
        for id in &self.withheld {
            out.extend_from_slice(id.as_bytes());
            out.extend_from_slice(&Self::WITHHELD.to_le_bytes());
        }
        out
    }

    /// Decodes and verifies a pack.
    ///
    /// # Errors
    ///
    /// Fails if the pack is truncated or an object does not match its id.
    pub fn decode(mut bytes: &[u8]) -> Result<Self, PackError> {
        let mut pack = Self::default();
        while !bytes.is_empty() {
            let (id, rest) = bytes
                .split_first_chunk::<{ ObjectId::LEN }>()
                .ok_or(PackError::Truncated)?;
            let (len, rest) = rest.split_first_chunk::<8>().ok_or(PackError::Truncated)?;
            let id = ObjectId::from_bytes(*id);
            let len = u64::from_le_bytes(*len);
            if len == Self::WITHHELD {
                pack.withheld.push(id);
                bytes = rest;
                continue;
            }
            let len = usize::try_from(len).map_err(|_| PackError::Truncated)?;
            let (object, rest) = rest.split_at_checked(len).ok_or(PackError::Truncated)?;
            pack.objects
                .push(EncodedObject::verify(id, object.to_vec())?);
            bytes = rest;
        }
        Ok(pack)
    }
}

/// Branch updates with the objects they need, applied as one transaction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PushRequest {
    /// The branches to move, all or none.
    pub updates: Vec<RefUpdate>,
    /// Every object the new commits add, relative to their first parent.
    pub pack: Pack,
}

impl PushRequest {
    /// Encodes the push.
    ///
    /// # Panics
    ///
    /// Panics if the updates do not fit the 4 GiB header, which would take
    /// tens of millions of branches.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let updates = serde_json::to_vec(&self.updates).expect("updates serialize to JSON");
        let len = u32::try_from(updates.len()).expect("updates fit in 4 GiB");
        let mut out = len.to_le_bytes().to_vec();
        out.extend_from_slice(&updates);
        out.extend_from_slice(&self.pack.encode());
        out
    }

    /// Decodes and verifies a push.
    ///
    /// # Errors
    ///
    /// Fails if the push is truncated, its updates are invalid, or an object
    /// does not match its id.
    pub fn decode(bytes: &[u8]) -> Result<Self, PackError> {
        let (len, rest) = bytes.split_first_chunk::<4>().ok_or(PackError::Truncated)?;
        let len = usize::try_from(u32::from_le_bytes(*len)).map_err(|_| PackError::Truncated)?;
        let (updates, pack) = rest.split_at_checked(len).ok_or(PackError::Truncated)?;
        Ok(Self {
            updates: serde_json::from_slice(updates)?,
            pack: Pack::decode(pack)?,
        })
    }
}

/// Bytes that are not a valid pack or push.
#[derive(Debug, thiserror::Error)]
pub enum PackError {
    /// The bytes ended in the middle of an entry.
    #[error("pack is truncated")]
    Truncated,
    /// An object does not match its id.
    #[error(transparent)]
    Corrupt(#[from] IdMismatch),
    /// A push's branch updates are invalid.
    #[error("invalid branch updates")]
    Updates(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let pack = Pack {
            objects: vec![EncodedObject::blob(b"a"), EncodedObject::blob(b"")],
            withheld: vec![EncodedObject::blob(b"secret").id()],
        };
        assert_eq!(Pack::decode(&pack.encode()).unwrap(), pack);
        assert_eq!(Pack::decode(&[]).unwrap(), Pack::default());
    }

    #[test]
    fn pushes_round_trip() {
        let blob = EncodedObject::blob(b"a");
        let push = PushRequest {
            updates: vec![RefUpdate {
                branch: crate::BranchName::default(),
                expected: None,
                new: Some(blob.id()),
            }],
            pack: Pack {
                objects: vec![blob],
                withheld: Vec::new(),
            },
        };
        assert_eq!(PushRequest::decode(&push.encode()).unwrap(), push);
        assert!(matches!(
            PushRequest::decode(&[9, 0, 0, 0, b'[']),
            Err(PackError::Truncated)
        ));
    }

    #[test]
    fn rejects_truncated_and_corrupt_packs() {
        let mut bytes = Pack {
            objects: vec![EncodedObject::blob(b"content")],
            withheld: Vec::new(),
        }
        .encode();
        assert!(matches!(
            Pack::decode(&bytes[..bytes.len() - 1]),
            Err(PackError::Truncated)
        ));
        *bytes.last_mut().unwrap() ^= 1;
        assert!(matches!(Pack::decode(&bytes), Err(PackError::Corrupt(_))));
    }
}
