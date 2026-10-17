//! What the remote lets the user do: `whoami`, `permissions` and `token
//! issue`, and the paths it withholds.
//!
//! Every clone and fetch records the user's rights on the remote in the
//! repository's `.origin/access`. The paths they may not read are withheld:
//! their content never reaches this repository, so they're never in the
//! working copy, and snapshots keep them as they were (see
//! [`Store::with_withheld`]). Pushes that change a path they may not write
//! fail before sending anything. When the remote lets them read more, a
//! fetch restores the files they can read now.

use std::fs;
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use origin_shared::access::{Capability, PathAccess, Rights};
use origin_shared::api::TokenRequest;
use origin_shared::{Base, Period, RepoPath};

use super::{Workspace, WorkspaceError};
use crate::store::Store;
use crate::tree::{FileChange, Snapshots};
use crate::worktree::Worktree;

impl Workspace {
    /// The file in the repository's `.origin` holding the user's rights on
    /// the remote, as JSON.
    const ACCESS: &str = "access";

    /// The user's rights on the remote as of the last sync, if it has one.
    pub(super) fn access(store: &Store) -> Result<Option<Rights>, WorkspaceError> {
        let path = store.root().join(Self::ACCESS);
        let content = match fs::read(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(WorkspaceError::io(&path, source)),
        };
        serde_json::from_slice(&content)
            .map(Some)
            .map_err(|_| WorkspaceError::Malformed(path))
    }

    /// `store`, withholding what the remote does.
    pub(super) fn withholding(store: Store) -> Result<Store, WorkspaceError> {
        Ok(match Self::access(&store)? {
            Some(rights) => store.with_withheld(rights.paths),
            None => store,
        })
    }

    /// Records the user's rights on the remote in `store`.
    pub(super) fn save_access(store: &Store, rights: &Rights) -> Result<(), WorkspaceError> {
        let json = serde_json::to_vec_pretty(rights).expect("rights serialize to JSON");
        let write = store.durable_write();
        write
            .stage(store.root().join(Self::ACCESS), &json)
            .and_then(|()| write.commit())
            .map_err(crate::store::StoreError::from)?;
        Ok(())
    }

    /// Records the user's rights on the remote, and withholds what they may
    /// not read from now on. Restores the files of the working copy they
    /// read now but didn't before.
    pub(super) fn refresh_access(&mut self) -> Result<(), WorkspaceError> {
        let rights = self.require_remote()?.whoami()?;
        let old = Self::access(&self.store)?;
        if old.as_ref() == Some(&rights) {
            return Ok(());
        }
        Self::save_access(&self.store, &rights)?;
        self.store = self.store.clone().with_withheld(rights.paths.clone());
        match old {
            Some(old) => self.restore(&old.paths, &rights.paths),
            None => Ok(()),
        }
    }

    /// Fetches and writes the files of the working copy under the paths
    /// `now` reads and `before` didn't.
    fn restore(&self, before: &PathAccess, now: &PathAccess) -> Result<(), WorkspaceError> {
        let mut paths: Vec<RepoPath> = before
            .rules()
            .chain(now.rules())
            .map(|(path, _)| path.clone())
            .filter(|path| now.reads(path) && !before.reads(path))
            .collect();
        paths.sort_unstable();
        paths.dedup_by(|inner, outer| inner.starts_with(outer));
        if paths.is_empty() {
            return Ok(());
        }
        let snapshot = self.store.get(self.revision)?.snapshot;
        // The working copy kept what it couldn't read as the acceptance at
        // the bottom of its stack has it, which the remote has too.
        let mut base = self.store.get(self.revision)?.base;
        while let Base::Revision(below) = base {
            base = self.store.get(below)?.base;
        }
        self.sync(self.require_remote()?)
            .need_paths(base.into(), paths.clone())?;
        let mut restored = Vec::new();
        for path in &paths {
            let at = Snapshots::new(&self.store).entry_at(snapshot, path)?;
            let (tree, file) = match at {
                Some(entry) => (entry.subtree(), entry.file()),
                None => continue,
            };
            if let Some(file) = file {
                restored.push(FileChange {
                    path: path.clone(),
                    old: None,
                    new: Some(file),
                });
            }
            restored.extend(self.store.diff_at(path, None, tree)?);
        }
        // A restored file never replaces one the user wrote.
        restored
            .retain(|change| fs::symlink_metadata(self.root.join(change.path.as_str())).is_err());
        Ok(Worktree::new(&self.root, &self.store).checkout(&restored, &[])?)
    }

    /// `origin whoami`: prints who the remote thinks the user is.
    ///
    /// # Errors
    ///
    /// Fails if the remote cannot be reached.
    pub fn whoami(&self) -> Result<(), WorkspaceError> {
        let Some(remote) = self.remote()? else {
            println!("{}, the owner of this repository", self.config.user());
            return Ok(());
        };
        let rights = remote.whoami()?;
        match &rights.principal {
            None if rights.paths.writes_all() => {
                println!("The owner: the remote is a local repository.");
            }
            None => println!("Anonymous: set a token for the remote to sign in."),
            Some(principal) if rights.groups.is_empty() => println!("{principal}"),
            Some(principal) => {
                let groups: Vec<_> = rights.groups.iter().map(ToString::to_string).collect();
                println!("{principal} (groups: {})", groups.join(", "));
            }
        }
        if let Some(scope) = &rights.scope {
            let scope: Vec<_> = scope.iter().map(ToString::to_string).collect();
            println!("Token scope: {}", scope.join(", "));
        }
        Ok(())
    }

    /// `origin permissions`: prints what the user may do on the remote.
    ///
    /// # Errors
    ///
    /// Fails if the remote cannot be reached.
    pub fn permissions(&self) -> Result<(), WorkspaceError> {
        let rights = match self.remote()? {
            Some(remote) => remote.whoami()?,
            None => Rights::all(),
        };
        for (path, level) in rights.paths.rules() {
            println!("{level:<5} {path}");
        }
        let list = |items: Vec<String>| {
            if items.is_empty() {
                "nothing".to_owned()
            } else {
                items.join(", ")
            }
        };
        println!(
            "annotate: {}",
            list(rights.annotate.iter().map(ToString::to_string).collect())
        );
        println!(
            "land: {}",
            list(rights.land.iter().map(ToString::to_string).collect())
        );
        println!("admin: {}", if rights.admin { "yes" } else { "no" });
        Ok(())
    }

    /// `origin token issue`: asks the remote for a token acting for the
    /// user, with `scope`, valid for `ttl`, and prints it.
    ///
    /// # Errors
    ///
    /// Fails if there's no remote, it cannot be reached, or it refuses:
    /// the scope must be part of the user's rights.
    pub fn issue_token(&self, scope: Vec<Capability>, ttl: Period) -> Result<(), WorkspaceError> {
        let issued = self
            .require_remote()?
            .issue_token(&TokenRequest { scope, ttl })?;
        let left = Duration::from_secs(issued.expires).saturating_sub(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default(),
        );
        println!("{}", issued.token);
        eprintln!(
            "Acts for {} for {} minutes: send it as ORIGIN_TOKEN, or add it to [tokens] in \
             config.toml",
            issued.principal,
            left.as_secs().div_ceil(60)
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use origin_shared::access::Level;

    use super::*;

    #[test]
    fn rights_round_trip_through_the_access_file() {
        let rights = Rights {
            paths: PathAccess::new(
                Level::Read,
                [(RepoPath::new("secrets").unwrap(), Level::None)],
            ),
            ..Rights::all()
        };
        let json = serde_json::to_vec(&rights).unwrap();
        assert_eq!(serde_json::from_slice::<Rights>(&json).unwrap(), rights);
    }
}
