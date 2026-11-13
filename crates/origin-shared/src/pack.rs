//! The binary format objects travel in.
//!
//! A pack is a sequence of entries, each
//! `id:[u8; 32] len:u64 bytes:[u8; len]` with `bytes` the object's encoding.
//! Decoding verifies every object against its id, so a pack can come from an
//! untrusted peer.
//!
//! Transfers are bounded: a [`Packer`] splits objects into packs of at most
//! [`Pack::LIMIT`] bytes, in order, so a push takes several requests, and a
//! fetch response stops at a [`Part`], with the rest asked for again.

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

    /// The most bytes a request or a response carries as one pack, encoded.
    /// A pack holds one object at least, so an object larger than this
    /// travels alone.
    pub const LIMIT: usize = 64 << 20;

    /// The bytes `object` takes in an encoded pack.
    fn encoded_len(object: &EncodedObject) -> usize {
        Self::HEADER_LEN + object.bytes().len()
    }

    /// Encodes the pack.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let len = self.objects.iter().map(Self::encoded_len).sum();
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

/// Splits objects into packs of at most a limit, [`Pack::LIMIT`] by
/// default, in the order they're added. Objects only ever depend on those
/// before them, so each pack can be sent as soon as it's full.
#[derive(Debug)]
pub struct Packer {
    limit: usize,
    pack: Pack,
    /// The encoded size of `pack`.
    len: usize,
}

impl Default for Packer {
    fn default() -> Self {
        Self::new(Pack::LIMIT)
    }
}

impl Packer {
    /// A packer filling packs up to `limit` bytes.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            pack: Pack::default(),
            len: 0,
        }
    }

    /// Whether the pack being filled has room for `objects`: an empty one
    /// has room for anything.
    #[must_use]
    pub fn fits<'o>(&self, objects: impl IntoIterator<Item = &'o EncodedObject>) -> bool {
        let len: usize = objects.into_iter().map(Pack::encoded_len).sum();
        self.pack.objects.is_empty() || self.len + len <= self.limit
    }

    /// Adds `object` to the pack being filled. If it has no room for it,
    /// returns that pack, full, and starts the next one with the object.
    #[must_use = "a full pack is returned, not kept"]
    pub fn add(&mut self, object: EncodedObject) -> Option<Pack> {
        self.add_together(vec![object])
    }

    /// Adds `objects` to the pack being filled, which they all go in. If it
    /// has no room for them, returns that pack, full, and starts the next
    /// one with them.
    #[must_use = "a full pack is returned, not kept"]
    pub fn add_together(&mut self, objects: Vec<EncodedObject>) -> Option<Pack> {
        let full = (!self.fits(&objects)).then(|| {
            self.len = 0;
            std::mem::take(&mut self.pack)
        });
        self.extend(objects);
        full
    }

    /// Adds `objects` to the pack being filled, room or not: those
    /// [`Packer::fits`] said it has room for.
    pub fn extend(&mut self, objects: impl IntoIterator<Item = EncodedObject>) {
        for object in objects {
            self.len += Pack::encoded_len(&object);
            self.pack.objects.push(object);
        }
    }

    /// Whether no object was added since the last full pack.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pack.objects.is_empty()
    }

    /// The pack being filled.
    #[must_use]
    pub fn finish(self) -> Pack {
        self.pack
    }
}

/// A fetch's answer, which may stop short of everything asked for, to stay
/// within [`Pack::LIMIT`]: then `more` is set, and asking again for the
/// rest brings the next part (see
/// [`FetchRecords`](crate::api::FetchRecords) and
/// [`FetchSnapshot`](crate::api::FetchSnapshot)).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Part {
    /// The objects of this part.
    pub pack: Pack,
    /// Whether it stopped short.
    pub more: bool,
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
    fn packers_split_in_order_within_the_limit() {
        let large = [b'x'; 100];
        let objects: Vec<_> = [b"a".as_slice(), b"bb", b"c", &large]
            .map(EncodedObject::blob)
            .to_vec();
        let limit = 2 * Pack::encoded_len(&objects[0]) + 1;
        let mut packer = Packer::new(limit);
        let mut packs: Vec<Pack> = objects
            .iter()
            .filter_map(|object| packer.add(object.clone()))
            .collect();
        packs.push(packer.finish());
        let split: Vec<usize> = packs.iter().map(|pack| pack.objects.len()).collect();
        assert_eq!(split, [2, 1, 1], "one larger than the limit travels alone");
        let joined: Vec<_> = packs.into_iter().flat_map(|pack| pack.objects).collect();
        assert_eq!(joined, objects, "in order");
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
