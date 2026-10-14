use jiff::tz::{Offset, TimeZone};
use jiff::{Timestamp, Zoned};

use super::{DecodeError, Reader};
use crate::ObjectId;

/// Who made a commit, and when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// Display name.
    pub name: String,
    /// Email address.
    pub email: String,
    /// When, in whole seconds: the encoding drops anything finer.
    pub timestamp: Timestamp,
    /// The signer's offset from UTC at the time.
    pub offset: Offset,
}

impl Signature {
    /// Signs as `name <email>`, now, in the system's time zone.
    ///
    /// # Panics
    ///
    /// Never: the current time truncated to seconds is always in range.
    #[must_use]
    pub fn now(name: String, email: String) -> Self {
        let now = Zoned::now();
        Self {
            name,
            email,
            timestamp: Timestamp::from_second(now.timestamp().as_second())
                .expect("the current time is in range"),
            offset: now.offset(),
        }
    }

    /// The time as the signer saw it, in their offset from UTC.
    #[must_use]
    pub fn time(&self) -> Zoned {
        self.timestamp.to_zoned(TimeZone::fixed(self.offset))
    }
}

/// A snapshot of a tree with its history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    /// The root tree.
    pub tree: ObjectId,
    /// The commits this one follows, first parent first.
    pub parents: Vec<ObjectId>,
    /// Who made the commit.
    pub author: Signature,
    /// The commit message.
    pub message: String,
}

impl Commit {
    pub(super) fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.tree.as_bytes());
        out.extend_from_slice(&len_u32(self.parents.len()).to_le_bytes());
        for parent in &self.parents {
            out.extend_from_slice(parent.as_bytes());
        }
        for field in [&self.author.name, &self.author.email] {
            out.extend_from_slice(&len_u32(field.len()).to_le_bytes());
            out.extend_from_slice(field.as_bytes());
        }
        out.extend_from_slice(&self.author.timestamp.as_second().to_le_bytes());
        out.extend_from_slice(&self.author.offset.seconds().to_le_bytes());
        out.extend_from_slice(self.message.as_bytes());
    }

    pub(super) fn decode(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let tree = reader.id()?;
        let parent_count = reader.u32()?;
        let parents = (0..parent_count)
            .map(|_| reader.id())
            .collect::<Result<_, _>>()?;
        let name = reader.prefixed_str()?;
        let email = reader.prefixed_str()?;
        let timestamp = Timestamp::from_second(i64::from_le_bytes(reader.array()?))
            .map_err(|_| DecodeError::InvalidTime)?;
        let offset = Offset::from_seconds(i32::from_le_bytes(reader.array()?))
            .map_err(|_| DecodeError::InvalidTime)?;
        let message = std::str::from_utf8(reader.rest())
            .map_err(|_| DecodeError::InvalidUtf8)?
            .to_owned();
        Ok(Self {
            tree,
            parents,
            author: Signature {
                name,
                email,
                timestamp,
                offset,
            },
            message,
        })
    }
}

/// Lengths in encodings are `u32`. Nothing in a commit header comes close.
fn len_u32(len: usize) -> u32 {
    u32::try_from(len).expect("commit field longer than 4 GiB")
}
