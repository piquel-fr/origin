use super::{DecodeError, InvalidEntryName, Reader};
use crate::ObjectId;
use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::collections::btree_map;
use std::fmt;

/// The name of an entry in a tree: one path component.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryName(String);

impl EntryName {
    /// Names reserved for repository metadata, compared case-insensitively so
    /// they cannot be smuggled in on case-insensitive filesystems.
    const RESERVED: [&str; 2] = [".origin", ".git"];

    /// Validates a path component.
    ///
    /// # Errors
    ///
    /// Fails if the name is empty, `.` or `..`, reserved, or contains `/` or
    /// a NUL byte.
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidEntryName> {
        let name = name.into();
        let valid = !name.is_empty()
            && name != "."
            && name != ".."
            && !name.contains(['/', '\0'])
            && !Self::RESERVED
                .iter()
                .any(|reserved| name.eq_ignore_ascii_case(reserved));
        if valid {
            Ok(Self(name))
        } else {
            Err(InvalidEntryName(name))
        }
    }

    /// The name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for EntryName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EntryName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for EntryName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

/// What a tree entry points to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum EntryKind {
    /// A regular file, pointing to a blob.
    File = 1,
    /// An executable file, pointing to a blob.
    Executable = 2,
    /// A symbolic link, pointing to a blob holding its target.
    Symlink = 3,
    /// A directory, pointing to a tree.
    Tree = 4,
}

impl EntryKind {
    fn from_byte(byte: u8) -> Result<Self, DecodeError> {
        match byte {
            1 => Ok(Self::File),
            2 => Ok(Self::Executable),
            3 => Ok(Self::Symlink),
            4 => Ok(Self::Tree),
            _ => Err(DecodeError::UnknownEntryKind(byte)),
        }
    }

    /// Whether the entry points to a tree.
    #[must_use]
    pub fn is_tree(self) -> bool {
        self == Self::Tree
    }
}

/// One entry of a tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TreeEntry {
    /// What the entry points to.
    pub kind: EntryKind,
    /// The object it points to.
    pub id: ObjectId,
}

/// A directory listing, sorted by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    entries: BTreeMap<EntryName, TreeEntry>,
}

impl Tree {
    /// Adds or replaces an entry.
    pub fn insert(&mut self, name: EntryName, entry: TreeEntry) {
        self.entries.insert(name, entry);
    }

    /// Looks up an entry by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&TreeEntry> {
        self.entries.get(name)
    }

    /// The entries, sorted by name.
    pub fn iter(&self) -> btree_map::Iter<'_, EntryName, TreeEntry> {
        self.entries.iter()
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the tree has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn encode_into(&self, out: &mut Vec<u8>) {
        for (name, entry) in &self.entries {
            out.push(entry.kind as u8);
            out.extend_from_slice(name.0.as_bytes());
            out.push(0);
            out.extend_from_slice(entry.id.as_bytes());
        }
    }

    pub(super) fn decode(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let mut entries = BTreeMap::new();
        let mut previous: Option<EntryName> = None;
        while !reader.0.is_empty() {
            let kind = EntryKind::from_byte(reader.u8()?)?;
            let name = EntryName::new(reader.str_until_nul()?)?;
            let id = reader.id()?;
            if previous.as_ref().is_some_and(|previous| *previous >= name) {
                return Err(DecodeError::UnsortedTree);
            }
            previous = Some(name.clone());
            entries.insert(name, TreeEntry { kind, id });
        }
        Ok(Self { entries })
    }
}

impl<'a> IntoIterator for &'a Tree {
    type Item = (&'a EntryName, &'a TreeEntry);
    type IntoIter = btree_map::Iter<'a, EntryName, TreeEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl FromIterator<(EntryName, TreeEntry)> for Tree {
    fn from_iter<I: IntoIterator<Item = (EntryName, TreeEntry)>>(iter: I) -> Self {
        Self {
            entries: iter.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::tests::sample_tree;
    use crate::object::{EncodedObject, Object, ObjectKind};

    #[test]
    fn decode_rejects_non_canonical_trees() {
        let tree = Object::Tree(sample_tree()).encode();
        let entry_len = 1 + "a".len() + 1 + ObjectId::LEN;
        // Move the first entry ("a") after the others.
        let mut swapped = vec![ObjectKind::Tree as u8];
        swapped.extend_from_slice(&tree.bytes()[1 + entry_len..]);
        swapped.extend_from_slice(&tree.bytes()[1..=entry_len]);
        let swapped = EncodedObject::hash(swapped);
        assert_eq!(swapped.decode(), Err(DecodeError::UnsortedTree));

        let mut truncated = tree.bytes().to_vec();
        truncated.pop();
        assert_eq!(
            EncodedObject::hash(truncated).decode(),
            Err(DecodeError::Truncated)
        );
    }

    #[test]
    fn entry_names_are_validated() {
        for invalid in ["", ".", "..", "a/b", "nul\0", ".origin", ".GIT"] {
            assert!(EntryName::new(invalid).is_err(), "{invalid:?}");
        }
        for valid in ["a", ".hidden", "...", "日本", ".originals"] {
            assert!(EntryName::new(valid).is_ok(), "{valid:?}");
        }
    }
}
