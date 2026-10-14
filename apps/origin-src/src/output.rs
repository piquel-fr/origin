//! How commands present what they print.

use std::fmt;

use origin_shared::TreeEntry;
use similar::TextDiff;

use crate::store::{ObjectStore, StoreError};
use crate::tree::Change;

/// The unified diff of one changed file.
pub struct FileDiff<'a> {
    change: &'a Change,
    old: Vec<u8>,
    new: Vec<u8>,
}

impl<'a> FileDiff<'a> {
    /// Reads both sides of a change.
    ///
    /// # Errors
    ///
    /// Fails if either side cannot be read or is not a blob.
    pub fn new(store: &ObjectStore, change: &'a Change) -> Result<Self, StoreError> {
        let content = |entry: Option<TreeEntry>| -> Result<Vec<u8>, StoreError> {
            let Some(entry) = entry else {
                return Ok(Vec::new());
            };
            let blob = store.read(entry.id)?;
            let content = blob.blob_content().map_err(|source| StoreError::Decode {
                id: entry.id,
                source,
            })?;
            Ok(content.to_vec())
        };
        Ok(Self {
            change,
            old: content(change.old)?,
            new: content(change.new)?,
        })
    }

    /// The content as text, or `None` if it is binary: not valid UTF-8, or
    /// holding a NUL byte, which text never contains.
    fn text(content: &[u8]) -> Option<&str> {
        str::from_utf8(content)
            .ok()
            .filter(|text| !text.contains('\0'))
    }
}

impl fmt::Display for FileDiff<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = &self.change.path;

        let label = |entry: Option<TreeEntry>, side: &str| {
            entry.map_or_else(|| "/dev/null".to_owned(), |_| format!("{side}/{path}"))
        };
        let old_label = label(self.change.old, "a");
        let new_label = label(self.change.new, "b");

        let kind = |entry: Option<TreeEntry>| entry.map(|entry| entry.kind);
        if let (Some(old), Some(new)) = (kind(self.change.old), kind(self.change.new))
            && old != new
        {
            writeln!(f, "{path}: {old} -> {new}")?;
        }
        if self.old == self.new {
            return Ok(());
        }
        let (Some(old), Some(new)) = (Self::text(&self.old), Self::text(&self.new)) else {
            return writeln!(f, "Binary file {path} differs");
        };
        write!(
            f,
            "{}",
            TextDiff::from_lines(old, new)
                .unified_diff()
                .header(&old_label, &new_label)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_utf8_without_nul() {
        assert_eq!(FileDiff::text("héllo\n".as_bytes()), Some("héllo\n"));
        // The PNG signature has no NUL byte, but is not UTF-8.
        assert_eq!(FileDiff::text(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(FileDiff::text(b"a\0b"), None);
    }
}
