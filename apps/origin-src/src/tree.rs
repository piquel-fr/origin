//! Navigating and comparing trees.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use origin_shared::{EntryKind, EntryName, Object, ObjectId, RepoPath, Tree, TreeEntry};

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

/// Something trees can be read from: a store, or a push not yet stored.
pub trait TreeSource {
    /// Why a tree cannot be read.
    type Error;

    /// Reads a tree.
    ///
    /// # Errors
    ///
    /// Fails if the tree is missing or cannot be read.
    fn read_tree(&self, id: ObjectId) -> Result<Tree, Self::Error>;

    /// Finds the entry at `path` under the tree `root`, or `None` if nothing
    /// is there. The root path resolves to `root` itself.
    ///
    /// # Errors
    ///
    /// Fails if a tree along the path cannot be read.
    fn resolve(&self, root: ObjectId, path: &RepoPath) -> Result<Option<TreeEntry>, Self::Error> {
        let mut entry = TreeEntry {
            kind: EntryKind::Tree,
            id: root,
        };
        for name in path.components() {
            if !entry.kind.is_tree() {
                return Ok(None);
            }
            match self.read_tree(entry.id)?.get(name) {
                Some(child) => entry = *child,
                None => return Ok(None),
            }
        }
        Ok(Some(entry))
    }

    /// Visits the tree `new` and every entry under it that differs from the
    /// entry at the same path under `old`, parents before children. Subtrees
    /// equal to their counterpart are skipped without being read: these are
    /// exactly the objects `new` adds.
    ///
    /// # Errors
    ///
    /// Fails if a changed tree cannot be read, or `visit` fails.
    fn for_each_change(
        &self,
        new: ObjectId,
        old: Option<ObjectId>,
        visit: &mut impl FnMut(&RepoPath, TreeEntry) -> Result<(), Self::Error>,
    ) -> Result<(), Self::Error> {
        let root = TreeEntry {
            kind: EntryKind::Tree,
            id: new,
        };
        self.visit_changes(&RepoPath::root(), root, old, visit)
    }

    /// Lists the files that differ between two trees, sorted by path.
    /// `None` stands for an empty tree. Subtrees with equal ids are skipped
    /// without being read, so the cost follows the size of the change.
    ///
    /// # Errors
    ///
    /// Fails if a tree that differs cannot be read.
    fn diff(
        &self,
        old: Option<ObjectId>,
        new: Option<ObjectId>,
    ) -> Result<Vec<Change>, Self::Error> {
        let mut changes = Vec::new();
        self.diff_dir(&RepoPath::root(), old, new, &mut changes)?;
        Ok(changes)
    }

    #[doc(hidden)]
    fn diff_dir(
        &self,
        dir: &RepoPath,
        old: Option<ObjectId>,
        new: Option<ObjectId>,
        changes: &mut Vec<Change>,
    ) -> Result<(), Self::Error> {
        if old == new {
            return Ok(());
        }
        let read = |id: Option<ObjectId>| id.map(|id| self.read_tree(id)).transpose();
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

    #[doc(hidden)]
    fn visit_changes(
        &self,
        path: &RepoPath,
        new: TreeEntry,
        old: Option<ObjectId>,
        visit: &mut impl FnMut(&RepoPath, TreeEntry) -> Result<(), Self::Error>,
    ) -> Result<(), Self::Error> {
        if Some(new.id) == old {
            return Ok(());
        }
        visit(path, new)?;
        if !new.kind.is_tree() {
            return Ok(());
        }
        let old = old
            .map(|old| self.read_tree(old))
            .transpose()?
            .unwrap_or_default();
        for (name, entry) in &self.read_tree(new.id)? {
            let old_entry = old.get(name.as_str());
            if old_entry == Some(entry) {
                continue;
            }
            // Only a tree can be compared with a tree; anything else at the
            // same path is replaced wholesale.
            let old_tree = old_entry
                .filter(|old| old.kind == entry.kind)
                .map(|old| old.id);
            self.visit_changes(&path.join(name), *entry, old_tree, visit)?;
        }
        Ok(())
    }
}

impl TreeSource for ObjectStore {
    type Error = StoreError;

    fn read_tree(&self, id: ObjectId) -> Result<Tree, StoreError> {
        self.tree(id)
    }
}

/// Reads trees from a store, remembering them: resolving many paths of the
/// same tree reads each directory once.
pub struct CachedTrees<'a> {
    store: &'a ObjectStore,
    trees: RefCell<HashMap<ObjectId, Tree>>,
}

impl<'a> CachedTrees<'a> {
    /// Caches trees read from `store`.
    #[must_use]
    pub fn new(store: &'a ObjectStore) -> Self {
        Self {
            store,
            trees: RefCell::default(),
        }
    }
}

impl TreeSource for CachedTrees<'_> {
    type Error = StoreError;

    fn read_tree(&self, id: ObjectId) -> Result<Tree, StoreError> {
        if let Some(tree) = self.trees.borrow().get(&id) {
            return Ok(tree.clone());
        }
        let tree = self.store.tree(id)?;
        self.trees.borrow_mut().insert(id, tree.clone());
        Ok(tree)
    }
}

impl ObjectStore {
    /// Inserts `entries` into the tree `root` where nothing is at their
    /// path, and returns the new root. Snapshots use it to keep files the
    /// working copy cannot have, like withheld ones.
    ///
    /// # Errors
    ///
    /// Fails if a tree along a path cannot be read or written.
    pub fn overlay(
        &self,
        root: ObjectId,
        entries: &[(RepoPath, TreeEntry)],
    ) -> Result<ObjectId, StoreError> {
        let mut tree = self.tree(root)?;
        let mut changed = false;
        let mut nested: BTreeMap<EntryName, Vec<(RepoPath, TreeEntry)>> = BTreeMap::new();
        for (path, entry) in entries {
            let Some((first, rest)) = path.split_first() else {
                continue;
            };
            if !rest.is_root() {
                nested.entry(first).or_default().push((rest, *entry));
            } else if tree.get(first.as_str()).is_none() {
                tree.insert(first, *entry);
                changed = true;
            }
        }
        for (name, entries) in nested {
            let child = match tree.get(name.as_str()) {
                Some(child) if child.kind.is_tree() => child.id,
                Some(_) => continue,
                None => self.write_tree(Tree::default())?,
            };
            let id = self.overlay(child, &entries)?;
            if id != child {
                tree.insert(
                    name,
                    TreeEntry {
                        kind: EntryKind::Tree,
                        id,
                    },
                );
                changed = true;
            }
        }
        if !changed {
            return Ok(root);
        }
        self.write_tree(tree)
    }

    fn write_tree(&self, tree: Tree) -> Result<ObjectId, StoreError> {
        let encoded = Object::Tree(tree).encode();
        self.write(&encoded)?;
        Ok(encoded.id())
    }
}

#[cfg(test)]
mod tests {
    use origin_shared::EncodedObject;

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
    fn visits_exactly_the_added_objects() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path());
        let old = build(&store, &[("same/a", "1"), ("edit/b", "2"), ("file", "3")]);
        let new = build(
            &store,
            &[
                ("same/a", "1"),
                ("edit/b", "changed"),
                ("file/now-dir", "4"),
            ],
        );

        let mut visited = Vec::new();
        store
            .for_each_change(new, Some(old), &mut |path, _| {
                visited.push(path.to_string());
                Ok(())
            })
            .unwrap();
        assert_eq!(visited, ["/", "edit", "edit/b", "file", "file/now-dir"]);

        let mut count = 0;
        store
            .for_each_change(new, None, &mut |_, _| {
                count += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(count, 7, "every object of a tree without a parent is new");
    }

    #[test]
    fn overlays_fill_only_empty_paths() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path());
        let root = build(&store, &[("kept", "mine"), ("src/lib.rs", "lib")]);
        let overlaid = store
            .overlay(
                root,
                &[
                    (path("kept"), file("theirs")),
                    (path("src/secret.rs"), file("secret")),
                    (path("new/deep/file"), file("deep")),
                ],
            )
            .unwrap();
        let expected = build(
            &store,
            &[
                ("kept", "mine"),
                ("src/lib.rs", "lib"),
                ("src/secret.rs", "secret"),
                ("new/deep/file", "deep"),
            ],
        );
        assert_eq!(overlaid, expected);
        assert_eq!(
            store
                .overlay(root, &[(path("kept"), file("theirs"))])
                .unwrap(),
            root
        );
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
