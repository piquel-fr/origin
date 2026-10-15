//! Comparing trees, merging them, and looking up what they hold.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use origin_platform::Platform;
use origin_shared::{
    Digest, EncodedObject, EntryKind, EntryName, Inputs, ObjectId, Record, RepoPath, Tree,
    TreeEntry, TreeId,
};

use crate::conflict::Conflict;
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

/// Two trees merged with the changes each made to a common one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Merge {
    /// The merged tree: both sides' changes, and `ours` where they conflict.
    pub tree: TreeId,
    /// The trees the merge made, which the store may not have yet.
    pub trees: Vec<EncodedObject>,
    /// The files with conflict markers a [marked](Store::merge_marked)
    /// merge made, which the store may not have yet either.
    pub files: Vec<EncodedObject>,
    /// The paths both sides changed, each its own way, sorted.
    pub conflicts: Vec<RepoPath>,
}

impl<P: Platform> Store<P> {
    /// Merges `ours` and `theirs`, two trees made from `base`, file by file:
    /// a path one side changed takes that side's version, and a path both
    /// changed, each its own way, is a conflict, where the merge keeps
    /// `ours`. A path is a file, so two sides that changed different files
    /// of a directory merge cleanly. Without a base, every path either side
    /// has counts as changed. Subtrees with equal ids are never read, so the
    /// cost follows the size of the changes.
    ///
    /// # Errors
    ///
    /// Fails if a tree that differs cannot be read.
    pub fn merge(
        &self,
        base: Option<TreeId>,
        ours: TreeId,
        theirs: TreeId,
    ) -> Result<Merge, StoreError> {
        self.merge_with(base, ours, theirs, None)
    }

    /// Merges like [`Store::merge`], but where both sides changed a text
    /// file, the merged one holds both versions between conflict markers
    /// naming the sides with `labels`, see [`Conflict::mark`]. Binary files,
    /// and paths a directory is on either side of, keep `ours`.
    ///
    /// # Errors
    ///
    /// Fails if a tree that differs, or a file that conflicts, cannot be
    /// read.
    pub fn merge_marked(
        &self,
        base: Option<TreeId>,
        ours: TreeId,
        theirs: TreeId,
        labels: [&str; 2],
    ) -> Result<Merge, StoreError> {
        self.merge_with(base, ours, theirs, Some(labels))
    }

    fn merge_with(
        &self,
        base: Option<TreeId>,
        ours: TreeId,
        theirs: TreeId,
        labels: Option<[&str; 2]>,
    ) -> Result<Merge, StoreError> {
        let mut merge = Merge {
            tree: ours,
            trees: Vec::new(),
            files: Vec::new(),
            conflicts: Vec::new(),
        };
        let merged = self.merge_dir(
            &RepoPath::root(),
            base,
            Some(ours),
            Some(theirs),
            labels,
            &mut merge,
        )?;
        merge.tree = if let Some(tree) = merged {
            tree
        } else {
            let empty = Tree::default().encode();
            let id = empty.id().into();
            merge.trees.push(empty);
            id
        };
        merge.trees.sort_unstable_by_key(EncodedObject::id);
        merge.trees.dedup_by_key(|tree| tree.id());
        merge.files.sort_unstable_by_key(EncodedObject::id);
        merge.files.dedup_by_key(|file| file.id());
        merge.conflicts.sort_unstable();
        Ok(merge)
    }

    /// Merges three versions of the directory `dir`, `None` standing for one
    /// that doesn't exist, returning the merged one, or `None` if it's
    /// empty.
    fn merge_dir(
        &self,
        dir: &RepoPath,
        base: Option<TreeId>,
        ours: Option<TreeId>,
        theirs: Option<TreeId>,
        labels: Option<[&str; 2]>,
        merge: &mut Merge,
    ) -> Result<Option<TreeId>, StoreError> {
        if ours == theirs || base == theirs {
            return Ok(ours);
        }
        if base == ours {
            return Ok(theirs);
        }
        let read = |id: Option<TreeId>| id.map(|id| self.get(id)).transpose();
        let (base, ours, theirs): (Tree, Tree, Tree) = (
            read(base)?.unwrap_or_default(),
            read(ours)?.unwrap_or_default(),
            read(theirs)?.unwrap_or_default(),
        );
        let mut names: Vec<&EntryName> = [&base, &ours, &theirs]
            .into_iter()
            .flat_map(|tree| tree.iter().map(|(name, _)| name))
            .collect();
        names.sort_unstable();
        names.dedup();
        let mut merged = Tree::default();
        for name in names {
            let [base, ours, theirs] =
                [&base, &ours, &theirs].map(|tree| tree.get(name.as_str()).copied());
            let entry = if ours == theirs || base == theirs {
                ours
            } else if base == ours {
                theirs
            } else if ours.is_none_or(|ours| ours.kind.is_tree())
                && theirs.is_none_or(|theirs| theirs.kind.is_tree())
            {
                // Both sides changed a directory, or one changed it and the
                // other removed it: merge it file by file.
                let subtree = |entry: Option<TreeEntry>| entry.and_then(TreeEntry::subtree);
                self.merge_dir(
                    &dir.join(name),
                    subtree(base),
                    subtree(ours),
                    subtree(theirs),
                    labels,
                    merge,
                )?
                .map(|tree| TreeEntry {
                    kind: EntryKind::Tree,
                    id: tree.into(),
                })
            } else {
                merge.conflicts.push(dir.join(name));
                match labels {
                    Some(labels) => self.mark(ours, theirs, labels, merge)?,
                    None => ours,
                }
            };
            if let Some(entry) = entry {
                merged.insert(name.clone(), entry);
            }
        }
        if merged.is_empty() {
            return Ok(None);
        }
        let encoded = merged.encode();
        let id = encoded.id().into();
        merge.trees.push(encoded);
        Ok(Some(id))
    }

    /// The entry of a file both sides changed, `ours` and `theirs`, that
    /// holds both versions between conflict markers: a file on the side
    /// that removed it is empty. Keeps `ours` if either side isn't a
    /// regular file, or is binary or missing from the store.
    fn mark(
        &self,
        ours: Option<TreeEntry>,
        theirs: Option<TreeEntry>,
        labels: [&str; 2],
        merge: &mut Merge,
    ) -> Result<Option<TreeEntry>, StoreError> {
        let content = |entry: Option<TreeEntry>| match entry {
            None => Ok(Some(Vec::new())),
            Some(entry) if matches!(entry.kind, EntryKind::File | EntryKind::Executable) => {
                self.try_blob(entry.id)
            }
            Some(_) => Ok(None),
        };
        let (Some(our_content), Some(their_content)) = (content(ours)?, content(theirs)?) else {
            return Ok(ours);
        };
        let Some(marked) = Conflict::mark(&our_content, &their_content, labels) else {
            return Ok(ours);
        };
        let file = EncodedObject::blob(&marked);
        let kind = ours.or(theirs).map_or(EntryKind::File, |entry| entry.kind);
        let entry = TreeEntry {
            kind,
            id: file.id(),
        };
        merge.files.push(file);
        Ok(Some(entry))
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

    /// The files of `tree`, by path, with their contents' ids.
    fn files(store: &Store, tree: TreeId) -> Vec<(String, ObjectId)> {
        let empty = build(store, &[]);
        store
            .diff(empty, tree)
            .unwrap()
            .into_iter()
            .map(|change| (change.path.as_str().to_owned(), change.new.unwrap().id))
            .collect()
    }

    #[test]
    fn merges_changes_to_different_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        let base = build(
            &store,
            &[
                ("src/a", "a"),
                ("src/b", "b"),
                ("gone/x", "x"),
                ("same", "s"),
            ],
        );
        // Ours edits a file and removes a directory; theirs edits another
        // file of the same directory, adds one, and makes the same edit.
        let ours = build(&store, &[("src/a", "a2"), ("src/b", "b"), ("same", "s2")]);
        let theirs = build(
            &store,
            &[
                ("src/a", "a"),
                ("src/b", "b2"),
                ("gone/x", "x"),
                ("new/deep", "n"),
                ("same", "s2"),
            ],
        );
        let merge = store.merge(Some(base), ours, theirs).unwrap();
        assert_eq!(merge.conflicts, Vec::<RepoPath>::new());
        let expected = build(
            &store,
            &[
                ("src/a", "a2"),
                ("src/b", "b2"),
                ("new/deep", "n"),
                ("same", "s2"),
            ],
        );
        assert_eq!(files(&store, merge.tree), files(&store, expected));
        assert_eq!(merge.tree, expected, "trees are canonical");
        assert!(
            merge.trees.iter().any(|tree| tree.id() == expected.into()),
            "the merged trees are returned to be written"
        );

        // Merging onto an unchanged base reads nothing and makes no tree.
        let unchanged = store.merge(Some(base), ours, base).unwrap();
        assert_eq!((unchanged.tree, unchanged.trees), (ours, Vec::new()));
    }

    #[test]
    fn conflicts_keep_ours() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        let base = build(
            &store,
            &[("edit", "1"), ("dir/removed", "r"), ("file", "f")],
        );
        let ours = build(
            &store,
            &[("edit", "ours"), ("dir/removed", "r2"), ("file", "f")],
        );
        let theirs = build(
            &store,
            &[("edit", "theirs"), ("file/now-dir", "d"), ("other", "o")],
        );
        let merge = store.merge(Some(base), ours, theirs).unwrap();
        let paths: Vec<_> = merge.conflicts.iter().map(RepoPath::as_str).collect();
        assert_eq!(
            paths,
            ["dir/removed", "edit"],
            "both edited `edit`; ours edited what theirs removed"
        );
        let expected = build(
            &store,
            &[
                ("edit", "ours"),
                ("dir/removed", "r2"),
                ("file/now-dir", "d"),
                ("other", "o"),
            ],
        );
        assert_eq!(files(&store, merge.tree), files(&store, expected));

        // Without a base, both sides added everything.
        let unrelated = store.merge(None, ours, theirs).unwrap();
        let paths: Vec<_> = unrelated.conflicts.iter().map(RepoPath::as_str).collect();
        assert_eq!(paths, ["edit", "file"]);
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
