//! Sync: sharing records with a [remote](crate::remote).
//!
//! - **Fetching** brings the chain of each target this node follows, those
//!   it holds and those asked for, from the last acceptance it holds on.
//!   Each acceptance is verified against the key of the authority its
//!   policy names, and comes with what it landed. Then the records of each
//!   change are compared with the remote's, by id, and those missing
//!   fetched, with the records they refer to. Snapshots don't come with
//!   them: [`Sync::need`] fetches one when something reads it, such as a
//!   checkout, so a clone fetches one snapshot, its target's tip. The
//!   remote withholds the content of the paths the user may not read: the
//!   trees keep their place, and nothing reads them (see
//!   [`Store::with_withheld`]).
//! - **Pushing** shares revisions, with their snapshots, and the user's
//!   statements. It never lands anything. It first compares each change's
//!   heads with the remote's, and refuses before uploading anything when
//!   the remote has a revision this node lacks: the change is stale, or a
//!   revision changes a path the user may not write. Revisions only go to
//!   a remote that has the acceptance they're based on.
//!
//! Fetched records are written before the chains that refer to them, and
//! the op log's `writers` lock is held from the first write until the last
//! chain is appended, so readers of events wait for the chains rather than
//! skip the acceptances of targets this node didn't follow yet.

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::mem;

use origin_platform::Native;
use origin_shared::access::{Level, PathAccess};
use origin_shared::api::{ErrorCode, FetchRecords, FetchSnapshot, InventoryRequest, LogQuery};
use origin_shared::object::{BadSignature, DecodeError};
use origin_shared::pack::Pack;
use origin_shared::{
    Acceptance, AcceptanceId, Base, ChangeId, EncodedObject, Head, Object, ObjectId, ObjectKind,
    Principal, PrincipalName, Record, RecordId, RepoPath, Revision, RevisionId, Source, TargetName,
    TreeId,
};

use crate::annotations::Annotations;
use crate::index::Index;
use crate::permissions::Forbidden;
use crate::policy::{Policy, PolicyError};
use crate::remote::{Remote, RemoteError};
use crate::service::Dependencies;
use crate::store::{Batch, Store, StoreError};

/// A store, and the remote it syncs with.
pub struct Sync<'a> {
    store: &'a Store,
    remote: &'a Remote,
    /// This node, which flags the divergence fetched revisions reveal.
    node: Principal,
    /// The user, whose statements pushes share.
    user: Principal,
    /// What the user may write on the remote, as of the last fetch.
    writes: PathAccess,
}

/// A target whose chain a fetch extended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advanced {
    /// The target.
    pub target: TargetName,
    /// Its head before, if this node followed it.
    pub from: Option<Head>,
    /// Its head now.
    pub to: Head,
}

/// What fetching records did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fetched {
    /// The revisions fetched.
    pub revisions: usize,
    /// The statements fetched.
    pub statements: usize,
}

/// What a push shared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pushed {
    /// The revisions the remote didn't have.
    pub revisions: usize,
    /// The statements the remote didn't have.
    pub statements: usize,
    /// Every object sent: those, and the blobs and trees of their
    /// snapshots the remote lacked.
    pub objects: usize,
}

/// The new acceptances of a target, verified and about to be appended.
struct Chain {
    target: TargetName,
    /// This node's head of the target, if it has one.
    from: Option<Head>,
    /// The remote's head: the last of `acceptances`.
    to: Head,
    acceptances: Vec<(AcceptanceId, Acceptance)>,
}

impl<'a> Sync<'a> {
    /// Syncs `store` with `remote`, as the node `node`, for `user`, who
    /// writes everything until [`Sync::writing`] says otherwise.
    pub fn new(store: &'a Store, remote: &'a Remote, node: PrincipalName, user: Principal) -> Self {
        Self {
            store,
            remote,
            node: Principal::Node(node),
            user,
            writes: PathAccess::uniform(Level::Write),
        }
    }

    /// Syncs knowing the user may only write `paths`, so pushes that would
    /// be refused fail before sending anything.
    #[must_use]
    pub fn writing(self, paths: PathAccess) -> Self {
        Self {
            writes: paths,
            ..self
        }
    }

    /// Fetches the chains of `policy`, of the targets this node holds, and
    /// of `more`, with what they landed, as the module explains. Returns
    /// the targets that moved, `policy` first. Learns the names of all the
    /// remote's targets, so acceptances of those this node doesn't follow
    /// can be described.
    ///
    /// # Errors
    ///
    /// Fails if the remote doesn't have a target of `more`, a target's
    /// chain differs from the remote's, an acceptance doesn't verify, or
    /// the remote or the store fails.
    pub fn fetch_chains(&self, more: &[TargetName]) -> Result<Vec<Advanced>, SyncError> {
        let remote: HashMap<TargetName, Head> = self
            .remote
            .targets()?
            .into_iter()
            .map(|target| (target.name, target.head))
            .collect();
        for name in remote.keys() {
            self.store.name_target(name)?;
        }
        let mut follow: BTreeSet<TargetName> = self.store.targets()?.into_iter().collect();
        for target in more {
            if !remote.contains_key(target) {
                return Err(SyncError::UnknownTarget(target.clone()));
            }
            follow.insert(target.clone());
        }
        follow.remove(&TargetName::POLICY);
        let mut chains = Vec::new();
        for target in [TargetName::POLICY].into_iter().chain(follow) {
            // Targets only this node has stay as they are.
            if let Some(&head) = remote.get(&target) {
                chains.extend(self.chain(target, head)?);
            }
        }
        let fetched: HashMap<AcceptanceId, &Acceptance> = chains
            .iter()
            .flat_map(|chain| {
                chain
                    .acceptances
                    .iter()
                    .map(|(id, acceptance)| (*id, acceptance))
            })
            .collect();
        self.verify(&chains, &fetched)?;
        let appending: HashSet<ObjectId> = fetched.keys().map(|&id| id.into()).collect();
        let landed = chains
            .iter()
            .flat_map(|chain| &chain.acceptances)
            .flat_map(|(_, acceptance)| &acceptance.body().landed)
            .map(|&source| match source {
                Source::Revision(id) => id.into(),
                Source::Target(id) => id.into(),
            });
        let held = self.store.batch();
        let records = self.closure(&held, landed, &appending)?;
        held.commit()?;

        let _writing = self.store.writing()?;
        if !records.is_empty() {
            self.store.write_all(&records)?;
        }
        let mut advanced = Vec::new();
        for chain in chains {
            let acceptances: Vec<_> = chain.acceptances.into_iter().map(|(_, a)| a).collect();
            self.store.append_all(&chain.target, &acceptances, &[])?;
            advanced.push(Advanced {
                target: chain.target,
                from: chain.from,
                to: chain.to,
            });
        }
        Ok(advanced)
    }

    /// The acceptances of `target` after the last this node holds, up to
    /// the remote's head, `theirs`, checked against their ids and target.
    /// `None` if there are none.
    fn chain(&self, target: TargetName, theirs: Head) -> Result<Option<Chain>, SyncError> {
        let ours = self.store.head(&target)?;
        if ours == Some(theirs) {
            return Ok(None);
        }
        let query = LogQuery {
            target: target.clone(),
            since: ours.map_or(0, |ours| ours.seq),
            limit: None,
        };
        let mut entries = self.remote.log(&query)?.into_iter();
        // The remote's log goes through this node's head.
        if let Some(ours) = ours
            && entries
                .next()
                .is_none_or(|entry| entry.id != ours.acceptance)
        {
            return Err(SyncError::Diverged(target));
        }
        let mut acceptances = Vec::new();
        for entry in entries {
            let body = entry.acceptance.body();
            if entry.acceptance.encode().id() != entry.id.into() || body.target != target.id() {
                return Err(SyncError::Unverified {
                    target,
                    seq: body.seq,
                    why: Unverified::Id(entry.id),
                });
            }
            acceptances.push((entry.id, entry.acceptance));
        }
        let Some((id, last)) = acceptances.last() else {
            return Ok(None);
        };
        let to = Head {
            seq: last.body().seq,
            acceptance: *id,
        };
        Ok(Some(Chain {
            target,
            from: ours,
            to,
            acceptances,
        }))
    }

    /// Checks that the authority each acceptance's policy names signed it.
    /// The first acceptance of `policy` names no policy: its own snapshot
    /// holds the one it's checked against. The policies' snapshots are
    /// fetched as needed, and those of `policy`'s new acceptances always,
    /// so the policy in force can be read.
    fn verify(
        &self,
        chains: &[Chain],
        fetched: &HashMap<AcceptanceId, &Acceptance>,
    ) -> Result<(), SyncError> {
        let mut policies: HashMap<AcceptanceId, Policy> = HashMap::new();
        for chain in chains {
            for (id, acceptance) in &chain.acceptances {
                let body = acceptance.body();
                // Small, and the policy in force once it's the tip.
                if chain.target == TargetName::POLICY {
                    self.fetch_snapshot(RecordId::Acceptance(*id), body.state, &[])?;
                }
                let unverified = |why| SyncError::Unverified {
                    target: chain.target.clone(),
                    seq: body.seq,
                    why,
                };
                let policy_id = match body.policy {
                    Some(policy) => policy,
                    None if chain.target == TargetName::POLICY && body.seq == 0 => *id,
                    None => return Err(unverified(Unverified::NoPolicy)),
                };
                let policy = match policies.entry(policy_id) {
                    Entry::Occupied(policy) => policy.into_mut(),
                    Entry::Vacant(vacant) => {
                        let state = match fetched.get(&policy_id) {
                            Some(policy) => policy.body().state,
                            None => self.store.get(policy_id)?.body().state,
                        };
                        self.fetch_snapshot(RecordId::Acceptance(policy_id), state, &[])?;
                        let policy = Policy::of_snapshot(self.store, state)?
                            .ok_or(unverified(Unverified::NoPolicy))?;
                        vacant.insert(policy)
                    }
                };
                let authority = policy
                    .authority(&chain.target)
                    .ok_or(unverified(Unverified::NoAuthority))?;
                let key = policy
                    .node_key(authority)
                    .ok_or_else(|| unverified(Unverified::NoKey(authority.clone())))?;
                acceptance
                    .verify(key)
                    .map_err(|error| unverified(Unverified::Signature(error)))?;
            }
        }
        Ok(())
    }

    /// Fetches the records of `changes` the remote has and this node
    /// doesn't: their revisions and the statements about them, with the
    /// records they refer to. Flags the changes that diverged.
    ///
    /// # Errors
    ///
    /// Fails if the remote or the store fails.
    pub fn fetch_changes(&self, changes: &[ChangeId]) -> Result<Fetched, SyncError> {
        if changes.is_empty() {
            return Ok(Fetched::default());
        }
        let inventories = self.remote.inventory(&InventoryRequest {
            changes: changes.to_vec(),
        })?;
        let ids = inventories.iter().flat_map(|inventory| {
            let revisions = inventory.revisions.iter().map(|&id| ObjectId::from(id));
            revisions.chain(inventory.statements.iter().map(|&id| id.into()))
        });
        let held = self.store.batch();
        let records = self.closure(&held, ids, &HashSet::new())?;
        held.commit()?;
        let mut fetched = Fetched::default();
        let mut revised = BTreeSet::new();
        for record in &records {
            match record.kind() {
                Ok(ObjectKind::Revision) => {
                    fetched.revisions += 1;
                    let revision: Revision =
                        record.decode_as().map_err(|source| SyncError::Malformed {
                            id: record.id(),
                            source,
                        })?;
                    revised.insert(revision.change);
                }
                Ok(ObjectKind::Statement) => fetched.statements += 1,
                _ => {}
            }
        }
        if !records.is_empty() {
            self.store.write_all(&records)?;
        }
        let annotations = Annotations::open(self.store)?;
        for change in revised {
            annotations.flag_divergence(change, self.node.clone())?;
        }
        Ok(fetched)
    }

    /// Fetches the records `ids` names that this node lacks, other than
    /// those of `skip`, with the records they refer to, recursively: a
    /// revision's base and dependencies, a statement's subject and the
    /// statements it supersedes or promotes, but not their snapshots. Each
    /// comes with the blob it carries. Those this node has are made durable
    /// by `held`, which must commit before anything refers to them.
    fn closure(
        &self,
        held: &Batch<'_, Native>,
        ids: impl IntoIterator<Item = ObjectId>,
        skip: &HashSet<ObjectId>,
    ) -> Result<Vec<EncodedObject>, SyncError> {
        let mut seen = HashSet::new();
        let mut wanted = Vec::new();
        let mut want = |id: ObjectId, wanted: &mut Vec<ObjectId>| {
            if !skip.contains(&id) && !held.depend(id)? && seen.insert(id) {
                wanted.push(id);
            }
            Ok::<_, StoreError>(())
        };
        for id in ids {
            want(id, &mut wanted)?;
        }
        let mut objects = Vec::new();
        while !wanted.is_empty() {
            let request = FetchRecords {
                ids: mem::take(&mut wanted),
            };
            for object in self.remote.records(&request)?.objects {
                if matches!(
                    object.kind(),
                    Ok(ObjectKind::Revision | ObjectKind::Statement)
                ) {
                    let decoded = object.decode().map_err(|source| SyncError::Malformed {
                        id: object.id(),
                        source,
                    })?;
                    for id in Dependencies::of(&decoded).records {
                        want(id, &mut wanted)?;
                    }
                }
                objects.push(object);
            }
        }
        Ok(objects)
    }

    /// Fetches the trees and blobs of the snapshot of `record`, a revision
    /// or an acceptance this node has, unless the store has its root,
    /// skipping what the snapshots `have`, which it holds whole, hold at
    /// the same path. Returns whether it fetched.
    ///
    /// # Errors
    ///
    /// Fails if the record has no snapshot, the remote doesn't have it, or
    /// the remote or the store fails.
    pub fn need(&self, record: RecordId, have: &[TreeId]) -> Result<bool, SyncError> {
        let snapshot = self
            .store
            .snapshot_of(record)?
            .ok_or(SyncError::NoSnapshot(record))?;
        self.fetch_snapshot(record, snapshot, have)
    }

    /// [`Sync::need`], given the snapshot of `record`.
    fn fetch_snapshot(
        &self,
        record: RecordId,
        snapshot: TreeId,
        have: &[TreeId],
    ) -> Result<bool, SyncError> {
        if self.store.contains(snapshot.into())? {
            return Ok(false);
        }
        let have = self.store.present(have.iter().copied())?;
        self.fetch(&FetchSnapshot {
            of: record,
            have,
            paths: Vec::new(),
        })?;
        Ok(true)
    }

    /// Fetches what the snapshot of `record` holds under `paths`, whatever
    /// the store has: files a checkout lacks, such as those whose content
    /// was withheld until the user could read them.
    ///
    /// # Errors
    ///
    /// Fails if the remote doesn't have the record, or the remote or the
    /// store fails.
    pub fn need_paths(&self, record: RecordId, paths: Vec<RepoPath>) -> Result<(), SyncError> {
        self.fetch(&FetchSnapshot {
            of: record,
            have: Vec::new(),
            paths,
        })
    }

    fn fetch(&self, request: &FetchSnapshot) -> Result<(), SyncError> {
        let pack = self.remote.snapshot(request)?;
        self.store.write_all(&pack.objects)?;
        Ok(())
    }

    /// Shares `changes`, and the changes below them, with the remote, as
    /// the module explains.
    ///
    /// # Errors
    ///
    /// Fails if a change has no revision here, is stale, or is based on an
    /// acceptance the remote doesn't have, or if the remote or the store
    /// fails. Nothing is sent then.
    pub fn push(&self, changes: &[ChangeId]) -> Result<Pushed, SyncError> {
        let index = Index::open(self.store)?;
        let changes = self.stack(&index, changes)?;
        let inventories = self.remote.inventory(&InventoryRequest {
            changes: changes.clone(),
        })?;
        let mut shared: HashSet<ObjectId> = HashSet::new();
        for inventory in &inventories {
            let local = index.revisions(inventory.change);
            if let Some(&head) = inventory.heads.iter().find(|head| !local.contains(head)) {
                return Err(SyncError::Stale {
                    change: inventory.change,
                    head,
                });
            }
            shared.extend(inventory.revisions.iter().map(|&id| ObjectId::from(id)));
            shared.extend(inventory.statements.iter().map(|&id| ObjectId::from(id)));
        }

        let mut revisions = Vec::new();
        for &change in &changes {
            for &id in index.revisions(change) {
                if !shared.contains(&id.into()) {
                    revisions.push((id, self.store.get(id)?));
                }
            }
        }
        let mut checked = HashSet::new();
        for (id, revision) in &revisions {
            if let Base::Acceptance(base) = revision.base
                && checked.insert(base)
            {
                self.check_shared(*id, revision, base)?;
            }
            self.check_writes(revision)?;
        }

        let mut ids = Vec::new();
        let mut seen = HashSet::new();
        let mut send = |id: ObjectId, ids: &mut Vec<ObjectId>| {
            if seen.insert(id) {
                ids.push(id);
            }
        };
        for (id, revision) in &revisions {
            shared.insert((*id).into());
            // The remote has these snapshots, or gets them first.
            let mut have = vec![self.store.base_snapshot(revision.base)?];
            for &predecessor in &revision.predecessors {
                if shared.contains(&predecessor.into()) {
                    have.push(self.store.get(predecessor)?.snapshot);
                }
            }
            let have = self.store.present(have)?;
            for placed in self.store.added(revision.snapshot, &have, &[], None)? {
                send(placed.entry.id, &mut ids);
            }
            send(revision.description.into(), &mut ids);
            send((*id).into(), &mut ids);
        }
        let mut statements = 0;
        let mut subjects: Vec<RecordId> = changes
            .iter()
            .flat_map(|&change| index.revisions(change))
            .map(|&id| RecordId::Revision(id))
            .collect();
        while let Some(subject) = subjects.pop() {
            for &id in index.statements(subject) {
                subjects.push(RecordId::Statement(id));
                if shared.contains(&id.into()) {
                    continue;
                }
                let statement = self.store.get(id)?;
                // Others' statements are theirs to share, and this node's
                // own, like divergence, the remote finds itself.
                if statement.issuer != self.user {
                    continue;
                }
                let payload = statement.payload;
                // A statement promoted from one the remote lacks stays:
                // the remote's queue promotes results itself.
                let dependencies = Dependencies::of(&Object::Statement(statement));
                if !dependencies.records.iter().all(|id| shared.contains(id)) {
                    continue;
                }
                if let Some(payload) = payload
                    && self.store.contains(payload.into())?
                {
                    send(payload.into(), &mut ids);
                }
                send(id.into(), &mut ids);
                shared.insert(id.into());
                statements += 1;
            }
        }
        if ids.is_empty() {
            return Ok(Pushed::default());
        }
        let objects = self.store.read_all(&ids)?;
        self.remote.put_records(Pack { objects })?;
        Ok(Pushed {
            revisions: revisions.len(),
            statements,
            objects: ids.len(),
        })
    }

    /// Checks that the user may write every path `revision` changes.
    fn check_writes(&self, revision: &Revision) -> Result<(), SyncError> {
        if self.writes.writes_all() {
            return Ok(());
        }
        let base = self.store.base_snapshot(revision.base)?;
        for change in self.store.diff(base, revision.snapshot)? {
            if !self.writes.writes(&change.path) {
                return Err(Forbidden::Write(change.path).into());
            }
        }
        Ok(())
    }

    /// `changes`, and every change below their revisions, following their
    /// bases and dependencies.
    fn stack(&self, index: &Index, changes: &[ChangeId]) -> Result<Vec<ChangeId>, SyncError> {
        let mut stack = Vec::new();
        let mut seen = HashSet::new();
        let mut next = changes.to_vec();
        while let Some(change) = next.pop() {
            if !seen.insert(change) {
                continue;
            }
            let revisions = index.revisions(change);
            if revisions.is_empty() {
                return Err(SyncError::UnknownChange(change));
            }
            stack.push(change);
            for &id in revisions {
                let revision = self.store.get(id)?;
                let base = match revision.base {
                    Base::Revision(base) => Some(base),
                    Base::Acceptance(_) => None,
                };
                for below in base.into_iter().chain(revision.deps) {
                    if self.store.contains(below.into())? {
                        next.push(self.store.get(below)?.change);
                    }
                }
            }
        }
        Ok(stack)
    }

    /// Checks that the remote has `base`, the acceptance the revision `id`
    /// is based on.
    fn check_shared(
        &self,
        id: RevisionId,
        revision: &Revision,
        base: AcceptanceId,
    ) -> Result<(), SyncError> {
        let body = self.store.get(base)?.body().clone();
        let unshared = |target| SyncError::Unshared {
            change: revision.change,
            revision: id,
            target,
            seq: body.seq,
        };
        let Some(target) = self.store.target_name(body.target)? else {
            return Err(unshared(None));
        };
        let query = LogQuery {
            target: target.clone(),
            since: body.seq,
            limit: Some(1),
        };
        match self.remote.log(&query) {
            Ok(entries) if entries.first().is_some_and(|entry| entry.id == base) => Ok(()),
            Ok(_) => Err(unshared(Some(target))),
            Err(error) if error.code() == Some(ErrorCode::NotFound) => Err(unshared(Some(target))),
            Err(error) => Err(error.into()),
        }
    }
}

/// Why an acceptance fetched doesn't verify.
#[derive(Debug, thiserror::Error)]
pub enum Unverified {
    /// It doesn't match its id, or belongs to another target.
    #[error("it isn't acceptance {0} of the target")]
    Id(AcceptanceId),
    /// It names no policy, or its policy has no `policy.toml`.
    #[error("it has no policy")]
    NoPolicy,
    /// Its policy names no authority for the target.
    #[error("its policy names no authority for the target")]
    NoAuthority,
    /// Its policy lists no key for the authority.
    #[error("its policy lists no key for node:{0}")]
    NoKey(PrincipalName),
    /// The authority didn't sign it.
    #[error("its authority didn't sign it")]
    Signature(#[source] BadSignature),
}

/// A failure to sync with a remote.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// The remote has no such target.
    #[error("the remote has no target `{0}`")]
    UnknownTarget(TargetName),
    /// A snapshot was needed of a record that has none.
    #[error("{0} has no snapshot")]
    NoSnapshot(RecordId),
    /// A push would change what the user may not.
    #[error("the remote would refuse the push")]
    Forbidden(#[from] Forbidden),
    /// The change has no revision to push.
    #[error("change {} has no revisions", .0.short())]
    UnknownChange(ChangeId),
    /// A target's chain here isn't a start of the remote's.
    #[error("the history of `{0}` here differs from the remote's")]
    Diverged(TargetName),
    /// An acceptance fetched doesn't verify.
    #[error("acceptance #{seq} of `{target}` from the remote doesn't verify: {why}")]
    Unverified {
        /// Its target.
        target: TargetName,
        /// Its position.
        seq: u64,
        /// Why.
        #[source]
        why: Unverified,
    },
    /// The remote has a revision of a change that this node lacks: the
    /// change moved on there.
    #[error(
        "change {} is stale: the remote has revision {} of it, which this repository doesn't. \
         Run `origin fetch`, then `origin converge {}`",
        change.short(),
        head.short(),
        change.short()
    )]
    Stale {
        /// The change.
        change: ChangeId,
        /// The remote's head this node lacks.
        head: RevisionId,
    },
    /// A revision to push is based on an acceptance the remote doesn't
    /// have.
    #[error(
        "revision {} of change {} is based on {} #{seq}, which the remote doesn't have: \
         start the change on a target the remote has, with `origin new --on`",
        revision.short(),
        change.short(),
        target.as_ref().map_or_else(|| "a target".to_owned(), |target| format!("`{target}`"))
    )]
    Unshared {
        /// The change.
        change: ChangeId,
        /// The revision.
        revision: RevisionId,
        /// The base's target, if this node knows its name.
        target: Option<TargetName>,
        /// The base's position.
        seq: u64,
    },
    /// The remote sent an object that doesn't decode.
    #[error("the remote sent a malformed object {id}")]
    Malformed {
        /// The object.
        id: ObjectId,
        /// What is wrong.
        source: DecodeError,
    },
    /// The remote failed.
    #[error(transparent)]
    Remote(#[from] RemoteError),
    /// A policy fetched cannot be read.
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// Objects cannot be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use origin_platform::Native;
    use origin_shared::event::{EventKind, Filter};
    use origin_shared::{AcceptanceBody, NodeKey};

    use super::*;
    use crate::events::Events;
    use crate::queue::tests::{Repo, ci_test};
    use crate::service::{Node, OriginService};
    use crate::tree::tests::build;

    const MAIN_RULES: &str = "[targets.main]\nauthority = \"node:queue\"\n";

    /// A node's own repository, empty, syncing with the queue's repository
    /// of `server` in-process.
    struct Client {
        _dir: tempfile::TempDir,
        store: Store,
        remote: Remote,
    }

    impl Client {
        fn new(server: &Repo) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::new(dir.path().join(".origin"), Native::default());
            store.init().unwrap();
            let node = Node {
                name: "queue".parse().unwrap(),
                key: Arc::new(NodeKey::from_bytes(&[7; 32])),
            };
            let remote = Remote::Local(Box::new(OriginService::new(server.store.clone(), node)));
            Self {
                _dir: dir,
                store,
                remote,
            }
        }

        fn sync(&self) -> Sync<'_> {
            Sync::new(
                &self.store,
                &self.remote,
                "laptop".parse().unwrap(),
                "user:ada".parse().unwrap(),
            )
        }

        /// Writes a revision of `change` with `files` on `base`, with every
        /// object it needs.
        fn revision(
            &self,
            change: u8,
            predecessors: &[RevisionId],
            base: Base,
            files: &[(&str, &str)],
        ) -> RevisionId {
            let mut objects: Vec<_> = files
                .iter()
                .map(|(_, content)| EncodedObject::blob(content.as_bytes()))
                .collect();
            let description = EncodedObject::attachment(b"work");
            let revision = Revision {
                change: ChangeId::from_bytes([change; 16]),
                predecessors: predecessors.to_vec(),
                base,
                deps: Vec::new(),
                snapshot: build(&self.store, files),
                description: description.id().into(),
                author: "user:ada".parse().unwrap(),
            }
            .encode();
            objects.extend([description, revision.clone()]);
            self.store.write_all(&objects).unwrap();
            revision.id().into()
        }
    }

    fn main_tip(store: &Store) -> Base {
        Base::Acceptance(store.head(&TargetName::MAIN).unwrap().unwrap().acceptance)
    }

    #[test]
    fn fetches_verified_chains_and_what_they_landed_but_not_snapshots() {
        let server = Repo::new(MAIN_RULES);
        let landed = server.change_on_main(1, &[("a", "a")]);
        server.land(1);
        let client = Client::new(&server);

        let advanced = client.sync().fetch_chains(&[]).unwrap();
        assert_eq!(advanced.len(), 1, "`policy` is always followed");
        let advanced = client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
        assert_eq!(advanced[0].to, server.tip(&TargetName::MAIN));
        assert_eq!(
            client.store.log(&TargetName::MAIN).unwrap(),
            server.store.log(&TargetName::MAIN).unwrap()
        );
        assert!(
            client.store.contains(landed.into()).unwrap(),
            "what landed comes"
        );
        assert!(Policy::in_force(&client.store).is_ok(), "the policy too");

        let state = client
            .store
            .get(server.tip(&TargetName::MAIN).acceptance)
            .unwrap()
            .body()
            .state;
        assert!(
            !client.store.contains(state.into()).unwrap(),
            "snapshots are lazy"
        );
        let tip = RecordId::Acceptance(server.tip(&TargetName::MAIN).acceptance);
        assert!(client.sync().need(tip, &[]).unwrap());
        assert!(!client.sync().need(tip, &[]).unwrap(), "once");
        assert_eq!(client.store.diff(state, state).unwrap(), []);
        assert_eq!(client.sync().fetch_chains(&[]).unwrap(), []);
    }

    #[test]
    fn acceptances_that_do_not_verify_are_refused() {
        let server = Repo::new(MAIN_RULES);
        let tip = server.tip(&TargetName::MAIN);
        let state = server.store.get(tip.acceptance).unwrap().body().state;
        let forged = AcceptanceBody {
            target: TargetName::MAIN.id(),
            seq: 1,
            prev: Some(tip.acceptance),
            state,
            landed: Vec::new(),
            evidence: Vec::new(),
            policy: Some(server.tip(&TargetName::POLICY).acceptance),
        }
        .sign(&NodeKey::from_bytes(&[8; 32]));
        server.store.append(&TargetName::MAIN, &forged).unwrap();
        let client = Client::new(&server);

        let error = client.sync().fetch_chains(&[TargetName::MAIN]).unwrap_err();
        assert!(
            matches!(
                &error,
                SyncError::Unverified {
                    seq: 1,
                    why: Unverified::Signature(_),
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(
            client.store.head(&TargetName::MAIN).unwrap(),
            None,
            "nothing of the chain was appended"
        );
    }

    #[test]
    fn acceptances_fetched_before_their_chain_advance_it_once_appended() {
        let server = Repo::new(MAIN_RULES);
        server.change_on_main(1, &[("a", "a")]);
        server.land(1);
        let release = TargetName::new("release").unwrap();
        server
            .queue()
            .create_target(&release, &TargetName::MAIN)
            .unwrap();
        let main_tip = server.tip(&TargetName::MAIN).acceptance;
        let client = Client::new(&server);
        let events = Events::new(&client.store);

        // `release` landed `main`'s tip, which comes as a record.
        client.sync().fetch_chains(&[release]).unwrap();
        assert!(client.store.contains(main_tip.into()).unwrap());
        let advanced = |cursor| {
            let page = events
                .after(cursor, &Filter::default(), usize::MAX)
                .unwrap();
            let main: Vec<u64> = page
                .events
                .iter()
                .filter_map(|event| match &event.kind {
                    EventKind::TargetAdvanced { target, seq, .. }
                        if *target == TargetName::MAIN =>
                    {
                        Some(*seq)
                    }
                    _ => None,
                })
                .collect();
            (main, page.next)
        };
        let (main, cursor) = advanced(0);
        assert!(main.is_empty(), "not followed, so nothing advanced");

        client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
        assert_eq!(
            advanced(cursor).0,
            [0, 1],
            "the acceptance held before advances `main` too, once appended"
        );
    }

    #[test]
    fn fetches_the_records_of_changes() {
        let server = Repo::new(MAIN_RULES);
        let first = server.change_on_main(1, &[("a", "a")]);
        let annotations = server.annotations();
        annotations.issue(&ci_test(first, "node:ci"), None).unwrap();
        let client = Client::new(&server);
        client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();

        let fetched = client
            .sync()
            .fetch_changes(&[ChangeId::from_bytes([1; 16])])
            .unwrap();
        assert_eq!(
            fetched,
            Fetched {
                revisions: 1,
                statements: 1
            }
        );
        assert!(client.store.contains(first.into()).unwrap());
        let again = client
            .sync()
            .fetch_changes(&[ChangeId::from_bytes([1; 16])])
            .unwrap();
        assert_eq!(again, Fetched::default());
    }

    #[test]
    fn pushes_revisions_snapshots_and_statements() {
        let server = Repo::new(MAIN_RULES);
        let client = Client::new(&server);
        client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
        let base = main_tip(&client.store);
        let first = client.revision(1, &[], base, &[("a", "1"), ("b", "1")]);
        let second = client.revision(1, &[first], base, &[("a", "2"), ("b", "1")]);
        let statement = ci_test(second, "user:ada").encode();
        client
            .store
            .write_all(std::slice::from_ref(&statement))
            .unwrap();

        let change = ChangeId::from_bytes([1; 16]);
        let pushed = client.sync().push(&[change]).unwrap();
        assert_eq!((pushed.revisions, pushed.statements), (2, 1), "{pushed:?}");
        assert_eq!(
            pushed.objects,
            2 + 1 + 1 + 2 + 2,
            "the revisions, their description, the statement, two trees and two blobs"
        );
        for id in [first.into(), second.into(), statement.id()] {
            assert!(server.store.contains(id).unwrap());
        }
        assert_eq!(client.sync().push(&[change]).unwrap(), Pushed::default());
    }

    #[test]
    fn pushes_refuse_stale_changes_and_bases_the_remote_lacks() {
        let server = Repo::new(MAIN_RULES);
        let shared = server.change_on_main(1, &[("a", "a")]);
        let client = Client::new(&server);
        client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
        let change = ChangeId::from_bytes([1; 16]);
        client.sync().fetch_changes(&[change]).unwrap();

        let base = main_tip(&client.store);
        let ours = client.revision(1, &[shared], base, &[("a", "ours")]);
        let theirs = server.revision(1, &[shared], base, &[("a", "theirs")]);
        let error = client.sync().push(&[change]).unwrap_err();
        assert!(
            matches!(error, SyncError::Stale { change: c, head } if c == change && head == theirs),
            "{error}"
        );
        assert!(
            !server.store.contains(ours.into()).unwrap(),
            "nothing was sent"
        );

        let local = TargetName::new("local").unwrap();
        let key = NodeKey::from_bytes(&[9; 32]);
        let genesis = AcceptanceBody {
            target: local.id(),
            seq: 0,
            prev: None,
            state: build(&client.store, &[]),
            landed: Vec::new(),
            evidence: Vec::new(),
            policy: None,
        }
        .sign(&key);
        client.store.append(&local, &genesis).unwrap();
        let on_local = Base::Acceptance(genesis.encode().id().into());
        client.revision(2, &[], on_local, &[("b", "b")]);
        let error = client
            .sync()
            .push(&[ChangeId::from_bytes([2; 16])])
            .unwrap_err();
        assert!(
            matches!(&error, SyncError::Unshared { target: Some(target), .. } if *target == local),
            "{error}"
        );
    }
}
