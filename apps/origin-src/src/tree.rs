//! Comparing trees, and looking up what they hold.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use origin_platform::Platform;
use origin_shared::{
    Digest, EntryKind, EntryName, Inputs, ObjectId, RepoPath, Tree, TreeEntry, TreeId,
};

use crate::store::{Store, StoreError};

/// A file that differs between two trees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    /// Where the file is.
    pub path: RepoPath,
    /// The file before, if it existed.
    pub old: Option<TreeEntry>,
    /// The file after, if it exists.
    pub new: Option<TreeEntry>,
}

impl<P: Platform> Store<P> {
    /// Lists the files that differ between two trees, sorted by path.
    /// Subtrees with equal ids are skipped without being read, so the cost
    /// follows the size of the change.
    ///
    /// # Errors
    ///
    /// Fails if a tree that differs cannot be read.
    pub fn diff(&self, old: TreeId, new: TreeId) -> Result<Vec<FileChange>, StoreError> {
        let mut changes = Vec::new();
        self.diff_dir(&RepoPath::root(), Some(old), Some(new), &mut changes)?;
        Ok(changes)
    }

    /// Diffs two directories, `None` standing for one that doesn't exist.
    fn diff_dir(
        &self,
        dir: &RepoPath,
        old: Option<TreeId>,
        new: Option<TreeId>,
        changes: &mut Vec<FileChange>,
    ) -> Result<(), StoreError> {
        if old == new {
            return Ok(());
        }
        let read = |id: Option<TreeId>| id.map(|id| self.get(id)).transpose();
        let (old, new): (Tree, Tree) = (
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
                changes.push(FileChange {
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

/// Looks up the entries of snapshots by path, reading each tree once,
/// however many snapshots share it.
#[derive(Debug)]
pub struct Snapshots<'a, P> {
    store: &'a Store<P>,
    trees: HashMap<TreeId, Tree>,
}

impl<'a, P: Platform> Snapshots<'a, P> {
    /// Looks up snapshots in `store`.
    pub fn new(store: &'a Store<P>) -> Self {
        Self {
            store,
            trees: HashMap::new(),
        }
    }

    /// What `snapshot` stores at `path`, or `None` if nothing is there. At
    /// the root, that's the snapshot's own tree.
    ///
    /// # Errors
    ///
    /// Fails if a tree on the way cannot be read.
    pub fn entry_at(
        &mut self,
        snapshot: TreeId,
        path: &RepoPath,
    ) -> Result<Option<TreeEntry>, StoreError> {
        let mut entry = TreeEntry {
            kind: EntryKind::Tree,
            id: snapshot.into(),
        };
        for name in path.components() {
            let Some(id) = entry.subtree() else {
                return Ok(None);
            };
            let tree = match self.trees.entry(id) {
                Entry::Occupied(tree) => tree.into_mut(),
                Entry::Vacant(vacant) => vacant.insert(self.store.get(id)?),
            };
            match tree.get(name) {
                Some(&next) => entry = next,
                None => return Ok(None),
            }
        }
        Ok(Some(entry))
    }

    /// The input digest of `snapshot`: one lookup per path of `inputs`.
    ///
    /// # Errors
    ///
    /// Fails if a tree on the way to a path cannot be read.
    pub fn input_digest(
        &mut self,
        snapshot: TreeId,
        inputs: &Inputs,
    ) -> Result<Digest, StoreError> {
        inputs.digest(|path| self.entry_at(snapshot, path))
    }
}

impl<P: Platform> Store<P> {
    /// Adds every object the trees `roots` reach to `reached`: the trees
    /// themselves, their subtrees and their files. Trees already in
    /// `reached` are not read again, and those the store doesn't have, as
    /// after a lazy clone, are skipped.
    ///
    /// # Errors
    ///
    /// Fails if a tree cannot be read.
    pub fn reach(
        &self,
        roots: impl IntoIterator<Item = TreeId>,
        reached: &mut HashSet<ObjectId>,
    ) -> Result<(), StoreError> {
        let mut pending: Vec<TreeId> = roots.into_iter().collect();
        while let Some(id) = pending.pop() {
            if !reached.insert(id.into()) {
                continue;
            }
            let tree: Tree = match self.get(id) {
                Err(StoreError::NotFound(_)) => continue,
                tree => tree?,
            };
            for (_, entry) in &tree {
                match entry.subtree() {
                    Some(subtree) => pending.push(subtree),
                    None => {
                        reached.insert(entry.id);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use origin_platform::Native;
    use origin_shared::{EncodedObject, EntryKind, Record};

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
        /// Adds a file at `path`, creating parent directories.
        fn insert(&mut self, path: &str, entry: TreeEntry) {
            let (dir, name) = match path.rsplit_once('/') {
                Some((dir, name)) => (Some(dir), name),
                None => (None, path),
            };
            let mut builder = self;
            for component in dir.into_iter().flat_map(|dir| dir.split('/')) {
                let node = builder
                    .entries
                    .entry(EntryName::new(component).unwrap())
                    .or_insert_with(|| Node::Dir(Self::default()));
                let Node::Dir(child) = node else {
                    panic!("{component} is a file")
                };
                builder = child;
            }
            builder
                .entries
                .insert(EntryName::new(name).unwrap(), Node::File(entry));
        }

        /// Writes every tree to the store and returns the root's id.
        fn write(self, store: &Store) -> TreeId {
            let tree: Tree = self
                .entries
                .into_iter()
                .map(|(name, node)| {
                    let entry = match node {
                        Node::File(entry) => entry,
                        Node::Dir(dir) => TreeEntry {
                            kind: EntryKind::Tree,
                            id: dir.write(store).into(),
                        },
                    };
                    (name, entry)
                })
                .collect();
            let encoded = tree.encode();
            store.write_all(std::slice::from_ref(&encoded)).unwrap();
            encoded.id().into()
        }
    }

    fn file(content: &str) -> TreeEntry {
        TreeEntry {
            kind: EntryKind::File,
            id: EncodedObject::blob(content.as_bytes()).id(),
        }
    }

    pub(crate) fn build(store: &Store, files: &[(&str, &str)]) -> TreeId {
        let mut builder = TreeBuilder::default();
        for (path, content) in files {
            builder.insert(path, file(content));
        }
        builder.write(store)
    }

    #[test]
    fn diffs_files_across_directories() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
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

        let changes = store.diff(old, new).unwrap();
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
        assert_eq!(store.diff(old, old).unwrap(), []);
        let empty = build(&store, &[]);
        assert_eq!(store.diff(empty, old).unwrap().len(), 5);
    }

    #[test]
    fn looks_entries_up_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        let root = build(&store, &[("src/lib.rs", "lib"), ("README", "r")]);
        let mut snapshots = Snapshots::new(&store);
        let mut at = |path: &str| {
            snapshots
                .entry_at(root, &RepoPath::new(path).unwrap())
                .unwrap()
        };

        assert_eq!(at("src/lib.rs"), Some(file("lib")));
        assert_eq!(at("").unwrap().id, root.into(), "the root is the snapshot");
        assert!(at("src").unwrap().kind.is_tree());
        for missing in ["nope", "src/nope", "README/inside"] {
            assert_eq!(at(missing), None, "{missing}");
        }

        let mut reached = HashSet::new();
        store.reach([root], &mut reached).unwrap();
        assert_eq!(reached.len(), 4, "two trees and two files");
        assert!(reached.contains(&file("lib").id));
    }
}
