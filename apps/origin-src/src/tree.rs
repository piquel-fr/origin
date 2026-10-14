//! Navigating and comparing trees.

use origin_shared::{EntryKind, EntryName, ObjectId, RepoPath, TreeEntry};

use crate::store::{ObjectStore, StoreError};

/// A file that differs between two trees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// Where the file is.
    pub path: RepoPath,
    /// The file before, if it existed.
    pub old: Option<TreeEntry>,
    /// The file after, if it exists.
    pub new: Option<TreeEntry>,
}

impl ObjectStore {
    /// Finds the entry at `path` under the tree `root`, or `None` if nothing
    /// is there. The root path resolves to `root` itself.
    ///
    /// # Errors
    ///
    /// Fails if a tree along the path cannot be read.
    pub fn resolve(
        &self,
        root: ObjectId,
        path: &RepoPath,
    ) -> Result<Option<TreeEntry>, StoreError> {
        let mut entry = TreeEntry {
            kind: EntryKind::Tree,
            id: root,
        };
        for name in path.components() {
            if !entry.kind.is_tree() {
                return Ok(None);
            }
            match self.tree(entry.id)?.get(name) {
                Some(child) => entry = *child,
                None => return Ok(None),
            }
        }
        Ok(Some(entry))
    }

    /// Lists the files that differ between two trees, sorted by path.
    /// `None` stands for an empty tree. Subtrees with equal ids are skipped
    /// without being read, so the cost follows the size of the change.
    ///
    /// # Errors
    ///
    /// Fails if a tree that differs cannot be read.
    pub fn diff(
        &self,
        old: Option<ObjectId>,
        new: Option<ObjectId>,
    ) -> Result<Vec<Change>, StoreError> {
        let mut changes = Vec::new();
        self.diff_dir(&RepoPath::root(), old, new, &mut changes)?;
        Ok(changes)
    }

    fn diff_dir(
        &self,
        dir: &RepoPath,
        old: Option<ObjectId>,
        new: Option<ObjectId>,
        changes: &mut Vec<Change>,
    ) -> Result<(), StoreError> {
        if old == new {
            return Ok(());
        }
        let read = |id: Option<ObjectId>| id.map(|id| self.tree(id)).transpose();
        let (old, new) = (
            read(old)?.unwrap_or_default(),
            read(new)?.unwrap_or_default(),
        );
        let mut names: Vec<&EntryName> =
            old.iter().chain(new.iter()).map(|(name, _)| name).collect();
        names.sort_unstable();
        names.dedup();
        for name in names {
            let (old, new) = (
                old.get(name.as_str()).copied(),
                new.get(name.as_str()).copied(),
            );
            if old == new {
                continue;
            }
            let path = dir.join(name);
            // A name can hold a file on one side and a directory on the
            // other: the file part is a change, the directory part is
            // diffed against an empty tree.
            let old_file = old.and_then(TreeEntry::file);
            let new_file = new.and_then(TreeEntry::file);

            if old_file.is_some() || new_file.is_some() {
                changes.push(Change {
                    path: path.clone(),
                    old: old_file,
                    new: new_file,
                });
            }

            let old_tree = old.and_then(TreeEntry::subtree);
            let new_tree = new.and_then(TreeEntry::subtree);
            self.diff_dir(&path, old_tree, new_tree, changes)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use origin_shared::{EncodedObject, EntryKind, Object, Tree};

    use super::*;

    /// Builds nested trees from files inserted by path.
    #[derive(Debug, Default)]
    struct TreeBuilder {
        entries: BTreeMap<EntryName, Node>,
    }

    #[derive(Debug)]
    enum Node {
        File(TreeEntry),
        Dir(TreeBuilder),
    }

    impl TreeBuilder {
        /// Adds a file (not a tree) at `path`, creating parent directories and
        /// replacing whatever was there. Inserting at the root does nothing.
        fn insert(&mut self, path: &RepoPath, entry: TreeEntry) {
            debug_assert!(!entry.kind.is_tree(), "only files are inserted");
            let mut names = path.names().peekable();
            let mut dir = self;
            while let Some(name) = names.next() {
                if names.peek().is_none() {
                    dir.entries.insert(name, Node::File(entry));
                    return;
                }
                let node = dir
                    .entries
                    .entry(name)
                    .or_insert_with(|| Node::Dir(Self::default()));
                if let Node::File(_) = node {
                    *node = Node::Dir(Self::default());
                }
                let Node::Dir(child) = node else {
                    unreachable!("replaced by a directory above")
                };
                dir = child;
            }
        }

        /// Writes every tree to the store and returns the root's id. The root is
        /// written even when empty; empty subdirectories cannot occur since only
        /// files are inserted.
        ///
        /// # Errors
        ///
        /// Fails if a tree cannot be written.
        fn write(self, store: &ObjectStore) -> Result<ObjectId, StoreError> {
            let mut tree = Tree::default();
            for (name, node) in self.entries {
                let entry = match node {
                    Node::File(entry) => entry,
                    Node::Dir(dir) => TreeEntry {
                        kind: EntryKind::Tree,
                        id: dir.write(store)?,
                    },
                };
                tree.insert(name, entry);
            }
            let encoded = Object::Tree(tree).encode();
            store.write(&encoded)?;
            Ok(encoded.id())
        }
    }

    fn file(content: &str) -> TreeEntry {
        TreeEntry {
            kind: EntryKind::File,
            id: EncodedObject::blob(content.as_bytes()).id(),
        }
    }

    fn path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn build(store: &ObjectStore, files: &[(&str, &str)]) -> ObjectId {
        let mut builder = TreeBuilder::default();
        for (name, content) in files {
            builder.insert(&path(name), file(content));
        }
        builder.write(store).unwrap()
    }

    #[test]
    fn builds_nested_trees() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path());
        let root = build(
            &store,
            &[
                ("a.txt", "a"),
                ("src/lib.rs", "lib"),
                ("src/bin/main.rs", "main"),
            ],
        );

        let root = store.tree(root).unwrap();
        assert_eq!(root.len(), 2);
        assert_eq!(root.get("a.txt"), Some(&file("a")));
        let src = store.tree(root.get("src").unwrap().id).unwrap();
        assert_eq!(src.get("lib.rs"), Some(&file("lib")));
        assert!(src.get("bin").unwrap().kind.is_tree());
    }

    #[test]
    fn resolves_paths() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path());
        let root = build(&store, &[("src/lib.rs", "lib")]);

        assert_eq!(
            store.resolve(root, &RepoPath::root()).unwrap().unwrap().id,
            root
        );
        assert!(
            store
                .resolve(root, &path("src"))
                .unwrap()
                .unwrap()
                .kind
                .is_tree()
        );
        assert_eq!(
            store.resolve(root, &path("src/lib.rs")).unwrap(),
            Some(file("lib"))
        );
        assert_eq!(
            store.resolve(root, &path("src/lib.rs/deeper")).unwrap(),
            None
        );
        assert_eq!(store.resolve(root, &path("missing")).unwrap(), None);
    }

    #[test]
    fn identical_contents_give_identical_trees() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path());
        let first = build(&store, &[("b", "2"), ("a/x", "1")]);
        let second = build(&store, &[("a/x", "1"), ("b", "2")]);
        assert_eq!(first, second);
    }

    #[test]
    fn diffs_files_across_directories() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path());
        let old = build(
            &store,
            &[
                ("same", "s"),
                ("edit", "1"),
                ("gone", "g"),
                ("dir/inner", "i"),
                ("swap", "f"),
            ],
        );
        let new = build(
            &store,
            &[
                ("same", "s"),
                ("edit", "2"),
                ("added/deep", "d"),
                ("swap/now-dir", "n"),
            ],
        );

        let changes = store.diff(Some(old), Some(new)).unwrap();
        let summary: Vec<_> = changes
            .iter()
            .map(|c| (c.path.as_str(), c.old.is_some(), c.new.is_some()))
            .collect();
        assert_eq!(
            summary,
            [
                ("added/deep", false, true),
                ("dir/inner", true, false),
                ("edit", true, true),
                ("gone", true, false),
                ("swap", true, false),
                ("swap/now-dir", false, true),
            ]
        );
        assert_eq!(store.diff(Some(old), Some(old)).unwrap(), []);
        assert_eq!(store.diff(None, Some(old)).unwrap().len(), 5);
    }
}
