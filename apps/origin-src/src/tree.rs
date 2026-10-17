//! Comparing trees, merging them, and looking up what they hold.
//!
//! In a repository whose remote withholds some paths (see
//! [`Store::with_withheld`]), nothing here reads what it lacks there:
//! diffs skip withheld paths, merges compare them by id, and walks leave
//! them out.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use origin_platform::Platform;
use origin_shared::access::{Level, PathAccess};
use origin_shared::{
    Base, Digest, EncodedObject, EntryKind, EntryName, Inputs, ObjectId, Record, RecordId,
    RepoPath, Tree, TreeEntry, TreeId,
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
    /// What this repository may not read at or under `dir`, if anything:
    /// tree operations check paths only while it's some.
    fn withheld_under(&self, dir: &RepoPath) -> Option<&PathAccess> {
        Self::unsettled(self.readable.as_deref(), dir)
    }

    /// Whether a tree operation may read the entry at `path` of a
    /// directory whose withheld paths are `withheld`: a file it reads, or a
    /// directory it reads something under.
    fn readable(withheld: Option<&PathAccess>, path: &RepoPath, entry: &TreeEntry) -> bool {
        withheld.is_none_or(|access| access.reads_entry(path, entry))
    }

    /// Lists the files that differ between two trees, sorted by path.
    /// Subtrees with equal ids are skipped without being read, so the cost
    /// follows the size of the change. Withheld paths are skipped.
    ///
    /// # Errors
    ///
    /// Fails if a tree that differs cannot be read.
    pub fn diff(&self, old: TreeId, new: TreeId) -> Result<Vec<FileChange>, StoreError> {
        self.diff_at(&RepoPath::root(), Some(old), Some(new))
    }

    /// Lists the files that differ between two versions of the directory
    /// `dir`, like [`Store::diff`], `None` standing for one that doesn't
    /// exist.
    ///
    /// # Errors
    ///
    /// Fails if a tree that differs cannot be read.
    pub fn diff_at(
        &self,
        dir: &RepoPath,
        old: Option<TreeId>,
        new: Option<TreeId>,
    ) -> Result<Vec<FileChange>, StoreError> {
        let mut changes = Vec::new();
        self.diff_dir(dir, old, new, &mut changes)?;
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
        let withheld = self.withheld_under(dir);
        let mut names: Vec<&EntryName> =
            old.iter().chain(new.iter()).map(|(name, _)| name).collect();
        names.sort_unstable();
        names.dedup();
        for name in names {
            let path = dir.join(name);
            let readable = |entry: Option<TreeEntry>| {
                entry.filter(|entry| Self::readable(withheld, &path, entry))
            };
            let (old, new) = (
                readable(old.get(name.as_str()).copied()),
                readable(new.get(name.as_str()).copied()),
            );
            if old == new {
                continue;
            }
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
    /// cost follows the size of the changes. A withheld directory both
    /// sides changed is a conflict, as a file is.
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
        let withheld = self.withheld_under(dir);
        let mut merged = Tree::default();
        for name in names {
            let [base, ours, theirs] =
                [&base, &ours, &theirs].map(|tree| tree.get(name.as_str()).copied());
            let readable = withheld.is_none_or(|access| access.reads_under(&dir.join(name)));
            let entry = if ours == theirs || base == theirs {
                ours
            } else if base == ours {
                theirs
            } else if readable
                && ours.is_none_or(|ours| ours.kind.is_tree())
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

impl<P: Platform> Store<P> {
    /// The working copy's files, the tree `files`, with what `kept`, the
    /// revision it was, holds at the paths withheld from this repository:
    /// files the user cannot read are never in the working copy, and stay
    /// in its snapshots as they were. Returns the tree, and the trees it
    /// made, which the store may not have yet. Only the directories on the
    /// way to withheld paths are read.
    ///
    /// # Errors
    ///
    /// Fails if a tree on the way cannot be read.
    pub fn keep_withheld(
        &self,
        files: TreeId,
        kept: TreeId,
    ) -> Result<(TreeId, Vec<EncodedObject>), StoreError> {
        let mut trees = Vec::new();
        let tree = self.keep_dir(&RepoPath::root(), Some(files), Some(kept), &mut trees)?;
        let tree = tree.unwrap_or_else(|| {
            let empty = Tree::default().encode();
            let id = empty.id().into();
            trees.push(empty);
            id
        });
        Ok((tree, trees))
    }

    /// Keeps the withheld paths of `kept` in `files`, two versions of `dir`,
    /// `None` standing for one that doesn't exist, returning the result, or
    /// `None` if it's empty.
    fn keep_dir(
        &self,
        dir: &RepoPath,
        files: Option<TreeId>,
        kept: Option<TreeId>,
        trees: &mut Vec<EncodedObject>,
    ) -> Result<Option<TreeId>, StoreError> {
        let Some(access) = self.withheld_under(dir) else {
            return Ok(files);
        };
        if files == kept {
            return Ok(files);
        }
        let read = |id: Option<TreeId>| id.map(|id| self.get(id)).transpose();
        let (on_disk, held): (Tree, Tree) = (
            read(files)?.unwrap_or_default(),
            read(kept)?.unwrap_or_default(),
        );
        let mut names: Vec<&EntryName> = on_disk
            .iter()
            .chain(held.iter())
            .map(|(name, _)| name)
            .collect();
        names.sort_unstable();
        names.dedup();
        let mut merged = Tree::default();
        for name in names {
            let path = dir.join(name);
            let file = on_disk.get(name.as_str()).copied();
            let kept = held.get(name.as_str()).copied();
            let is_tree =
                |entry: Option<TreeEntry>| entry.is_some_and(|entry| entry.kind.is_tree());
            let entry = if is_tree(file) || file.is_none() && is_tree(kept) {
                // A directory: keep what's withheld under it.
                if access.reads_under(&path) {
                    let subtree = |entry: Option<TreeEntry>| entry.and_then(TreeEntry::subtree);
                    self.keep_dir(&path, subtree(file), subtree(kept), trees)?
                        .map(|tree| TreeEntry {
                            kind: EntryKind::Tree,
                            id: tree.into(),
                        })
                } else {
                    kept
                }
            } else if access.reads(&path) {
                file
            } else {
                kept
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
        trees.push(encoded);
        Ok(Some(id))
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
    /// The snapshot `base` names: an acceptance's state or a revision's
    /// snapshot.
    ///
    /// # Errors
    ///
    /// Fails if the record cannot be read.
    pub fn base_snapshot(&self, base: Base) -> Result<TreeId, StoreError> {
        Ok(match base {
            Base::Acceptance(id) => self.get(id)?.body().state,
            Base::Revision(id) => self.get(id)?.snapshot,
        })
    }

    /// The snapshot of a record: a revision's, an acceptance's state, or a
    /// snapshot itself. Statements have none.
    ///
    /// # Errors
    ///
    /// Fails if the record cannot be read.
    pub fn snapshot_of(&self, record: RecordId) -> Result<Option<TreeId>, StoreError> {
        Ok(match record {
            RecordId::Snapshot(id) => Some(id),
            RecordId::Revision(id) => Some(self.get(id)?.snapshot),
            RecordId::Acceptance(id) => Some(self.get(id)?.body().state),
            RecordId::Statement(_) => None,
        })
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
    /// the way. With `access`, only what it reads, and the trees on the way
    /// to it: the rest keeps its place in the trees, but its content is
    /// withheld. Subtrees a `have` holds whole are skipped without being
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
        access: Option<&PathAccess>,
    ) -> Result<Vec<Placed>, StoreError> {
        let mut added = Vec::new();
        let mut seen = HashSet::new();
        let walk = Walk {
            scope: Scope(paths),
            seen: &mut seen,
            added: &mut added,
        };
        // What this repository lacks, it cannot give.
        let access = access.or(self.readable.as_deref());
        let root = RepoPath::root();
        walk.dir(self, &root, snapshot, have, Self::unsettled(access, &root))?;
        Ok(added)
    }

    /// `access`, if what it allows at and under `path` isn't settled to
    /// reading: walks only check paths while it isn't.
    fn unsettled<'a>(access: Option<&'a PathAccess>, path: &RepoPath) -> Option<&'a PathAccess> {
        access.filter(|access| access.settled(path).is_none_or(|level| level < Level::Read))
    }

    /// The first path of `snapshot` whose object is neither in `supplied`
    /// nor at the same path of one of the snapshots `references`, if any.
    /// A revision may only hold content its writer supplied, or that a
    /// snapshot it refers to holds at the same path: then every path of it
    /// holds what someone who could read that path put there. Subtrees a
    /// reference holds whole are skipped without being read.
    ///
    /// # Errors
    ///
    /// Fails if a tree of `snapshot` or of a reference cannot be read.
    pub fn unsupplied(
        &self,
        snapshot: TreeId,
        references: &[TreeId],
        supplied: &HashSet<ObjectId>,
    ) -> Result<Option<RepoPath>, StoreError> {
        self.unsupplied_dir(&RepoPath::root(), snapshot, references, supplied)
    }

    fn unsupplied_dir(
        &self,
        dir: &RepoPath,
        tree: TreeId,
        references: &[TreeId],
        supplied: &HashSet<ObjectId>,
    ) -> Result<Option<RepoPath>, StoreError> {
        if references.contains(&tree) {
            return Ok(None);
        }
        if !supplied.contains(&tree.into()) {
            return Ok(Some(dir.clone()));
        }
        let references: Vec<Tree> = references
            .iter()
            .map(|&id| self.get(id))
            .collect::<Result<_, _>>()?;
        for (name, &entry) in &self.get(tree)? {
            let path = dir.join(name);
            let held = references.iter().filter_map(|tree| tree.get(name.as_str()));
            if held.clone().any(|held| *held == entry) {
                continue;
            }
            let unsupplied = match entry.subtree() {
                Some(subtree) => {
                    let held: Vec<_> = held.filter_map(|held| held.subtree()).collect();
                    self.unsupplied_dir(&path, subtree, &held, supplied)?
                }
                None => (!supplied.contains(&entry.id)).then_some(path),
            };
            if unsupplied.is_some() {
                return Ok(unsupplied);
            }
        }
        Ok(None)
    }
}

/// A walk of [`Store::added`].
struct Walk<'w> {
    scope: Scope<'w>,
    seen: &'w mut HashSet<ObjectId>,
    added: &'w mut Vec<Placed>,
}

impl Walk<'_> {
    fn dir<P: Platform>(
        mut self,
        store: &Store<P>,
        dir: &RepoPath,
        tree: TreeId,
        have: &[TreeId],
        access: Option<&PathAccess>,
    ) -> Result<Self, StoreError> {
        if have.contains(&tree) {
            return Ok(self);
        }
        if self.seen.insert(tree.into()) {
            self.added.push(Placed {
                path: dir.clone(),
                entry: TreeEntry {
                    kind: EntryKind::Tree,
                    id: tree.into(),
                },
            });
        }
        let have: Vec<Tree> = have
            .iter()
            .map(|&id| store.get(id))
            .collect::<Result<_, _>>()?;
        for (name, &entry) in &store.get(tree)? {
            let path = dir.join(name);
            let covered = self.scope.covers(&path);
            if !(covered || entry.kind.is_tree() && self.scope.leads_in(&path)) {
                continue;
            }
            if !Store::<P>::readable(access, &path, &entry) {
                continue;
            }
            let held = have.iter().filter_map(|tree| tree.get(name.as_str()));
            if held.clone().any(|held| *held == entry) {
                continue;
            }
            match entry.subtree() {
                Some(subtree) => {
                    let held: Vec<_> = held.filter_map(|held| held.subtree()).collect();
                    let access = Store::<P>::unsettled(access, &path);
                    self = self.dir(store, &path, subtree, &held, access)?;
                }
                None if self.seen.insert(entry.id) => self.added.push(Placed { path, entry }),
                None => {}
            }
        }
        Ok(self)
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
            paths(store.added(new, &[old], &[], None).unwrap()),
            ["/", "docs", "docs/copy", "edit", "edit/b", "file"],
            "`file/now-dir` has the content of `docs/copy`, listed first"
        );
        assert_eq!(
            store.added(new, &[], &[], None).unwrap().len(),
            8,
            "5 trees, and 5 files of 3 contents"
        );
        assert_eq!(store.added(new, &[new], &[], None).unwrap(), []);
        let empty = build(&store, &[]);
        assert_eq!(
            paths(store.added(new, &[empty, old], &[], None).unwrap()),
            paths(store.added(new, &[old], &[], None).unwrap()),
            "any have holding an object is enough"
        );

        let only = |path: &str| [RepoPath::new(path).unwrap()];
        assert_eq!(
            paths(store.added(new, &[], &only("edit/b"), None).unwrap()),
            ["/", "edit", "edit/b"],
            "the trees on the way, then the path"
        );
        assert_eq!(
            paths(store.added(new, &[old], &only("same"), None).unwrap()),
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
    }
}

#[cfg(test)]
mod withheld_tests {
    use origin_platform::Native;
    use origin_shared::access::{Level, PathAccess};

    use super::tests::build;
    use super::*;

    fn path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    /// Snapshots of `files` with each version of `secrets/key`, in a store
    /// that lacks `secrets`, which its remote withholds.
    fn withheld(keys: &[&str]) -> (tempfile::TempDir, Store, Vec<TreeId>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        let snapshots: Vec<_> = keys
            .iter()
            .map(|key| build(&store, &[("src/lib.rs", "lib"), ("secrets/key", key)]))
            .collect();
        let secrets: Vec<ObjectId> = snapshots
            .iter()
            .map(|&snapshot| {
                Snapshots::new(&store)
                    .entry_at(snapshot, &path("secrets"))
                    .unwrap()
                    .unwrap()
                    .id
            })
            .collect();
        store.remove(&secrets).unwrap();
        let readable = PathAccess::new(Level::Write, [(path("secrets"), Level::None)]);
        (dir, store.with_withheld(readable), snapshots)
    }

    #[test]
    fn withheld_paths_are_never_read() {
        let (_dir, store, snapshots) = withheld(&["a", "b", "c"]);
        let [base, ours, theirs] = snapshots[..] else {
            unreachable!()
        };
        assert_eq!(
            store.diff(base, ours).unwrap(),
            [],
            "only the secret differs"
        );
        let merge = store.merge(Some(base), ours, theirs).unwrap();
        assert_eq!(
            merge.conflicts,
            [path("secrets")],
            "compared by id, as a file"
        );
        assert_eq!(store.merge(Some(base), base, theirs).unwrap().tree, theirs);
        let walked = store.added(ours, &[], &[], None).unwrap();
        assert!(
            walked
                .iter()
                .all(|placed| !placed.path.starts_with(&path("secrets")))
        );
    }

    #[test]
    fn snapshots_keep_withheld_paths_as_they_were() {
        let (_dir, store, snapshots) = withheld(&["a"]);
        let kept = snapshots[0];
        // The working copy has `src`, edited, and nothing of `secrets`.
        let files = build(&store, &[("src/lib.rs", "edited"), ("new", "n")]);
        let (tree, trees) = store.keep_withheld(files, kept).unwrap();
        store.write_all(&trees).unwrap();
        let changes = store.diff(kept, tree).unwrap();
        let paths: Vec<_> = changes.iter().map(|change| change.path.as_str()).collect();
        assert_eq!(paths, ["new", "src/lib.rs"]);
        let secrets = |tree| {
            Snapshots::new(&store)
                .entry_at(tree, &path("secrets"))
                .unwrap()
        };
        assert_eq!(secrets(tree), secrets(kept), "the secret stays as it was");

        let unchanged = build(&store, &[("src/lib.rs", "lib")]);
        assert_eq!(store.keep_withheld(unchanged, kept).unwrap().0, kept);
    }
}
