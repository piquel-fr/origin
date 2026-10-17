use std::collections::BTreeMap;
use std::collections::btree_map;
use std::fmt;

use super::{DecodeError, Field, ObjectKind, Reader, Record};
use crate::name::names;
use crate::{ObjectId, TreeId};

names! {
    /// The name of an entry in a tree: one path component. It isn't empty,
    /// `.` or `..`, and contains no `/` or NUL byte. The names of repository
    /// metadata directories are reserved, compared case-insensitively so they
    /// cannot be smuggled in on case-insensitive file systems.
    EntryName("file name");
}

impl EntryName {
    const RESERVED: [&str; 2] = [".origin", ".git"];

    fn is_valid(name: &str) -> bool {
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.contains(['/', '\0'])
            && !Self::is_reserved(name)
    }

    /// Whether a name belongs to repository metadata, which working copies
    /// skip and trees cannot contain.
    #[must_use]
    pub fn is_reserved(name: &str) -> bool {
        Self::RESERVED
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
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
    /// The kind a tag byte encodes, as in tree encodings.
    ///
    /// # Errors
    ///
    /// Fails if the byte encodes no kind.
    pub fn from_byte(tag: u8) -> Result<Self, DecodeError> {
        match tag {
            1 => Ok(Self::File),
            2 => Ok(Self::Executable),
            3 => Ok(Self::Symlink),
            4 => Ok(Self::Tree),
            tag => Err(DecodeError::UnknownTag {
                field: "EntryKind",
                tag,
            }),
        }
    }

    /// Whether the entry points to a tree.
    #[must_use]
    pub fn is_tree(self) -> bool {
        self == Self::Tree
    }
}

impl fmt::Display for EntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::File => "file",
            Self::Executable => "executable",
            Self::Symlink => "symlink",
            Self::Tree => "directory",
        })
    }
}

impl std::str::FromStr for EntryKind {
    type Err = String;

    fn from_str(kind: &str) -> Result<Self, Self::Err> {
        match kind {
            "file" => Ok(Self::File),
            "executable" => Ok(Self::Executable),
            "symlink" => Ok(Self::Symlink),
            "directory" => Ok(Self::Tree),
            _ => Err(format!("invalid entry kind `{kind}`")),
        }
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

impl TreeEntry {
    /// The entry, if it is a file (regular, executable or symlink).
    #[must_use]
    pub fn file(self) -> Option<Self> {
        (!self.kind.is_tree()).then_some(self)
    }

    /// The tree the entry points to, if it is a directory.
    #[must_use]
    pub fn subtree(self) -> Option<TreeId> {
        self.kind.is_tree().then_some(self.id.into())
    }
}

/// A directory listing, sorted by name.
///
/// Encoded as its entries in order, each `kind:u8 name 0x00 id:[u8; 32]`,
/// up to the end of the object: a tree is never a field of another record.
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
}

impl Record for Tree {
    const KIND: ObjectKind = ObjectKind::Tree;
}

impl Field for Tree {
    fn write(&self, out: &mut Vec<u8>) {
        for (name, entry) in &self.entries {
            out.push(entry.kind as u8);
            out.extend_from_slice(name.as_str().as_bytes());
            out.push(0);
            entry.id.write(out);
        }
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let mut entries = BTreeMap::new();
        let mut previous: Option<EntryName> = None;
        while reader.remaining() > 0 {
            let kind = EntryKind::from_byte(reader.u8()?)?;
            let name = EntryName::new(reader.str_until_nul()?)?;
            let id = ObjectId::read(reader)?;
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
