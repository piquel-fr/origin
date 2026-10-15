//! Workspaces: working copies that share a repository's store and index,
//! like git worktrees, each with its own current change and stat cache.
//!
//! The main working copy's `.origin/workspaces/` has a file for each other
//! workspace, named by the hash of its root and holding that root. Records
//! are immutable, and only target chains and the index are locked, so
//! commands run concurrently in different workspaces.
//!
//! Creating a workspace registers it, checks out its files, and only then
//! publishes its `.origin/workspace`, so a crash never leaves partial files
//! that a snapshot would record: it leaves a workspace that `origin
//! workspace rm` removes. Removing a workspace snapshots it first, so its
//! work stays a revision of its change.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{self, Path, PathBuf};

use origin_platform::DurableWrite;
use origin_shared::{ChangeId, Digest, EncodedObject, Record, Revision, Tree};

use super::{Workspace, WorkspaceError};

impl Workspace {
    /// The directory in the store's `.origin` with a file per workspace.
    const REGISTRY: &str = "workspaces";

    /// `origin workspace new [--change <change>] [<path>]`: creates a
    /// workspace at `path`, by default beside the main working copy, and
    /// checks out the change's head in it. Without a change, it starts a
    /// new one on the working copy's base, beside the working copy's
    /// change.
    ///
    /// # Errors
    ///
    /// Fails if `path` is in a working copy or has files, the change cannot
    /// be found or diverged, or the workspace cannot be written.
    pub fn add_workspace(
        &mut self,
        change: Option<&str>,
        path: Option<&Path>,
    ) -> Result<(), WorkspaceError> {
        let working_copy = self.refresh()?;
        let (id, revision, records) = if let Some(change) = change {
            let change = working_copy.find_change(change)?;
            let head = Self::single_head(&working_copy.index, change)?;
            (head, self.store.get(head)?, Vec::new())
        } else {
            let description = EncodedObject::blob(b"");
            let base = working_copy.revision.base;
            let revision = Revision {
                change: ChangeId::random().map_err(WorkspaceError::Random)?,
                predecessors: Vec::new(),
                base,
                deps: Vec::new(),
                snapshot: self.base_tree(base)?,
                description: description.id().into(),
                author: self.config.user(),
            };
            let encoded = revision.encode();
            let id = encoded.id().into();
            let conflicts = self.base_conflicts(base)?;
            let layers = self.revision_layers(vec![description], &encoded, &conflicts);
            (id, revision, layers)
        };
        let root = self.new_root(path, revision.change)?;
        self.write(&records)?;
        self.write_durably(self.registered(&root), &Self::path_bytes(&root))?;
        let mut workspace = Self {
            root,
            store: self.store.clone(),
            config: self.config.clone(),
            revision: id,
        };
        let empty = Tree::default().encode().id().into();
        workspace.checkout(empty, revision.snapshot, None)?;
        let repository = Self::path_bytes(self.store.root());
        workspace.write_durably(workspace.meta(Self::REPOSITORY), &repository)?;
        workspace.point_at(id)?;
        println!("Created workspace {}", workspace.root.display());
        println!("Working copy: {}", workspace.headline(&revision)?);
        println!("On top of: {}", workspace.describe_base(revision.base)?);
        Ok(())
    }

    /// `origin workspace list`: prints each workspace's root and working
    /// copy, as of its last snapshot, marking this one.
    ///
    /// # Errors
    ///
    /// Fails if the workspaces cannot be listed, or the records they point
    /// at cannot be read.
    pub fn list_workspaces(&self) -> Result<(), WorkspaceError> {
        for root in self.workspaces()? {
            let marker = if root == self.root { '@' } else { ' ' };
            let state = if root.is_dir() {
                match Self::read_pointer(&root)? {
                    Some(id) => self.headline(&self.store.get(id)?)?,
                    None => "(incomplete)".to_owned(),
                }
            } else {
                "(missing)".to_owned()
            };
            println!("{marker} {}: {state}", root.display());
        }
        Ok(())
    }

    /// `origin workspace rm <path>`: snapshots a workspace, so its work
    /// stays a revision of its change, then deletes it.
    ///
    /// # Errors
    ///
    /// Fails if `path` is the main working copy or no workspace, or the
    /// workspace cannot be snapshotted or deleted.
    pub fn remove_workspace(&self, path: &Path) -> Result<(), WorkspaceError> {
        let root = Self::canonical(path)?;
        if root == self.main_root() {
            return Err(WorkspaceError::RemoveMain(root));
        }
        let registered = self.registered(&root);
        if !registered.exists() {
            return Err(WorkspaceError::UnknownWorkspace(root));
        }
        if Self::read_pointer(&root)?.is_some() {
            let mut workspace = Self::open(&root, self.config.clone())?;
            let working_copy = workspace.refresh()?;
            println!(
                "Kept its working copy: {}",
                workspace.headline(&working_copy.revision)?
            );
        }
        match fs::remove_dir_all(&root) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(WorkspaceError::io(&root, error));
            }
            _ => {}
        }
        fs::remove_file(&registered).map_err(|source| WorkspaceError::io(&registered, source))?;
        self.store
            .sync_dir(&self.store.root().join(Self::REGISTRY))?;
        println!("Removed workspace {}", root.display());
        Ok(())
    }

    /// Where to create a workspace for `change`: `path`, or by default
    /// beside the main working copy. It must be empty, and in no working
    /// copy.
    fn new_root(&self, path: Option<&Path>, change: ChangeId) -> Result<PathBuf, WorkspaceError> {
        let root = if let Some(path) = path {
            Self::canonical(path)?
        } else {
            let main = self.main_root();
            let mut name = main.file_name().unwrap_or("workspace".as_ref()).to_owned();
            name.push(format!("-{}", change.short()));
            main.with_file_name(name)
        };
        if root
            .ancestors()
            .any(|dir| dir.join(Self::META_DIR).exists())
        {
            return Err(WorkspaceError::Nested(root));
        }
        match fs::read_dir(&root).map(|mut entries| entries.next().is_some()) {
            Ok(true) => Err(WorkspaceError::NotEmpty(root)),
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(WorkspaceError::io(&root, error))
            }
            _ => Ok(root),
        }
    }

    /// The roots of every workspace: the main working copy's, then the
    /// others' in order.
    pub(super) fn workspaces(&self) -> Result<Vec<PathBuf>, WorkspaceError> {
        let dir = self.store.root().join(Self::REGISTRY);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(vec![self.main_root().to_owned()]);
            }
            Err(source) => return Err(WorkspaceError::io(&dir, source)),
        };
        let mut roots = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| WorkspaceError::io(&dir, source))?;
            if DurableWrite::<()>::is_temporary(&entry.file_name()) {
                continue;
            }
            roots.push(Self::read_path(&entry.path())?);
        }
        roots.sort();
        roots.insert(0, self.main_root().to_owned());
        Ok(roots)
    }

    /// The root of the main working copy, whose `.origin` holds the store.
    fn main_root(&self) -> &Path {
        self.store.root().parent().unwrap_or(Path::new("/"))
    }

    /// The registry file of the workspace at `root`.
    fn registered(&self, root: &Path) -> PathBuf {
        let name = Digest::of(root.as_os_str().as_bytes()).to_hex();
        self.store.root().join(Self::REGISTRY).join(name)
    }

    /// A path as a line of a file, which [`Workspace::read_path`] reads.
    fn path_bytes(path: &Path) -> Vec<u8> {
        let mut bytes = path.as_os_str().as_bytes().to_vec();
        bytes.push(b'\n');
        bytes
    }

    /// The path a file written with [`Workspace::path_bytes`] holds.
    pub(super) fn read_path(file: &Path) -> Result<PathBuf, WorkspaceError> {
        let bytes = fs::read(file).map_err(|source| WorkspaceError::io(file, source))?;
        let path = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
        Ok(PathBuf::from(OsStr::from_bytes(path)))
    }

    /// `path` made absolute, without symlinks or `..`. It may not exist,
    /// but its parent must.
    fn canonical(path: &Path) -> Result<PathBuf, WorkspaceError> {
        let absolute = path::absolute(path).map_err(|source| WorkspaceError::io(path, source))?;
        if let Ok(resolved) = fs::canonicalize(&absolute) {
            return Ok(resolved);
        }
        let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) else {
            return Err(WorkspaceError::io(path, io::ErrorKind::NotFound.into()));
        };
        let parent =
            fs::canonicalize(parent).map_err(|source| WorkspaceError::io(parent, source))?;
        Ok(parent.join(name))
    }
}
