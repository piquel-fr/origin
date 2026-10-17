//! The sync commands: `clone`, `remote`, `fetch`, `pull` and `push`, and
//! fetching the snapshots a lazy clone lacks.
//!
//! A repository has at most one remote, named in `.origin/remote`, which
//! its workspaces share. A clone holds the chains of the targets it
//! follows, the records of the changes it knows, and only the snapshots it
//! read: whenever a command needs one it lacks, such as a checkout, it is
//! fetched from the remote, skipping what the working copy already holds.
//! Syncing never changes files under a running workspace: `pull` is a
//! fetch, then a move of the working copy.

use std::fs;
use std::io;
use std::path::{self, Path, PathBuf};
use std::slice;
use std::sync::OnceLock;

use origin_platform::Native;
use origin_shared::{
    Base, ChangeId, EncodedObject, Record, RecordId, Revision, RevisionId, TargetName, Tree,
};

use super::{Workspace, WorkspaceError};
use crate::config::UserConfig;
use crate::index::Index;
use crate::oplog::{Actor, Why};
use crate::remote::{Remote, RemoteUrl};
use crate::store::Store;
use crate::sync::{Advanced, Sync};

impl Workspace {
    /// The file in the repository's `.origin` naming its remote.
    const REMOTE: &str = "remote";

    /// `origin clone <url> [<path>] [--target <target>]`: creates a
    /// repository in `path`, by default named after the remote's, which
    /// follows `policy` and `target`, and starts a change on its tip. Only
    /// that snapshot is fetched.
    ///
    /// # Errors
    ///
    /// Fails if `path` has files, the remote cannot be reached or has no
    /// such target, or the repository cannot be written. A failed clone
    /// leaves nothing behind.
    pub fn clone(
        url: &str,
        path: Option<&Path>,
        target: &TargetName,
        config: UserConfig,
        actor: &Actor,
    ) -> Result<Self, WorkspaceError> {
        let url = RemoteUrl::parse(url)?;
        let path = match path {
            Some(path) => path.to_owned(),
            None => PathBuf::from(url.name().ok_or(WorkspaceError::Unnamed)?),
        };
        let root = path::absolute(&path).map_err(|source| WorkspaceError::io(&path, source))?;
        if fs::read_dir(&root).is_ok_and(|mut entries| entries.next().is_some()) {
            return Err(WorkspaceError::NotEmpty(root));
        }
        let created = !root.exists();
        let remote = Self::open_remote(&url, &config)?;
        let cloned = Self::clone_into(root.clone(), &url, remote, target, config, actor);
        if cloned.is_err() {
            // Best effort: the error says what went wrong.
            let _ = if created {
                fs::remove_dir_all(&root)
            } else {
                fs::remove_dir_all(root.join(Self::META_DIR))
            };
        }
        cloned
    }

    fn clone_into(
        root: PathBuf,
        url: &RemoteUrl,
        remote: Remote,
        target: &TargetName,
        config: UserConfig,
        actor: &Actor,
    ) -> Result<Self, WorkspaceError> {
        let meta = root.join(Self::META_DIR);
        let store = Store::new(meta, Native::default()).with_actor(actor.in_workspace(&root));
        store.init()?;
        Self::save_access(&store, &remote.whoami()?)?;
        let store = Self::withholding(store)?;
        let sync = Sync::new(&store, &remote, config.node().clone(), config.user());
        sync.fetch_chains(slice::from_ref(target))?;
        let tip = store
            .head(target)?
            .ok_or_else(|| WorkspaceError::UnknownTarget(target.clone()))?;
        let state = store.get(tip.acceptance)?.body().state;
        sync.need(RecordId::Acceptance(tip.acceptance), &[])?;

        let empty_tree = Tree::default().encode();
        let empty_description = EncodedObject::attachment(b"");
        let revision = Revision {
            change: ChangeId::random().map_err(WorkspaceError::Random)?,
            predecessors: Vec::new(),
            base: Base::Acceptance(tip.acceptance),
            deps: Vec::new(),
            snapshot: state,
            description: empty_description.id().into(),
            author: config.user(),
        };
        let encoded = revision.encode();
        let id: RevisionId = encoded.id().into();
        let mut workspace = Self::at(root, store, config, id);
        workspace.write(&[vec![empty_tree.clone(), empty_description], vec![encoded]])?;
        workspace.write_durably(
            workspace.store.root().join(Self::REMOTE),
            format!("{url}\n").as_bytes(),
        )?;
        workspace.remote = OnceLock::from(Some(remote));
        workspace.checkout(empty_tree.id().into(), RecordId::Acceptance(tip.acceptance))?;
        workspace.publish_pointer(None, id, Why::Command)?;
        println!(
            "Cloned {url} into {}, following `{target}` at #{}",
            workspace.root.display(),
            tip.seq
        );
        println!("Working copy: {}", workspace.headline(&revision)?);
        println!("On top of: {}", workspace.describe_base(revision.base)?);
        Ok(workspace)
    }

    /// The URL of the repository's remote, if it has one.
    fn remote_url(&self) -> Result<Option<RemoteUrl>, WorkspaceError> {
        let path = self.store.root().join(Self::REMOTE);
        match fs::read_to_string(&path) {
            Ok(url) => Ok(Some(RemoteUrl::parse(url.trim_end())?)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(WorkspaceError::io(&path, source)),
        }
    }

    /// Opens the remote at `url` for the user of `config`, with their
    /// token for it, if they have one.
    fn open_remote(url: &RemoteUrl, config: &UserConfig) -> Result<Remote, WorkspaceError> {
        let token = match url {
            RemoteUrl::Http { base, .. } => config.token(base),
            RemoteUrl::Path(_) => None,
        };
        Ok(Remote::open(url, Self::node_of(config)?, token)?)
    }

    /// The repository's remote, opened the first time it's needed, if it
    /// has one.
    pub(super) fn remote(&self) -> Result<Option<&Remote>, WorkspaceError> {
        if let Some(remote) = self.remote.get() {
            return Ok(remote.as_ref());
        }
        let remote = match self.remote_url()? {
            Some(url) => Some(Self::open_remote(&url, &self.config)?),
            None => None,
        };
        Ok(self.remote.get_or_init(|| remote).as_ref())
    }

    /// Syncs with `remote`, as this node, for its user.
    pub(super) fn sync<'s>(&'s self, remote: &'s Remote) -> Sync<'s> {
        Sync::new(
            &self.store,
            remote,
            self.config.node().clone(),
            self.config.user(),
        )
    }

    /// The repository's remote, which the command needs.
    pub(super) fn require_remote(&self) -> Result<&Remote, WorkspaceError> {
        self.remote()?.ok_or(WorkspaceError::NoRemote)
    }

    /// `origin remote [<url>]`: prints the repository's remote, or sets it.
    ///
    /// # Errors
    ///
    /// Fails if the URL is invalid, or the file naming the remote cannot be
    /// read or written.
    pub fn set_remote(&self, url: Option<&str>) -> Result<(), WorkspaceError> {
        let Some(url) = url else {
            match self.remote_url()? {
                Some(url) => println!("{url}"),
                None => println!("No remote: set one with `origin remote <url>`."),
            }
            return Ok(());
        };
        let url = RemoteUrl::parse(url)?;
        self.write_durably(
            self.store.root().join(Self::REMOTE),
            format!("{url}\n").as_bytes(),
        )?;
        println!("Remote: {url}");
        Ok(())
    }

    /// `origin fetch [--target <target>]... [--change <change>]...`:
    /// fetches the chains of the targets this repository follows and of
    /// `targets`, then the records of the changes it knows and of
    /// `changes`. Changes nothing under the working copy.
    ///
    /// # Errors
    ///
    /// Fails if there's no remote, it lacks a target or change asked for,
    /// or the remote or the store fails.
    pub fn fetch(
        &mut self,
        targets: &[TargetName],
        changes: &[ChangeId],
    ) -> Result<(), WorkspaceError> {
        self.refresh_access()?;
        let remote = self.require_remote()?;
        let sync = self.sync(remote);
        let advanced = sync.fetch_chains(targets)?;
        let mut known: Vec<ChangeId> = Index::open(&self.store)?.changes().collect();
        known.extend(changes);
        known.sort_unstable();
        known.dedup();
        let fetched = sync.fetch_changes(&known)?;
        let index = Index::open(&self.store)?;
        if let Some(&missing) = changes
            .iter()
            .find(|&&change| index.revisions(change).is_empty())
        {
            return Err(WorkspaceError::UnknownChange(missing));
        }
        for Advanced { target, from, to } in &advanced {
            match from {
                Some(from) => println!("`{target}`: #{} -> #{}", from.seq, to.seq),
                None => println!("`{target}`: now followed, at #{}", to.seq),
            }
        }
        if fetched.revisions + fetched.statements > 0 {
            println!(
                "Fetched {} revisions and {} statements",
                fetched.revisions, fetched.statements
            );
        }
        if advanced.is_empty() && fetched.revisions + fetched.statements == 0 {
            println!("Already up to date.");
        }
        Ok(())
    }

    /// `origin pull`: fetches, then moves the working copy onto the tip of
    /// the target its change is on, if that merges cleanly. A change that
    /// landed starts a new one on the tip, as every command does.
    ///
    /// # Errors
    ///
    /// Fails like [`Workspace::fetch`], or if the working copy cannot be
    /// snapshotted or moved.
    pub fn pull(&mut self) -> Result<(), WorkspaceError> {
        self.fetch(&[], &[])?;
        let working_copy = self.refresh()?;
        let Base::Acceptance(base) = working_copy.revision.base else {
            println!("The working copy is on a stack of changes, which stays where it is.");
            return Ok(());
        };
        let target = self.target_name(self.store.get(base)?.body().target)?;
        let tip = self
            .store
            .head(&target)?
            .ok_or_else(|| WorkspaceError::UnknownTarget(target.clone()))?;
        if tip.acceptance == base {
            println!("The working copy is on the tip of `{target}`.");
            return Ok(());
        }
        let files = self.base_tree(working_copy.revision.base)?;
        self.rebase_onto_tip(&working_copy, files, &target)
    }

    /// `origin push [<change>]...`: shares the changes, the working copy's
    /// by default, with the ones below them, with the remote: their
    /// revisions, snapshots and statements. Lands nothing.
    ///
    /// # Errors
    ///
    /// Fails if there's no remote, a change cannot be found, is stale or is
    /// on a target the remote lacks, or the remote or the store fails.
    pub fn push(&mut self, changes: &[String]) -> Result<(), WorkspaceError> {
        let working_copy = self.refresh()?;
        let changes = if changes.is_empty() {
            vec![working_copy.revision.change]
        } else {
            changes
                .iter()
                .map(|change| working_copy.find_change(change))
                .collect::<Result<_, _>>()?
        };
        let mut sync = self.sync(self.require_remote()?);
        if let Some(rights) = Self::access(&self.store)? {
            sync = sync.writing(rights.paths);
        }
        let pushed = sync.push(&changes)?;
        if pushed.objects == 0 {
            println!("Everything up to date.");
        } else {
            println!(
                "Pushed {} revisions and {} statements ({} objects)",
                pushed.revisions, pushed.statements, pushed.objects
            );
        }
        Ok(())
    }

    /// Fetches the snapshots of `records`, revisions and acceptances, that
    /// the repository lacks, if it has a remote: a clone only holds the
    /// snapshots it read. Skips what the working copy's revision and its
    /// base hold. Without a remote, reading a missing snapshot fails later.
    pub(super) fn need(&self, records: &[RecordId]) -> Result<(), WorkspaceError> {
        let mut lacking = Vec::new();
        for &record in records {
            if let Some(snapshot) = self.store.snapshot_of(record)?
                && !self.store.contains(snapshot.into())?
            {
                lacking.push(record);
            }
        }
        if lacking.is_empty() {
            return Ok(());
        }
        let Some(remote) = self.remote()? else {
            return Ok(());
        };
        let current = self.store.get(self.revision)?;
        let base = self.store.base_snapshot(current.base)?;
        let sync = self.sync(remote);
        for record in lacking {
            sync.need(record, &[current.snapshot, base])?;
        }
        Ok(())
    }
}
