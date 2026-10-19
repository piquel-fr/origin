//! Comparing trees, merging them, and looking up what they hold.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::PathBuf;

use origin_platform::Platform;
use origin_shared::{
    Digest, EncodedObject, EntryKind, EntryName, Inputs, ObjectId, Record, RepoPath, Tree,
    TreeEntry, TreeId,
};

use crate::conflict::{Conflicted, Conflicts};
use crate::store::{Batch, Store, StoreError};

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
    /// The paths both sides changed, each its own way, with what the merge
    /// left there.
    pub conflicts: Conflicts,
}

/// What a merge leaves where both sides changed a path, each its own way.
#[derive(Clone, Copy, Debug)]
enum OnConflict<'l> {
    /// `ours`.
    KeepOurs,
    /// Both versions between conflict markers naming the sides with these
    /// labels, see [`Conflicts::mark`].
    Mark([&'l str; 2]),
    /// [`Store::UNKNOWN`], a version neither side can have.
    Unknown,
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
        self.merge_with(base, ours, theirs, OnConflict::KeepOurs)
    }

    /// Merges like [`Store::merge`], but where both sides changed a text
    /// file, the merged one holds both versions between conflict markers
    /// naming the sides with `labels`, see [`Conflicts::mark`]. Binary files,
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
        self.merge_with(base, ours, theirs, OnConflict::Mark(labels))
    }

    /// The entry a merge of merge bases has where they conflict: an id no
    /// object has, so it equals neither side of a merge from it. A merge
    /// never reads under it either: the bases may have had a directory
    /// there, so any change at or under its path conflicts.
    const UNKNOWN: TreeEntry = TreeEntry {
        kind: EntryKind::File,
        id: ObjectId::from_bytes([0; ObjectId::LEN]),
    };

    /// Merges like [`Store::merge`], but where both sides changed a path,
    /// the merged tree has [`Store::UNKNOWN`]: how merge bases are merged
    /// into one, so a merge from it conflicts there unless both sides
    /// agree.
    ///
    /// # Errors
    ///
    /// Fails if a tree that differs cannot be read.
    pub fn merge_bases(
        &self,
        base: Option<TreeId>,
        ours: TreeId,
        theirs: TreeId,
    ) -> Result<Merge, StoreError> {
        self.merge_with(base, ours, theirs, OnConflict::Unknown)
    }

    fn merge_with(
        &self,
        base: Option<TreeId>,
        ours: TreeId,
        theirs: TreeId,
        on_conflict: OnConflict<'_>,
    ) -> Result<Merge, StoreError> {
        let mut merge = Merge {
            tree: ours,
            trees: Vec::new(),
            files: Vec::new(),
            conflicts: Conflicts::default(),
        };
        let merged = self.merge_dir(
            &RepoPath::root(),
            base,
            Some(ours),
            Some(theirs),
            on_conflict,
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
        on_conflict: OnConflict<'_>,
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
            } else if base != Some(Self::UNKNOWN)
                && ours.is_none_or(|ours| ours.kind.is_tree())
                && theirs.is_none_or(|theirs| theirs.kind.is_tree())
            {
                // Both sides changed a directory, or one changed it and the
                // other removed it: merge it file by file, unless what it was
                // is unknown, which would take what one side removed for
                // what the other added.
                let subtree = |entry: Option<TreeEntry>| entry.and_then(TreeEntry::subtree);
                self.merge_dir(
                    &dir.join(name),
                    subtree(base),
                    subtree(ours),
                    subtree(theirs),
                    on_conflict,
                    merge,
                )?
                .map(|tree| TreeEntry {
                    kind: EntryKind::Tree,
                    id: tree.into(),
                })
            } else {
                let written = match on_conflict {
                    OnConflict::KeepOurs => ours,
                    OnConflict::Mark(labels) => self.mark(ours, theirs, labels, merge)?,
                    OnConflict::Unknown => Some(Self::UNKNOWN),
                };
                // Paths come in order, each once.
                merge.conflicts.push(Conflicted {
                    path: dir.join(name),
                    written: written.map(|entry| entry.id),
                });
                written
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
        let Some(marked) = Conflicts::mark(&our_content, &their_content, labels) else {
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

/// A snapshot being fetched, marked durably in the store's `fetching/`, so
/// an interrupted fetch, which published trees before what they hold, is
/// never taken for a whole snapshot: see [`Store::holds`]. Dropping it
/// without [`Fetching::done`] leaves the mark, and fetching the snapshot
/// again completes it.
#[derive(Debug)]
pub struct Fetching {
    path: PathBuf,
}

impl Fetching {
    /// Removes the mark, once everything fetched is durable. A crash that
    /// loses the removal only fetches the snapshot again.
    ///
    /// # Errors
    ///
    /// Fails if the mark cannot be removed.
    pub fn done(self) -> Result<(), StoreError> {
        match fs::remove_file(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(StoreError::io(&self.path, error))
            }
            _ => Ok(()),
        }
    }
}

impl<P: Platform> Store<P> {
    /// The directory marking the snapshots being fetched.
    const FETCHING: &str = "fetching";

    fn fetching_path(&self, snapshot: TreeId) -> PathBuf {
        self.root().join(Self::FETCHING).join(snapshot.to_hex())
    }

    /// Whether the store holds `snapshot` whole: its root, and no fetch of
    /// it that didn't finish (see [`Fetching`]).
    ///
    /// # Errors
    ///
    /// Fails like [`Store::contains`].
    pub fn holds(&self, snapshot: TreeId) -> Result<bool, StoreError> {
        Ok(self.contains(snapshot.into())? && !self.fetching_path(snapshot).exists())
    }

    /// Those of `snapshots` the store [holds](Store::holds) whole, in
    /// order.
    ///
    /// # Errors
    ///
    /// Fails like [`Store::contains`].
    pub fn held_whole(
        &self,
        snapshots: impl IntoIterator<Item = TreeId>,
    ) -> Result<Vec<TreeId>, StoreError> {
        let mut held = Vec::new();
        for snapshot in snapshots {
            if self.holds(snapshot)? {
                held.push(snapshot);
            }
        }
        Ok(held)
    }

    /// Marks `snapshot` as being fetched, durably before anything `batch`
    /// stages is published, at no sync of its own: the batch syncs the mark
    /// with what it stages.
    ///
    /// # Errors
    ///
    /// Fails if the mark, or the directory holding it, cannot be created.
    pub fn fetching(&self, snapshot: TreeId, batch: &Batch<'_, P>) -> Result<Fetching, StoreError> {
        let dir = self.root().join(Self::FETCHING);
        if !dir.is_dir() {
            fs::create_dir_all(&dir).map_err(|source| StoreError::io(&dir, source))?;
            self.sync_dir(self.root())?;
        }
        let path = dir.join(snapshot.to_hex());
        File::create(&path).map_err(|source| StoreError::io(&path, source))?;
        batch.sync_first(&path);
        batch.sync_first(dir);
        Ok(Fetching { path })
    }
}

/// An object of a snapshot, and where it is: [`Store::added`] lists them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    /// Its path: the root for the snapshot's own tree.
    pub path: RepoPath,
    /// The object, and its kind.
    pub entry: TreeEntry,
}

/// Which part of a snapshot [`Store::added`] walks.
struct Scope<'p>(&'p [RepoPath]);

impl Scope<'_> {
    /// Whether everything at `path` and under it is in scope.
    fn covers(&self, path: &RepoPath) -> bool {
        self.0.is_empty() || self.0.iter().any(|prefix| path.starts_with(prefix))
    }

    /// Whether `path` is a directory on the way to something in scope.
    fn leads_in(&self, path: &RepoPath) -> bool {
        self.0.iter().any(|prefix| prefix.starts_with(path))
    }
}

impl<P: Platform> Store<P> {
    /// The objects of `snapshot` that none of the snapshots `have` holds at
    /// the same path, parents before children: what someone holding `have`
    /// lacks of it. With `paths`, only those under them, and the trees on
    /// the way. Subtrees a `have` holds whole are skipped without being
    /// read, so the cost follows the size of the change. Objects appear
    /// once, at their first path.
    ///
    /// # Errors
    ///
    /// Fails if a tree of `snapshot`, or of a `have`, cannot be read.
    pub fn added(
        &self,
        snapshot: TreeId,
        have: &[TreeId],
        paths: &[RepoPath],
    ) -> Result<Vec<Placed>, StoreError> {
        let mut added = Vec::new();
        let mut seen = HashSet::new();
        self.add_dir(
            &RepoPath::root(),
            snapshot,
            have,
            &Scope(paths),
            &mut seen,
            &mut added,
        )?;
        Ok(added)
    }

    fn add_dir(
        &self,
        dir: &RepoPath,
        tree: TreeId,
        have: &[TreeId],
        scope: &Scope<'_>,
        seen: &mut HashSet<ObjectId>,
        added: &mut Vec<Placed>,
    ) -> Result<(), StoreError> {
        if have.contains(&tree) {
            return Ok(());
        }
        if seen.insert(tree.into()) {
            added.push(Placed {
                path: dir.clone(),
                entry: TreeEntry {
                    kind: EntryKind::Tree,
                    id: tree.into(),
                },
            });
        }
        let have: Vec<Tree> = have
            .iter()
            .map(|&id| self.get(id))
            .collect::<Result<_, _>>()?;
        for (name, &entry) in &self.get(tree)? {
            let path = dir.join(name);
            let covered = scope.covers(&path);
            if !(covered || entry.kind.is_tree() && scope.leads_in(&path)) {
                continue;
            }
            let held = have.iter().filter_map(|tree| tree.get(name.as_str()));
            if held.clone().any(|held| *held == entry) {
                continue;
            }
            match entry.subtree() {
                Some(subtree) => {
                    let held: Vec<_> = held.filter_map(|held| held.subtree()).collect();
                    self.add_dir(&path, subtree, &held, scope, seen, added)?;
                }
                None if seen.insert(entry.id) => added.push(Placed { path, entry }),
                None => {}
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
    fn lists_what_the_haves_lack() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        let old = build(
            &store,
            &[
                ("same/a", "1"),
                ("edit/b", "2"),
                ("file", "3"),
                ("docs/x", "4"),
            ],
        );
        let new = build(
            &store,
            &[
                ("same/a", "1"),
                ("edit/b", "changed"),
                ("file/now-dir", "4"),
                ("docs/x", "4"),
                ("docs/copy", "4"),
            ],
        );
        let paths = |placed: Vec<Placed>| -> Vec<String> {
            placed
                .iter()
                .map(|placed| placed.path.to_string())
                .collect()
        };

        assert_eq!(
            paths(store.added(new, &[old], &[]).unwrap()),
            ["/", "docs", "docs/copy", "edit", "edit/b", "file"],
            "`file/now-dir` has the content of `docs/copy`, listed first"
        );
        assert_eq!(
            store.added(new, &[], &[]).unwrap().len(),
            8,
            "5 trees, and 5 files of 3 contents"
        );
        assert_eq!(store.added(new, &[new], &[]).unwrap(), []);
        let empty = build(&store, &[]);
        assert_eq!(
            paths(store.added(new, &[empty, old], &[]).unwrap()),
            paths(store.added(new, &[old], &[]).unwrap()),
            "any have holding an object is enough"
        );

        let only = |path: &str| [RepoPath::new(path).unwrap()];
        assert_eq!(
            paths(store.added(new, &[], &only("edit/b")).unwrap()),
            ["/", "edit", "edit/b"],
            "the trees on the way, then the path"
        );
        assert_eq!(
            paths(store.added(new, &[old], &only("same")).unwrap()),
            ["/"]
        );
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
        assert_eq!(merge.conflicts, Conflicts::default());
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
        let paths: Vec<_> = merge
            .conflicts
            .paths()
            .iter()
            .map(ToString::to_string)
            .collect();
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
        let paths: Vec<_> = unrelated
            .conflicts
            .paths()
            .iter()
            .map(ToString::to_string)
            .collect();
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
