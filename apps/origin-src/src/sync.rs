//! Sync: sharing records with a [remote](crate::remote).
//!
//! - **Fetching** brings the chain of each target this node follows, those
//!   it holds and those asked for, from the last acceptance it holds on.
//!   Each acceptance must follow the one before it, and is verified against
//!   the key of the authority named by the policy governing it
//!   ([`Policy::governing`]): for an acceptance of `policy`, the one before
//!   it, and for any other, the policy it names, which must be on the chain
//!   of `policy` as this node verified it. Only the first acceptance of
//!   `policy` is checked against its own. Acceptances this node merely
//!   stores, such as the base of a revision fetched, never govern anything.
//!   Each acceptance comes with what it landed, and the records of the
//!   changes that brings.
//! - Then the records of each change that moved are compared with the
//!   remote's, by id, and those missing fetched, with the records they
//!   refer to, and the records of the changes those belong to. The changes
//!   that moved are those the remote's events since the last fetch name
//!   ([`Sync::moved`]), so a fetch that brings nothing costs the same
//!   however long the history: only without a cursor, or once it expired,
//!   are all the changes this node knows compared.
//! - Snapshots don't come with records: [`Sync::need`] fetches one when
//!   something reads it, such as a checkout, so a clone fetches one
//!   snapshot, its target's tip. The remote withholds the content of the
//!   paths the user may not read: the trees keep their place, and nothing
//!   reads them (see [`Store::with_withheld`]). A snapshot is marked while
//!   it's fetched ([`Store::fetching`]), so one whose fetch was interrupted
//!   is fetched again rather than taken for whole.
//! - **Pushing** shares revisions, with their snapshots, and the user's
//!   statements. It never lands anything. It first compares each change's
//!   heads with the remote's, and refuses before uploading anything when
//!   the remote has a revision this node lacks: the change is stale, or a
//!   revision changes a path the user may not write. The remote checks the
//!   heads again as it publishes the revisions, so a push racing another
//!   is refused too. Revisions only go to a remote that has the acceptance
//!   they're based on. Each object goes after those it needs: snapshots
//!   and attachments, then revisions, each with its reserved statements,
//!   then the other statements. A statement that
//!   needs one the remote neither has nor gets stays here, and the push
//!   says so.
//!
//! Transfers are bounded, in both directions: a push is split into
//! requests of at most [`Pack::LIMIT`], and a fetch's answers stop there,
//! with the rest asked for again (see [`Part`]). Each request of a push
//! expects the heads the requests before it left. A revision goes in one
//! request with its reserved statements, such as its conflicts and its
//! abandonment, and the other statements about one record go in one
//! request, so the remote never lands a revision whose abandonment, or a
//! result whose replacement, is still to come. Fetched records are
//! written before the chains that refer to them.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::{iter, mem};

use origin_platform::Native;
use origin_shared::access::{Level, PathAccess};
use origin_shared::api::{
    ErrorCode, EventsQuery, Expected, FetchRecords, FetchSnapshot, Inventory, InventoryRequest,
    List, LogQuery, PutQuery,
};
use origin_shared::event::EventKind;
use origin_shared::object::{BadSignature, DecodeError};
use origin_shared::pack::{Pack, Packer, Part};
use origin_shared::{
    Acceptance, AcceptanceId, Base, ChangeId, EncodedObject, Head, ObjectId, ObjectKind, Principal,
    PrincipalName, Record, RecordId, RepoPath, Revision, RevisionId, Source, Statement,
    StatementId, TargetName, TreeId,
};

use crate::annotations::Annotations;
use crate::index::Index;
use crate::permissions::{Effects, Forbidden};
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
    /// The most bytes a push's request holds: [`Pack::LIMIT`], but in
    /// tests.
    pack_limit: usize,
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

/// What a fetch did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetch {
    /// The targets whose chains it extended.
    pub advanced: Vec<Advanced>,
    /// The records it fetched.
    pub fetched: Fetched,
    /// The remote's event cursor to fetch from next time.
    pub cursor: Cursor,
}

/// Where the remote's events are read from: after a sequence number of
/// its op log, which only means something in the log of its generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// The generation of the remote's op log.
    pub generation: String,
    /// The sequence number to read after.
    pub next: u64,
}

/// What the remote's events say moved since a cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Moved {
    /// The remote's cursor after those events: where the next fetch reads
    /// from.
    pub cursor: Cursor,
    /// The changes that have revisions or statements the events name, or
    /// `None` if the events since the cursor are unknown: there was none,
    /// the remote no longer keeps them, or it's of another op log.
    pub changes: Option<HashSet<ChangeId>>,
}

/// What a push shared.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pushed {
    /// The revisions the remote didn't have.
    pub revisions: usize,
    /// The statements the remote didn't have.
    pub statements: usize,
    /// Every object sent: those, and the blobs and trees of their
    /// snapshots the remote lacked.
    pub objects: usize,
    /// The user's statements that stay here: they need a record the remote
    /// neither has nor got, such as a statement they supersede about
    /// another change, or the original they promote.
    pub unsent: Vec<StatementId>,
}

/// The statements a push sends, in order, and those that stay.
struct Statements {
    statements: Vec<(StatementId, Statement)>,
    unsent: Vec<StatementId>,
}

/// What a push sends, and what each of its requests expects of the
/// remote.
#[derive(Default)]
struct Transfer {
    /// The objects, each after those it needs.
    ids: Vec<ObjectId>,
    /// Those that go in the same request as the one before them.
    joined: HashSet<ObjectId>,
    /// The change of each revision sent, and those it evolves from.
    revisions: HashMap<ObjectId, (ChangeId, Vec<RevisionId>)>,
    /// The heads of each change on the remote, as of the requests sent.
    heads: HashMap<ChangeId, Vec<RevisionId>>,
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

/// Items, by position, joined into groups: each group is named by its
/// first item.
struct Groups(Vec<usize>);

impl Groups {
    /// `len` items, each in a group of its own.
    fn new(len: usize) -> Self {
        Self((0..len).collect())
    }

    /// The first item of the group of the one `at`.
    fn root(&mut self, mut at: usize) -> usize {
        while self.0[at] != at {
            self.0[at] = self.0[self.0[at]];
            at = self.0[at];
        }
        at
    }

    /// Joins the groups of `a` and `b`.
    fn join(&mut self, a: usize, b: usize) {
        let (a, b) = (self.root(a), self.root(b));
        self.0[a.max(b)] = a.min(b);
    }
}

impl Transfer {
    /// Adds `revisions`, each in one request with the reserved statements
    /// of `statements` about it, such as its conflicts or its abandonment,
    /// and the statements those need: so the remote never has a revision
    /// without them. Those go after the last revision they need, in one
    /// request with every revision from the first they're about. Returns
    /// the other statements.
    fn add_revisions(
        &mut self,
        revisions: &[(RevisionId, Revision)],
        statements: Vec<(StatementId, Statement)>,
    ) -> Vec<(StatementId, Statement)> {
        let sent: HashMap<RecordId, usize> = revisions
            .iter()
            .enumerate()
            .map(|(at, &(id, _))| (RecordId::Revision(id), at))
            .collect();
        let position = Self::positions(&statements);
        // Those that go with a revision, grouped with those they need,
        // which come before them.
        let mut with: Vec<bool> = statements
            .iter()
            .map(|(_, statement)| {
                statement.kind.is_reserved() && sent.contains_key(&statement.subject)
            })
            .collect();
        let mut groups = Groups::new(statements.len());
        for at in (0..statements.len()).rev() {
            if !with[at] {
                continue;
            }
            for (needed, _) in Dependencies::of_statement(&statements[at].1).records {
                if let Some(&other) = position.get(&needed) {
                    with[other] = true;
                    groups.join(at, other);
                }
            }
        }
        // Each group's revisions: the first a reserved one is about, and
        // the last any needs.
        let mut spans: HashMap<usize, (usize, usize)> = HashMap::new();
        for (at, (_, statement)) in statements.iter().enumerate() {
            if let Some(&revision) = sent.get(&statement.subject)
                && with[at]
            {
                let span = spans.entry(groups.root(at)).or_insert((usize::MAX, 0));
                if statement.kind.is_reserved() {
                    span.0 = span.0.min(revision);
                }
                span.1 = span.1.max(revision);
            }
        }
        let mut joined = vec![false; revisions.len()];
        let mut after = vec![Vec::new(); revisions.len()];
        for &(first, last) in spans.values() {
            joined[first + 1..=last].fill(true);
        }
        for at in (0..statements.len()).filter(|&at| with[at]) {
            after[spans[&groups.root(at)].1].push(statements[at].0);
        }
        for (((id, revision), joined), after) in revisions.iter().zip(joined).zip(after) {
            if joined {
                self.joined.insert((*id).into());
            }
            self.ids.push((*id).into());
            self.revisions.insert(
                (*id).into(),
                (revision.change, revision.predecessors.clone()),
            );
            for statement in after {
                self.joined.insert(statement.into());
                self.ids.push(statement.into());
            }
        }
        statements
            .into_iter()
            .zip(with)
            .filter_map(|(statement, with)| (!with).then_some(statement))
            .collect()
    }

    /// Adds `statements`, each after those of them it needs, so that those
    /// about one record, with those they need or that need them, go in
    /// one request: then a landing waiting on the record never takes a
    /// result whose replacement, or the change's abandonment, is still to
    /// come.
    fn add_statements(&mut self, statements: &[(StatementId, Statement)]) {
        let position = Self::positions(statements);
        let mut groups = Groups::new(statements.len());
        let mut about: HashMap<RecordId, usize> = HashMap::new();
        for (at, (_, statement)) in statements.iter().enumerate() {
            let same_subject = *about.entry(statement.subject).or_insert(at);
            let needed = Dependencies::of_statement(statement)
                .records
                .into_iter()
                .filter_map(|(id, _)| position.get(&id).copied());
            for other in iter::once(same_subject).chain(needed) {
                groups.join(at, other);
            }
        }
        let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for at in 0..statements.len() {
            members.entry(groups.root(at)).or_default().push(at);
        }
        for members in members.into_values() {
            for (nth, at) in members.into_iter().enumerate() {
                let id = statements[at].0.into();
                if nth > 0 {
                    self.joined.insert(id);
                }
                self.ids.push(id);
            }
        }
    }

    /// Where each of `statements` is among them.
    fn positions(statements: &[(StatementId, Statement)]) -> HashMap<ObjectId, usize> {
        statements
            .iter()
            .enumerate()
            .map(|(at, &(id, _))| (id.into(), at))
            .collect()
    }

    /// The heads the changes of `revisions`, which a request sends, must
    /// have on the remote before it: those after the requests before.
    /// Then moves them, as the request does.
    fn expect(&mut self, revisions: &[ObjectId]) -> Vec<Expected> {
        let mut expect: Vec<Expected> = Vec::new();
        for id in revisions {
            let (change, predecessors) = &self.revisions[id];
            let heads = self.heads.entry(*change).or_default();
            if !expect.iter().any(|expected| expected.change == *change) {
                expect.push(Expected {
                    change: *change,
                    heads: heads.clone(),
                });
            }
            heads.retain(|head| !predecessors.contains(head));
            heads.push((*id).into());
        }
        expect
    }
}

impl<'a> Sync<'a> {
    /// The most events a page of [`Sync::moved`] asks for: as many as a
    /// server sends.
    const EVENTS: u32 = 1024;

    /// Syncs `store` with `remote`, as the node `node`, for `user`, who
    /// writes everything until [`Sync::writing`] says otherwise.
    pub fn new(store: &'a Store, remote: &'a Remote, node: PrincipalName, user: Principal) -> Self {
        Self {
            store,
            remote,
            node: Principal::Node(node),
            user,
            writes: PathAccess::uniform(Level::Write),
            pack_limit: Pack::LIMIT,
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

    /// Syncs pushing requests of at most `limit` bytes.
    #[cfg(test)]
    fn with_pack_limit(self, limit: usize) -> Self {
        Self {
            pack_limit: limit,
            ..self
        }
    }

    /// Fetches as the module explains: the chains of the targets this node
    /// follows and of `targets`, then the records of `changes` and of those
    /// this node knows that moved since `cursor`, the remote's event cursor
    /// as of the last fetch, or of all those it knows without one.
    ///
    /// # Errors
    ///
    /// Fails like [`Sync::fetch_chains`] and [`Sync::fetch_changes`].
    pub fn fetch(
        &self,
        targets: &[TargetName],
        changes: &[ChangeId],
        cursor: Option<&Cursor>,
    ) -> Result<Fetch, SyncError> {
        // Read first, so what moves while this fetches is fetched again.
        let moved = self.moved(cursor)?;
        let (advanced, landed) = self.chains(targets)?;
        let mut wanted: Vec<ChangeId> = match &moved.changes {
            // Nothing moved: the index needn't even be read.
            Some(moved) if moved.is_empty() => Vec::new(),
            Some(moved) => {
                let index = Index::open(self.store)?;
                moved
                    .iter()
                    .copied()
                    .filter(|&change| !index.revisions(change).is_empty())
                    .collect()
            }
            None => Index::open(self.store)?.changes().collect(),
        };
        wanted.extend(changes.iter().chain(&landed));
        wanted.sort_unstable();
        wanted.dedup();
        Ok(Fetch {
            advanced,
            fetched: self.fetch_changes(&wanted)?,
            cursor: moved.cursor,
        })
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
        let (advanced, landed) = self.chains(more)?;
        self.fetch_changes(&landed)?;
        Ok(advanced)
    }

    /// [`Sync::fetch_chains`], but for the records of the changes that
    /// landed, which it returns, for the caller to fetch.
    fn chains(&self, more: &[TargetName]) -> Result<(Vec<Advanced>, Vec<ChangeId>), SyncError> {
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
        self.verify(&chains)?;
        let appending: HashSet<ObjectId> = chains
            .iter()
            .flat_map(|chain| &chain.acceptances)
            .map(|&(id, _)| id.into())
            .collect();
        let landed = chains
            .iter()
            .flat_map(|chain| &chain.acceptances)
            .flat_map(|(_, acceptance)| &acceptance.body().landed)
            .map(|&source| match source {
                Source::Revision(id) => id.into(),
                Source::Target(id) => id.into(),
            });
        // Objects found stored stay until the chains refer to them.
        let _held = self.store.hold_objects()?;
        let held = self.store.batch();
        let records = self.closure(&held, landed, &appending)?;
        held.commit()?;

        let mut revised = Self::revised(&records)?;
        revised.sort_unstable();
        revised.dedup();
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
        // The changes that landed come whole, so the next fetch only looks
        // for what moved since.
        Ok((advanced, revised))
    }

    /// The acceptances of `target` after the last this node holds, up to
    /// the remote's head, `theirs`, each checked against its id and target,
    /// and to follow the one before it. `None` if there are none.
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
        let mut before = ours;
        for entry in entries {
            let body = entry.acceptance.body();
            let unverified = |why| SyncError::Unverified {
                target: target.clone(),
                seq: body.seq,
                why,
            };
            if entry.acceptance.encode().id() != entry.id.into() || body.target != target.id() {
                return Err(unverified(Unverified::Id(entry.id)));
            }
            if body.seq != before.map_or(0, |before| before.seq + 1)
                || body.prev != before.map(|before| before.acceptance)
            {
                return Err(unverified(Unverified::Order));
            }
            before = Some(Head {
                seq: body.seq,
                acceptance: entry.id,
            });
            acceptances.push((entry.id, entry.acceptance));
        }
        let Some(to) = before.filter(|_| !acceptances.is_empty()) else {
            return Ok(None);
        };
        Ok(Some(Chain {
            target,
            from: ours,
            to,
            acceptances,
        }))
    }

    /// Checks that the authority each acceptance's governing policy names
    /// signed it, as the module explains. `chains` start with `policy`'s,
    /// whose acceptances may govern those after them once verified. The
    /// policies' snapshots are fetched as needed, and those of `policy`'s
    /// new acceptances always, so the policy in force can be read.
    fn verify(&self, chains: &[Chain]) -> Result<(), SyncError> {
        let mut policies: HashMap<AcceptanceId, Policy> = HashMap::new();
        // The acceptances of `policy` verified so far, and their snapshots.
        let mut verified: HashMap<AcceptanceId, TreeId> = HashMap::new();
        for chain in chains {
            for (id, acceptance) in &chain.acceptances {
                let body = acceptance.body();
                let unverified = |why| SyncError::Unverified {
                    target: chain.target.clone(),
                    seq: body.seq,
                    why,
                };
                // Small, and the policy in force once it's the tip.
                if chain.target == TargetName::POLICY {
                    self.fetch_snapshot(RecordId::Acceptance(*id), body.state, &[])?;
                }
                let governing = Policy::governing(*id, body)
                    .map_err(|error| unverified(Unverified::Policy(error)))?;
                let policy = match policies.entry(governing) {
                    Entry::Occupied(policy) => policy.into_mut(),
                    Entry::Vacant(vacant) => {
                        // Only the first acceptance of `policy` governs
                        // itself: `chain` has it follow nothing.
                        let state = if governing == *id {
                            body.state
                        } else if let Some(&state) = verified.get(&governing) {
                            state
                        } else {
                            self.on_policy_chain(governing)?
                                .ok_or_else(|| unverified(Unverified::OffChain(governing)))?
                        };
                        self.fetch_snapshot(RecordId::Acceptance(governing), state, &[])?;
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
                if chain.target == TargetName::POLICY {
                    verified.insert(*id, body.state);
                }
            }
        }
        Ok(())
    }

    /// The snapshot of `id`, if it's on this node's chain of `policy`,
    /// which it verified as it appended it.
    fn on_policy_chain(&self, id: AcceptanceId) -> Result<Option<TreeId>, SyncError> {
        let acceptance = match self.store.get(id) {
            Ok(acceptance) => acceptance,
            Err(StoreError::NotFound(_)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let body = acceptance.body();
        let on_chain = body.target == TargetName::POLICY.id()
            && self.store.acceptance_at(body.target, body.seq)? == Some(id);
        Ok(on_chain.then_some(body.state))
    }

    /// The remote's cursor after its events since `since`, and the changes
    /// they say moved: those with a revision created or a statement issued
    /// about them, or about a statement about them. Without `since`, or if
    /// the remote no longer keeps its events, or its op log is of another
    /// generation, as once it was restored, the remote's current cursor,
    /// and no changes: then every change may have moved.
    ///
    /// # Errors
    ///
    /// Fails if the remote or the store fails.
    pub fn moved(&self, since: Option<&Cursor>) -> Result<Moved, SyncError> {
        // Should a name not parse, every event comes, and those of other
        // kinds name no change.
        let kinds = ["revision.created", "statement.issued"]
            .into_iter()
            .filter_map(|kind| kind.parse().ok())
            .collect();
        let mut query = EventsQuery {
            kind: List(kinds),
            limit: Some(Self::EVENTS),
            ..EventsQuery::default()
        };
        let everything = |query: &mut EventsQuery| {
            query.cursor = None;
            query.generation = None;
            let page = self.remote.events(query)?;
            Ok(Moved {
                cursor: Cursor {
                    generation: page.generation,
                    next: page.next,
                },
                changes: None,
            })
        };
        let Some(since) = since else {
            return everything(&mut query);
        };
        let mut cursor = since.clone();
        let mut events = Vec::new();
        loop {
            query.cursor = Some(cursor.next);
            query.generation = Some(cursor.generation.clone());
            let page = match self.remote.events(&query) {
                Err(error) if error.code() == Some(ErrorCode::Expired) => {
                    return everything(&mut query);
                }
                page => page?,
            };
            cursor.next = page.next;
            // A page short of the limit holds every event there was.
            let last = page.events.len() < Self::EVENTS as usize;
            events.extend(page.events.into_iter().map(|event| event.kind));
            if last {
                break;
            }
        }
        Ok(Moved {
            cursor,
            changes: Some(self.changes_moved(&events)?),
        })
    }

    /// The changes `events` name, as [`Sync::moved`] says. A statement is
    /// traced to its subject through the events, then this node's records:
    /// one about a record it knows nothing of is about no change it knows.
    fn changes_moved(&self, events: &[EventKind]) -> Result<HashSet<ChangeId>, SyncError> {
        let mut changes = HashSet::new();
        let mut revisions = HashMap::new();
        let mut subjects = HashMap::new();
        for event in events {
            match *event {
                EventKind::RevisionCreated {
                    revision, change, ..
                } => {
                    changes.insert(change);
                    revisions.insert(revision, change);
                }
                EventKind::StatementIssued {
                    statement, subject, ..
                } => {
                    subjects.insert(statement, subject);
                }
                _ => {}
            }
        }
        for &subject in subjects.values() {
            let mut subject = subject;
            // A chain of statements ends at a record, or one unknown.
            let change = loop {
                match subject {
                    RecordId::Revision(id) => match revisions.get(&id) {
                        Some(&change) => break Some(change),
                        None => break self.stored(id)?.map(|revision| revision.change),
                    },
                    RecordId::Statement(id) => match subjects.get(&id) {
                        Some(&next) if next != subject => subject = next,
                        _ => match self.stored(id)? {
                            Some(statement) => subject = statement.subject,
                            None => break None,
                        },
                    },
                    RecordId::Acceptance(_) | RecordId::Snapshot(_) => break None,
                }
            };
            changes.extend(change);
        }
        Ok(changes)
    }

    /// The record `id` names, if this node has it.
    fn stored<I: origin_shared::TypedId>(&self, id: I) -> Result<Option<I::Record>, StoreError> {
        match self.store.get(id) {
            Ok(record) => Ok(Some(record)),
            Err(StoreError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Fetches the records of `changes` the remote has and this node
    /// doesn't: their revisions and the statements about them, with the
    /// records they refer to, and then the records of the changes the
    /// revisions fetched belong to, so every change this node learns of
    /// comes whole. Flags the changes that diverged.
    ///
    /// # Errors
    ///
    /// Fails if the remote or the store fails.
    pub fn fetch_changes(&self, changes: &[ChangeId]) -> Result<Fetched, SyncError> {
        let mut fetched = Fetched::default();
        let mut asked: HashSet<ChangeId> = changes.iter().copied().collect();
        let mut next: Vec<ChangeId> = asked.iter().copied().collect();
        let mut revised = BTreeSet::new();
        // Objects found stored stay until what refers to them is written.
        let _held = self.store.hold_objects()?;
        while !next.is_empty() {
            let inventories = self.remote.inventory(&InventoryRequest {
                changes: mem::take(&mut next),
            })?;
            let ids = inventories.iter().flat_map(|inventory| {
                let revisions = inventory.revisions.iter().map(|&id| ObjectId::from(id));
                revisions.chain(inventory.statements.iter().map(|&id| id.into()))
            });
            let held = self.store.batch();
            let records = self.closure(&held, ids, &HashSet::new())?;
            held.commit()?;
            for change in Self::revised(&records)? {
                fetched.revisions += 1;
                revised.insert(change);
                if asked.insert(change) {
                    next.push(change);
                }
            }
            fetched.statements += records
                .iter()
                .filter(|record| record.kind().ok() == Some(ObjectKind::Statement))
                .count();
            if !records.is_empty() {
                self.store.write_all(&records)?;
            }
        }
        if !revised.is_empty() {
            let annotations = Annotations::open(self.store)?;
            for change in revised {
                annotations.flag_divergence(change, self.node.clone())?;
            }
        }
        Ok(fetched)
    }

    /// The change of each revision of `records`, one for each.
    fn revised(records: &[EncodedObject]) -> Result<Vec<ChangeId>, SyncError> {
        let mut changes = Vec::new();
        for record in records {
            if record.kind().ok() == Some(ObjectKind::Revision) {
                let revision: Revision =
                    record.decode_as().map_err(|source| SyncError::Malformed {
                        id: record.id(),
                        source,
                    })?;
                changes.push(revision.change);
            }
        }
        Ok(changes)
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
            let mut request = FetchRecords {
                ids: mem::take(&mut wanted),
            };
            while !request.ids.is_empty() {
                let Part { pack, more } = self.remote.records(&request)?;
                // A part that stops short holds the records up to the last
                // it has: the rest come from asking again.
                let rest = if more {
                    let position: HashMap<ObjectId, usize> = request
                        .ids
                        .iter()
                        .enumerate()
                        .map(|(position, &id)| (id, position))
                        .collect();
                    let last = pack
                        .objects
                        .iter()
                        .filter_map(|object| position.get(&object.id()))
                        .max();
                    last.map_or_else(Vec::new, |&last| request.ids.split_off(last + 1))
                } else {
                    Vec::new()
                };
                for object in pack.objects {
                    if matches!(
                        object.kind(),
                        Ok(ObjectKind::Revision | ObjectKind::Statement)
                    ) {
                        let decoded = object.decode().map_err(|source| SyncError::Malformed {
                            id: object.id(),
                            source,
                        })?;
                        for (id, _) in Dependencies::of(&decoded).records {
                            want(id, &mut wanted)?;
                        }
                    }
                    objects.push(object);
                }
                request.ids = rest;
            }
        }
        Ok(objects)
    }

    /// Fetches the trees and blobs of the snapshot of `record`, a revision
    /// or an acceptance this node has, unless the store holds it whole,
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

    /// Fetches the whole snapshot of `record` again, whatever the store
    /// holds: what the store lacks of a snapshot it has the root of, which
    /// reading it found.
    ///
    /// # Errors
    ///
    /// Fails like [`Sync::need`].
    pub fn repair(&self, record: RecordId) -> Result<(), SyncError> {
        let snapshot = self
            .store
            .snapshot_of(record)?
            .ok_or(SyncError::NoSnapshot(record))?;
        let request = FetchSnapshot {
            of: record,
            have: Vec::new(),
            paths: Vec::new(),
            skip: 0,
        };
        self.fetch_parts(request, Some(snapshot))
    }

    /// [`Sync::need`], given the snapshot of `record`.
    fn fetch_snapshot(
        &self,
        record: RecordId,
        snapshot: TreeId,
        have: &[TreeId],
    ) -> Result<bool, SyncError> {
        if self.store.holds(snapshot)? {
            return Ok(false);
        }
        let request = FetchSnapshot {
            of: record,
            have: self.store.held_whole(have.iter().copied())?,
            paths: Vec::new(),
            skip: 0,
        };
        self.fetch_parts(request, Some(snapshot))?;
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
        let request = FetchSnapshot {
            of: record,
            have: Vec::new(),
            paths,
            skip: 0,
        };
        self.fetch_parts(request, None)
    }

    /// Fetches what `request` asks for, part after part, in one batch. With
    /// `whole`, the snapshot it fetches whole, which is marked until the
    /// batch is durable.
    fn fetch_parts(
        &self,
        mut request: FetchSnapshot,
        whole: Option<TreeId>,
    ) -> Result<(), SyncError> {
        // Objects found stored stay until the trees referring to them are.
        let _held = self.store.hold_objects()?;
        let batch = self.store.batch();
        let fetching = whole
            .map(|snapshot| self.store.fetching(snapshot, &batch))
            .transpose()?;
        loop {
            let Part { pack, more } = self.remote.snapshot(&request)?;
            batch.stage_all(&pack.objects)?;
            if !more || pack.objects.is_empty() {
                break;
            }
            request.skip += pack.objects.len() as u64;
        }
        batch.commit()?;
        if let Some(fetching) = fetching {
            fetching.done()?;
        }
        Ok(())
    }

    /// Shares `changes`, and the changes below them, with the remote, as
    /// the module explains.
    ///
    /// # Errors
    ///
    /// Fails if a change has no revision here, is stale, or is based on an
    /// acceptance the remote doesn't have, or if the remote or the store
    /// fails. Nothing is sent then, unless the change moved on the remote
    /// while the push was sending: then the snapshots and records sent
    /// before stay there.
    pub fn push(&self, changes: &[ChangeId]) -> Result<Pushed, SyncError> {
        let index = Index::open(self.store)?;
        let changes = self.stack(&index, changes)?;
        let inventories = self.inventories(&index, &changes)?;
        let mut shared: HashSet<ObjectId> = HashSet::new();
        for inventory in &inventories {
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
        let revisions = Self::bottom_up(revisions);
        let mut checked = HashSet::new();
        for (id, revision) in &revisions {
            if let Base::Acceptance(base) = revision.base
                && checked.insert(base)
            {
                self.check_shared(*id, revision, base)?;
            }
            self.check_writes(revision)?;
        }

        // Snapshots and attachments first, then revisions, then statements.
        let mut content = Vec::new();
        let mut seen = HashSet::new();
        for (id, revision) in &revisions {
            // The remote has these snapshots, or gets them first.
            let mut have = vec![self.store.base_snapshot(revision.base)?];
            for &predecessor in &revision.predecessors {
                if shared.contains(&predecessor.into()) {
                    have.push(self.store.get(predecessor)?.snapshot);
                }
            }
            let have = self.store.held_whole(have)?;
            // Children first: a tree goes after what it holds.
            for placed in self
                .store
                .added(revision.snapshot, &have, &[], None)?
                .into_iter()
                .rev()
            {
                if seen.insert(placed.entry.id) {
                    content.push(placed.entry.id);
                }
            }
            if seen.insert(revision.description.into()) {
                content.push(revision.description.into());
            }
            shared.insert((*id).into());
        }
        let Statements { statements, unsent } = self.statements(&index, &changes, &mut shared)?;
        for (_, statement) in &statements {
            if let Some(payload) = statement.payload
                && self.store.contains(payload.into())?
                && seen.insert(payload.into())
            {
                content.push(payload.into());
            }
        }
        let objects = content.len() + revisions.len() + statements.len();
        let mut transfer = Transfer {
            ids: content,
            heads: inventories
                .into_iter()
                .map(|inventory| (inventory.change, inventory.heads))
                .collect(),
            ..Transfer::default()
        };
        let statements = transfer.add_revisions(&revisions, statements);
        transfer.add_statements(&statements);
        if let Err(error) = self.send(transfer) {
            // The remote refused as a change moved: say which.
            if let SyncError::Remote(remote) = &error
                && remote.code() == Some(ErrorCode::Conflict)
            {
                self.inventories(&Index::open(self.store)?, &changes)?;
            }
            return Err(error);
        }
        Ok(Pushed {
            revisions: revisions.len(),
            statements: statements.len(),
            objects,
            unsent,
        })
    }

    /// What the remote holds of `changes`, refusing if it has a head of one
    /// this node lacks: the change is stale.
    fn inventories(
        &self,
        index: &Index,
        changes: &[ChangeId],
    ) -> Result<Vec<Inventory>, SyncError> {
        let inventories = self.remote.inventory(&InventoryRequest {
            changes: changes.to_vec(),
        })?;
        for inventory in &inventories {
            let local = index.revisions(inventory.change);
            if let Some(&head) = inventory.heads.iter().find(|head| !local.contains(head)) {
                return Err(SyncError::Stale {
                    change: inventory.change,
                    head,
                });
            }
        }
        Ok(inventories)
    }

    /// `revisions`, each after those of them it evolves from, is based on
    /// or depends on.
    fn bottom_up(revisions: Vec<(RevisionId, Revision)>) -> Vec<(RevisionId, Revision)> {
        let position: HashMap<RevisionId, usize> = revisions
            .iter()
            .enumerate()
            .map(|(position, &(id, _))| (id, position))
            .collect();
        let mut placed = vec![false; revisions.len()];
        let mut order = Vec::with_capacity(revisions.len());
        for start in 0..revisions.len() {
            // Depth first, placing each revision once those below it are.
            let mut stack = vec![(start, false)];
            while let Some((at, below_placed)) = stack.pop() {
                if placed[at] {
                    continue;
                }
                if below_placed {
                    placed[at] = true;
                    order.push(at);
                    continue;
                }
                stack.push((at, true));
                let revision = &revisions[at].1;
                let base = match revision.base {
                    Base::Revision(base) => Some(base),
                    Base::Acceptance(_) => None,
                };
                let below = base.iter().chain(&revision.deps);
                for below in below.chain(&revision.predecessors) {
                    if let Some(&below) = position.get(below)
                        && !placed[below]
                    {
                        stack.push((below, false));
                    }
                }
            }
        }
        let mut revisions: Vec<_> = revisions.into_iter().map(Some).collect();
        order
            .into_iter()
            .filter_map(|at| revisions[at].take())
            .collect()
    }

    /// The user's statements about the revisions of `changes`, and about
    /// statements about them, that the remote lacks, each after those of
    /// them it needs, adding each to `shared`; and those that need a record
    /// that isn't in `shared`, which stay. Others' statements are theirs to
    /// share, and this node's own, like divergence, the remote finds
    /// itself.
    fn statements(
        &self,
        index: &Index,
        changes: &[ChangeId],
        shared: &mut HashSet<ObjectId>,
    ) -> Result<Statements, SyncError> {
        let mut subjects: Vec<RecordId> = changes
            .iter()
            .flat_map(|&change| index.revisions(change))
            .map(|&id| RecordId::Revision(id))
            .collect();
        let mut waiting = Vec::new();
        let mut seen = HashSet::new();
        while let Some(subject) = subjects.pop() {
            for &id in index.statements(subject) {
                if !seen.insert(id) {
                    continue;
                }
                subjects.push(RecordId::Statement(id));
                if shared.contains(&id.into()) {
                    continue;
                }
                let statement = self.store.get(id)?;
                if statement.issuer == self.user {
                    waiting.push((id, statement));
                }
            }
        }
        let mut ordered = Vec::new();
        loop {
            let before = waiting.len();
            waiting.retain(|(id, statement): &(StatementId, Statement)| {
                let ready = Dependencies::of_statement(statement)
                    .records
                    .iter()
                    .all(|(needed, _)| shared.contains(needed));
                if ready {
                    shared.insert((*id).into());
                    ordered.push((*id, statement.clone()));
                }
                !ready
            });
            if waiting.len() == before {
                break;
            }
        }
        Ok(Statements {
            statements: ordered,
            unsent: waiting.into_iter().map(|(id, _)| id).collect(),
        })
    }

    /// Puts the objects of `transfer`, in order, in requests of at most
    /// [`Pack::LIMIT`], keeping together those it joins.
    fn send(&self, mut transfer: Transfer) -> Result<(), SyncError> {
        let ids = mem::take(&mut transfer.ids);
        let mut packer = Packer::new(self.pack_limit);
        let mut together = Vec::new();
        let mut filling = Vec::new();
        self.store.read_each(&ids, |object| {
            if !transfer.joined.contains(&object.id()) {
                let objects = mem::take(&mut together);
                self.add(&mut packer, objects, &mut filling, &mut transfer)?;
            }
            together.push(object);
            Ok::<_, SyncError>(true)
        })?;
        self.add(&mut packer, together, &mut filling, &mut transfer)?;
        if !packer.is_empty() {
            self.put(packer.finish(), &filling, &mut transfer)?;
        }
        Ok(())
    }

    /// Adds `together` to `packer`, all in one pack: if they don't fit,
    /// first puts the pack it filled, whose revisions are `filling`.
    fn add(
        &self,
        packer: &mut Packer,
        together: Vec<EncodedObject>,
        filling: &mut Vec<ObjectId>,
        transfer: &mut Transfer,
    ) -> Result<(), SyncError> {
        let revisions: Vec<ObjectId> = together
            .iter()
            .map(EncodedObject::id)
            .filter(|id| transfer.revisions.contains_key(id))
            .collect();
        if let Some(full) = packer.add_together(together) {
            self.put(full, &mem::take(filling), transfer)?;
        }
        filling.extend(revisions);
        Ok(())
    }

    /// Puts `pack`, whose revisions are `revisions`, if their changes have
    /// the heads `transfer` expects of them.
    fn put(
        &self,
        pack: Pack,
        revisions: &[ObjectId],
        transfer: &mut Transfer,
    ) -> Result<(), SyncError> {
        let query = PutQuery {
            expect: List(transfer.expect(revisions)),
        };
        self.remote.push(pack, &query)?;
        Ok(())
    }

    /// Checks that `revision` has an effect the user may give it (see
    /// [`Effects`]).
    fn check_writes(&self, revision: &Revision) -> Result<(), SyncError> {
        match Effects::new(self.store, self.store, &self.writes).check(revision)? {
            Some(forbidden) => Err(forbidden.into()),
            None => Ok(()),
        }
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
    /// It doesn't follow the acceptance before it.
    #[error("it doesn't follow the acceptance before it")]
    Order,
    /// It names no policy, or another than the one governing it.
    #[error("it names the wrong policy")]
    Policy(#[source] PolicyError),
    /// The policy it names isn't on the chain of `policy`.
    #[error("its policy {} isn't on the chain of `policy`", .0.short())]
    OffChain(AcceptanceId),
    /// Its policy has no `policy.toml`.
    #[error("its policy has no policy.toml")]
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
    #[error("the remote would refuse the push, as of your rights when you last fetched")]
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
    use origin_shared::event::Filter;
    use origin_shared::{AcceptanceBody, Kind, NodeKey, Verdict};

    use super::*;
    use crate::events::Events;
    use crate::queue::tests::{Repo, ci_test};
    use crate::service::{Node, OriginService};
    use crate::tree::Snapshots;
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
            Self::answered_in_parts(server, Pack::LIMIT)
        }

        /// A client of a server answering in parts of `limit` bytes.
        fn answered_in_parts(server: &Repo, limit: usize) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::new(dir.path().join(".origin"), Native::default());
            store.init().unwrap();
            let node = Node {
                name: "queue".parse().unwrap(),
                key: Arc::new(NodeKey::from_bytes(&[7; 32])),
            };
            let service = OriginService::new(server.store.clone(), node).with_pack_limit(limit);
            let remote = Remote::Local(Box::new(service));
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

    /// Writes a snapshot of `policy.toml` making `key` the key of
    /// `node:queue`, the authority of `policy`, to `store`.
    fn policy_snapshot(store: &Store, key: &NodeKey) -> TreeId {
        let policy = format!(
            "[nodes]\nqueue = \"{}\"\n[targets.policy]\nauthority = \"node:queue\"\n{MAIN_RULES}",
            key.public()
        );
        let (file, tree) = Policy::snapshot(&policy);
        store.write_all(&[file, tree.clone()]).unwrap();
        tree.id().into()
    }

    #[test]
    fn fetches_verified_chains_and_what_they_landed_but_not_snapshots() {
        let server = Repo::new(MAIN_RULES);
        let landed = server.change_on_main(1, &[("a", "a")]);
        let result = server
            .annotations()
            .issue(&ci_test(landed, "node:ci"), None)
            .unwrap();
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
        assert!(
            client.store.contains(result.into()).unwrap(),
            "with the records of its change"
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
    fn only_the_chain_of_policy_governs() {
        let server = Repo::new(MAIN_RULES);
        let client = Client::new(&server);
        client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
        // An acceptance of `policy` off its chain, signed by another key,
        // which the client stores as the base of a revision it fetches.
        let attacker = NodeKey::from_bytes(&[8; 32]);
        let fake = AcceptanceBody {
            target: TargetName::POLICY.id(),
            seq: 40,
            prev: None,
            state: policy_snapshot(&server.store, &attacker),
            landed: Vec::new(),
            evidence: Vec::new(),
            policy: None,
        }
        .sign(&attacker);
        let fake_id: AcceptanceId = fake.encode().id().into();
        server.store.write_all(&[fake.encode()]).unwrap();
        server.revision(9, &[], Base::Acceptance(fake_id), &[("a", "a")]);
        client
            .sync()
            .fetch_changes(&[ChangeId::from_bytes([9; 16])])
            .unwrap();
        assert!(client.store.contains(fake_id.into()).unwrap());

        let tip = server.tip(&TargetName::MAIN);
        let forged = AcceptanceBody {
            target: TargetName::MAIN.id(),
            seq: tip.seq + 1,
            prev: Some(tip.acceptance),
            state: server.store.get(tip.acceptance).unwrap().body().state,
            landed: Vec::new(),
            evidence: Vec::new(),
            policy: Some(fake_id),
        }
        .sign(&attacker);
        server.store.append(&TargetName::MAIN, &forged).unwrap();
        let error = client.sync().fetch_chains(&[]).unwrap_err();
        assert!(
            matches!(
                error,
                SyncError::Unverified { why: Unverified::OffChain(id), .. } if id == fake_id
            ),
            "{error}"
        );
        assert_eq!(client.store.head(&TargetName::MAIN).unwrap(), Some(tip));
    }

    #[test]
    fn each_acceptance_of_policy_is_governed_by_the_one_before() {
        // The policy replaces the authority's key, then the old key extends
        // it, naming the policy it was replaced in, or the one before it.
        let extended_by_the_old_key = |names_genesis: bool| {
            let server = Repo::new(MAIN_RULES);
            let client = Client::new(&server);
            client.sync().fetch_chains(&[]).unwrap();
            let genesis = server.tip(&TargetName::POLICY).acceptance;
            let old = NodeKey::from_bytes(&[7; 32]);
            let rotation = AcceptanceBody {
                target: TargetName::POLICY.id(),
                seq: 1,
                prev: Some(genesis),
                state: policy_snapshot(&server.store, &NodeKey::from_bytes(&[8; 32])),
                landed: Vec::new(),
                evidence: Vec::new(),
                policy: Some(genesis),
            }
            .sign(&old);
            server.store.append(&TargetName::POLICY, &rotation).unwrap();
            client.sync().fetch_chains(&[]).unwrap();
            let rotation: AcceptanceId = rotation.encode().id().into();
            let reverted = AcceptanceBody {
                target: TargetName::POLICY.id(),
                seq: 2,
                prev: Some(rotation),
                state: server.store.get(genesis).unwrap().body().state,
                landed: Vec::new(),
                evidence: Vec::new(),
                policy: Some(if names_genesis { genesis } else { rotation }),
            }
            .sign(&old);
            server.store.append(&TargetName::POLICY, &reverted).unwrap();
            client.sync().fetch_chains(&[]).unwrap_err()
        };
        let error = extended_by_the_old_key(true);
        assert!(
            matches!(
                error,
                SyncError::Unverified {
                    seq: 2,
                    why: Unverified::Policy(PolicyError::NotPredecessor(2)),
                    ..
                }
            ),
            "{error}"
        );
        let error = extended_by_the_old_key(false);
        assert!(
            matches!(
                error,
                SyncError::Unverified {
                    seq: 2,
                    why: Unverified::Signature(_),
                    ..
                }
            ),
            "{error}"
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
    fn pushes_statements_after_those_they_need() {
        let server = Repo::new(MAIN_RULES);
        let client = Client::new(&server);
        client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
        let base = main_tip(&client.store);
        let first = client.revision(1, &[], base, &[("a", "1")]);
        let second = client.revision(1, &[first], base, &[("a", "2")]);
        let pass = ci_test(first, "user:ada");
        let fail = Statement {
            subject: RecordId::Revision(second),
            verdict: Some(Verdict::Fail),
            supersedes: Some(pass.encode().id().into()),
            ..pass.clone()
        };
        client
            .store
            .write_all(&[fail.encode(), pass.encode()])
            .unwrap();

        let change = ChangeId::from_bytes([1; 16]);
        let pushed = client.sync().push(&[change]).unwrap();
        assert_eq!((pushed.statements, &pushed.unsent[..]), (2, &[][..]));
        for statement in [&pass, &fail] {
            assert!(server.store.contains(statement.encode().id()).unwrap());
        }

        // One superseding a statement the remote lacks, about a change the
        // push leaves out, stays, and the push says so.
        let other = client.revision(2, &[], base, &[("b", "1")]);
        let old = ci_test(other, "user:ada");
        let newer = Statement {
            supersedes: Some(old.encode().id().into()),
            ..fail.clone()
        };
        client
            .store
            .write_all(&[old.encode(), newer.encode()])
            .unwrap();
        let pushed = client.sync().push(&[change]).unwrap();
        assert_eq!(pushed.statements, 0);
        assert_eq!(pushed.unsent, [StatementId::from(newer.encode().id())]);
    }

    #[test]
    fn transfers_go_in_parts_in_dependency_order() {
        let server = Repo::new(MAIN_RULES);
        // Small enough for about one object a part.
        let limit = 200;
        let client = Client::answered_in_parts(&server, limit);
        client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
        let base = main_tip(&client.store);
        let files: Vec<(String, String)> = (0..20)
            .map(|i| (format!("src/{}/{i}", i % 3), format!("content {i}")))
            .collect();
        let files: Vec<_> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        let first = client.revision(1, &[], base, &files);
        let second = client.revision(1, &[first], base, &files[1..]);
        let statement = ci_test(second, "user:ada").encode();
        client
            .store
            .write_all(std::slice::from_ref(&statement))
            .unwrap();
        // The remote refuses any object sent before one it needs.
        let change = ChangeId::from_bytes([1; 16]);
        let pushed = client
            .sync()
            .with_pack_limit(limit)
            .push(&[change])
            .unwrap();
        assert_eq!((pushed.revisions, pushed.statements), (2, 1));

        let other = Client::answered_in_parts(&server, limit);
        other.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
        let fetched = other.sync().fetch_changes(&[change]).unwrap();
        assert_eq!(
            fetched,
            Fetched {
                revisions: 2,
                statements: 1
            }
        );
        let record = RecordId::Revision(first);
        let request = FetchSnapshot {
            of: record,
            have: Vec::new(),
            paths: Vec::new(),
            skip: 0,
        };
        let part = other.remote.snapshot(&request).unwrap();
        assert!(part.more && part.pack.objects.len() < 10, "{part:?}");
        assert!(other.sync().need(record, &[]).unwrap());
        let snapshot = other.store.snapshot_of(record).unwrap().unwrap();
        let empty = build(&other.store, &[]);
        assert_eq!(other.store.diff(empty, snapshot).unwrap().len(), 20);
    }

    #[test]
    fn interrupted_snapshot_fetches_are_fetched_again() {
        let server = Repo::new(MAIN_RULES);
        let revision = server.change_on_main(1, &[("src/a", "a")]);
        let client = Client::new(&server);
        client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
        client
            .sync()
            .fetch_changes(&[ChangeId::from_bytes([1; 16])])
            .unwrap();
        let record = RecordId::Revision(revision);
        let snapshot = client.store.snapshot_of(record).unwrap().unwrap();
        let empty = build(&client.store, &[]);

        // A fetch cut short once the root was published, not what it holds.
        let batch = client.store.batch();
        let fetching = client.store.fetching(snapshot, &batch).unwrap();
        batch
            .stage_all(&[server.store.read(snapshot.into()).unwrap()])
            .unwrap();
        batch.commit().unwrap();
        drop(fetching);
        assert!(!client.store.holds(snapshot).unwrap());
        assert!(client.sync().need(record, &[]).unwrap(), "fetched again");
        assert!(client.store.holds(snapshot).unwrap());
        assert_eq!(client.store.diff(empty, snapshot).unwrap().len(), 1);

        // One that lacks a tree, unmarked, is repaired whole.
        let src = Snapshots::new(&client.store)
            .entry_at(snapshot, &RepoPath::new("src").unwrap())
            .unwrap()
            .unwrap();
        client.store.remove(&[src.id]).unwrap();
        assert!(client.store.diff(empty, snapshot).is_err());
        client.sync().repair(record).unwrap();
        assert_eq!(client.store.diff(empty, snapshot).unwrap().len(), 1);
    }

    #[test]
    fn the_remote_events_say_which_changes_moved() {
        let server = Repo::new(MAIN_RULES);
        let first = server.change_on_main(1, &[("a", "a")]);
        let second = server.change_on_main(2, &[("b", "b")]);
        let client = Client::new(&server);
        let both = [1, 2].map(|change| ChangeId::from_bytes([change; 16]));
        client.sync().fetch_changes(&both).unwrap();
        let moved = client.sync().moved(None).unwrap();
        assert_eq!(moved.changes, None, "without a cursor, any change may have");
        let cursor = moved.cursor;
        let nothing = client.sync().moved(Some(&cursor)).unwrap();
        assert_eq!(nothing.changes, Some(HashSet::new()));
        assert_eq!(nothing.cursor, cursor);

        // A statement about a statement about the second change, and a
        // revision of the first.
        let annotations = server.annotations();
        let result = annotations
            .issue(&ci_test(second, "node:ci"), None)
            .unwrap();
        let flaky = Statement {
            subject: RecordId::Statement(result),
            kind: "flaky".parse().unwrap(),
            verdict: None,
            inputs: None,
            ..ci_test(second, "user:ada")
        };
        annotations.issue(&flaky, None).unwrap();
        let tip = main_tip(&server.store);
        server.revision(1, &[first], tip, &[("a", "again")]);
        let moved = client.sync().moved(Some(&cursor)).unwrap();
        assert_eq!(moved.changes, Some(HashSet::from(both)));
        assert!(moved.cursor.next > cursor.next);
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

    #[test]
    fn each_part_of_a_push_expects_the_heads_the_parts_before_left() {
        let change = ChangeId::from_bytes([1; 16]);
        let [r0, r1, r2] = [1, 2, 3].map(|n| RevisionId::from(ObjectId::from_bytes([n; 32])));
        let mut transfer = Transfer {
            heads: HashMap::from([(change, vec![r0])]),
            revisions: HashMap::from([
                (r1.into(), (change, vec![r0])),
                (r2.into(), (change, vec![r1])),
            ]),
            ..Transfer::default()
        };
        let expected = |heads: Vec<RevisionId>| vec![Expected { change, heads }];
        assert_eq!(transfer.expect(&[r1.into()]), expected(vec![r0]));
        assert_eq!(transfer.expect(&[r2.into()]), expected(vec![r1]));
        assert_eq!(transfer.expect(&[]), []);
    }

    #[test]
    fn revisions_go_with_their_reserved_statements() {
        let [first, second] = [1, 2].map(|n| RevisionId::from(ObjectId::from_bytes([n; 32])));
        let revision = |change| Revision {
            change: ChangeId::from_bytes([change; 16]),
            predecessors: Vec::new(),
            base: Base::Revision(first),
            deps: Vec::new(),
            snapshot: TreeId::from(ObjectId::from_bytes([3; 32])),
            description: ObjectId::from_bytes([4; 32]).into(),
            author: "user:ada".parse().unwrap(),
        };
        let conflict = Statement {
            kind: Kind::CONFLICT,
            ..ci_test(first, "user:ada")
        };
        let result = ci_test(first, "user:ada");
        let abandon = Statement {
            kind: Kind::ABANDON,
            ..ci_test(second, "user:ada")
        };
        // About the second, but it needs the first's conflict: all go in
        // one request with both revisions.
        let listing = Statement {
            supersedes: Some(conflict.encode().id().into()),
            ..Statement {
                kind: Kind::CONFLICT,
                ..ci_test(second, "user:ada")
            }
        };
        let statements: Vec<_> = [conflict, result, abandon, listing]
            .into_iter()
            .map(|statement| (statement.encode().id().into(), statement))
            .collect();
        let [conflict, result, abandon, listing] =
            [0, 1, 2, 3].map(|at| ObjectId::from(statements[at].0));
        let mut transfer = Transfer::default();
        let others =
            transfer.add_revisions(&[(first, revision(1)), (second, revision(2))], statements);
        assert_eq!(
            others
                .iter()
                .map(|&(id, _)| id.into())
                .collect::<Vec<ObjectId>>(),
            [result]
        );
        assert_eq!(
            transfer.ids,
            [first.into(), second.into(), conflict, abandon, listing]
        );
        assert_eq!(
            transfer.joined,
            HashSet::from([second.into(), conflict, abandon, listing])
        );

        // Alone, each goes right after its revision.
        let alone = |statement: Statement| (statement.encode().id().into(), statement);
        let statements = vec![
            alone(Statement {
                kind: Kind::ABANDON,
                ..ci_test(first, "user:ada")
            }),
            alone(Statement {
                kind: Kind::DIVERGENT,
                ..ci_test(second, "user:ada")
            }),
        ];
        let [abandon, divergent] = [0, 1].map(|at| ObjectId::from(statements[at].0));
        let mut transfer = Transfer::default();
        transfer.add_revisions(&[(first, revision(1)), (second, revision(2))], statements);
        assert_eq!(
            transfer.ids,
            [first.into(), abandon, second.into(), divergent]
        );
        assert_eq!(transfer.joined, HashSet::from([abandon, divergent]));
    }

    #[test]
    fn statements_about_one_record_go_in_one_request() {
        let [first, second] = [1, 2].map(|n| RevisionId::from(ObjectId::from_bytes([n; 32])));
        let pass = ci_test(first, "user:ada");
        let pass_id = StatementId::from(pass.encode().id());
        let fail = Statement {
            verdict: Some(Verdict::Fail),
            supersedes: Some(pass_id),
            ..pass.clone()
        };
        let other = ci_test(second, "user:ada");
        // About the result, and what it needs: in one request with it.
        let flaky = Statement {
            subject: RecordId::Statement(pass_id),
            kind: "flaky".parse().unwrap(),
            ..other.clone()
        };
        let statements: Vec<_> = [pass, other, fail, flaky]
            .into_iter()
            .map(|statement| (statement.encode().id().into(), statement))
            .collect();
        let mut transfer = Transfer::default();
        transfer.add_statements(&statements);
        let [pass, other, fail, flaky] = [0, 1, 2, 3].map(|at| ObjectId::from(statements[at].0));
        assert_eq!(transfer.ids, [pass, fail, flaky, other]);
        assert_eq!(transfer.joined, HashSet::from([fail, flaky]));
    }

    #[test]
    fn a_push_in_parts_never_lands_an_abandoned_revision() {
        use std::os::unix::fs::PermissionsExt;
        // Requests of one object, or of the default size, which the
        // revision and its content fill.
        for fill in [false, true] {
            let server = Repo::new(MAIN_RULES);
            let client = Client::new(&server);
            client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
            let revision = if fill {
                let mut revision = Revision {
                    change: ChangeId::from_bytes([1; 16]),
                    predecessors: Vec::new(),
                    base: main_tip(&client.store),
                    deps: Vec::new(),
                    snapshot: build(&client.store, &[("a", "gone")]),
                    description: EncodedObject::attachment(b"").id().into(),
                    author: "user:ada".parse().unwrap(),
                };
                client
                    .store
                    .write_all(&[EncodedObject::blob(b"gone")])
                    .unwrap();
                let wire = |objects: Vec<EncodedObject>| Pack { objects }.encode().len();
                let content = client
                    .store
                    .added(revision.snapshot, &[], &[], None)
                    .unwrap()
                    .iter()
                    .map(|placed| client.store.read(placed.entry.id).unwrap())
                    .collect();
                let room = Pack::LIMIT
                    - wire(content)
                    - wire(vec![revision.encode(), EncodedObject::attachment(b"")]);
                let description = EncodedObject::attachment(&vec![b'x'; room]);
                revision.description = description.id().into();
                let revision = revision.encode();
                client
                    .store
                    .write_all(&[description, revision.clone()])
                    .unwrap();
                revision.id().into()
            } else {
                client.revision(1, &[], main_tip(&client.store), &[("a", "gone")])
            };
            let abandonment = Statement {
                kind: Kind::ABANDON,
                verdict: None,
                ..ci_test(revision, "user:ada")
            }
            .encode();
            client
                .store
                .write_all(std::slice::from_ref(&abandonment))
                .unwrap();
            // The remote can't take the abandonment.
            let hex = abandonment.id().to_hex();
            let shard = server.store.root().join("objects/loose").join(&hex[..2]);
            std::fs::create_dir_all(&shard).unwrap();
            std::fs::set_permissions(&shard, std::fs::Permissions::from_mode(0o555)).unwrap();
            let limit = if fill { Pack::LIMIT } else { 1 };
            let pushed = client
                .sync()
                .with_pack_limit(limit)
                .push(&[ChangeId::from_bytes([1; 16])]);
            std::fs::set_permissions(&shard, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(pushed.is_err(), "{pushed:?}");
            assert!(!server.store.contains(revision.into()).unwrap());
            // Pushed again, it's abandoned there too.
            let change = ChangeId::from_bytes([1; 16]);
            client
                .sync()
                .with_pack_limit(limit)
                .push(&[change])
                .unwrap();
            let landed =
                server
                    .queue()
                    .land_change(&mut server.annotations(), change, &TargetName::MAIN);
            assert!(
                matches!(landed, Err(crate::queue::LandError::Abandoned(_))),
                "{landed:?}"
            );
        }
    }

    #[test]
    fn queue_rebases_and_pushes_never_make_a_change_diverge() {
        let server = Repo::new(MAIN_RULES);
        server.change_on_main(1, &[("a", "a")]);
        let original = server.change_on_main(2, &[("b", "b")]);
        server.land(1);
        let change = ChangeId::from_bytes([2; 16]);
        // A push whose check of the heads passed, about to publish a
        // successor of the head, while the queue rebases that head.
        let pushing = server.store.lock_pushes().unwrap();
        let description = EncodedObject::blob(b"concurrent edit");
        let successor = Revision {
            predecessors: vec![original],
            description: description.id().into(),
            ..server.store.get(original).unwrap()
        }
        .encode();
        let landed = std::thread::scope(|scope| {
            let landing = scope.spawn(|| {
                server
                    .queue()
                    .land_change(&mut server.annotations(), change, &TargetName::MAIN)
            });
            std::thread::sleep(std::time::Duration::from_millis(100));
            server
                .store
                .write_all(&[description, successor.clone()])
                .unwrap();
            drop(pushing);
            landing.join().unwrap()
        });
        // The queue waited for the push, and found the change moved: or
        // had it come late, rebased the successor.
        let heads = Index::open(&server.store).unwrap().heads(change);
        assert_eq!(heads.len(), 1, "{landed:?}");
        assert!(
            matches!(landed, Err(crate::queue::LandError::Moved(moved)) if moved == change)
                || landed.is_ok(),
            "{landed:?}"
        );
    }

    #[test]
    fn cursors_of_another_op_log_read_everything() {
        let server = Repo::new(MAIN_RULES);
        let first = server.change_on_main(1, &[("a", "a")]);
        let change = ChangeId::from_bytes([1; 16]);
        let client = Client::new(&server);
        let fetch = client
            .sync()
            .fetch(&[TargetName::MAIN], &[change], None)
            .unwrap();
        // The remote was restored, with a new op log, and has a revision
        // since: a cursor of the old log, even ahead of the new one's
        // end, finds it.
        let cursor = Cursor {
            generation: "restored".to_owned(),
            next: fetch.cursor.next + 10_000,
        };
        let newer = server.revision(1, &[first], main_tip(&server.store), &[("a", "new")]);
        let fetch = client.sync().fetch(&[], &[], Some(&cursor)).unwrap();
        assert!(client.store.contains(newer.into()).unwrap());
        assert_ne!(fetch.cursor.generation, cursor.generation);
    }
}
