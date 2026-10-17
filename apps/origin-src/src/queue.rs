//! The landing queue: how a target's authority lands changes and other
//! targets on it, one acceptance at a time.
//!
//! Landing a change lands its stack: the head of each change below it that
//! isn't on the target yet, from the bottom, with an acceptance each. A
//! change that diverged, that its creator abandoned, or whose head has
//! unresolved conflicts (see [`conflict`](crate::conflict)) is refused. A
//! stack that waits is kept whole, so retrying it lands the rest once the
//! change it waits on can. For each revision:
//!
//! 1. **Rebase.** The changes it made to its base are merged onto the
//!    target's tip, file by file ([`Store::merge`]). A revision made on the
//!    tip lands as it is; any other is rebased into a new revision of its
//!    change, on the tip, by this node. A landing that would leave the
//!    target as it is lands nothing: the target already has the change.
//! 2. **Conflicts.** If both sides changed a path, a `core.conflict`
//!    statement about the revision lists the paths, and the change waits,
//!    blocked, until a new revision of it merges.
//! 3. **Policy.** The policy in force now must name this node as the
//!    target's authority. On `policy`, that's the policy of the acceptance
//!    before, never the one landing ([`Policy::governing`]). Each of the
//!    target's requirements needs a [claim](crate::annotations::Claim)
//!    about the revision that lands: results with the same input digest
//!    are promoted to it first, and a valid promotion claims what its
//!    original does, as its original's issuer.
//! 4. **Missing evidence** leaves the landing waiting, until the statements
//!    are issued and the landing is retried.
//! 5. **Otherwise** this node signs an acceptance naming what landed, the
//!    evidence and the policy, and appends it to the target's chain.
//!
//! Landing another target works the same way: its tip is merged from the
//! newest acceptances both targets' histories share, and the requirements
//! apply to the merged snapshot.
//!
//! The queue of each target lives in `queue/<target id>/`: a `lock` held
//! while landing, so landings on one target never race, and a file for each
//! landing that waits, `change-<id>` or `target-<target id>`, so waiting
//! landings can be listed and retried.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::slice;

use origin_platform::{DurableWrite, LockMode, LockedFile, Native, Platform};
use origin_shared::landing::{Blocked, Landing, Pending, Proposal};
use origin_shared::policy::Requirement;
use origin_shared::{
    AcceptanceBody, AcceptanceId, Base, ChangeId, EncodedObject, Head, Kind, NodeKey, Principal,
    PrincipalName, Record, RecordId, RepoPath, Revision, RevisionId, Source, StatementId, TargetId,
    TargetName, Tree, TreeId,
};

use crate::ancestry::Ancestry;
use crate::annotations::{AnnotateError, Annotations, Claim};
use crate::conflict::Conflicts;
use crate::oplog::Effect;
use crate::policy::{Policy, PolicyError};
use crate::store::{Store, StoreError};

/// The landing queues of a store's targets, as run by one node.
#[derive(Debug)]
pub struct Queue<'a, P = Native> {
    store: &'a Store<P>,
    key: &'a NodeKey,
    node: PrincipalName,
}

/// Landings onto one target, holding its queue's lock.
struct Session<'q, 'a, P> {
    queue: &'q Queue<'a, P>,
    onto: TargetName,
    policy: Policy,
    /// The acceptance of `policy` holding `policy`.
    policy_head: AcceptanceId,
    /// The target's latest acceptance, and its snapshot.
    tip: Head,
    state: TreeId,
    /// The statements, through an index caught up once the lock was taken.
    annotations: &'q mut Annotations<'a, P>,
    _lock: LockedFile,
}

/// What a session is about to land.
struct Candidate {
    /// What lands.
    proposal: Proposal,
    /// What the queue keeps if it waits: the landing of the stack it's in.
    kept_as: Proposal,
    source: Source,
    /// What the evidence must be about.
    subject: RecordId,
    /// The target's snapshot once it lands.
    state: TreeId,
    /// The trees the merge made, to write first.
    trees: Vec<EncodedObject>,
    /// The rebased revision, to write after the trees, if it is one.
    revision: Option<EncodedObject>,
}

impl<'a, P: Platform> Queue<'a, P> {
    /// The queues of `store`, run by the node `node`, which signs with
    /// `key`.
    pub fn new(store: &'a Store<P>, key: &'a NodeKey, node: PrincipalName) -> Self {
        Self { store, key, node }
    }

    /// This node, which issues the statements and revisions the queue
    /// makes.
    fn principal(&self) -> Principal {
        Principal::Node(self.node.clone())
    }

    /// Starts a repository's history: the `policy` target, holding `policy`
    /// as its `policy.toml`, and an empty `main`, both of which the policy
    /// must have this node order. Returns `main`'s first acceptance.
    ///
    /// # Errors
    ///
    /// Fails if the policy is invalid or doesn't make this node the
    /// authority of both targets, either target exists, or the records
    /// cannot be written.
    pub fn init(&self, policy: &str) -> Result<AcceptanceId, LandError> {
        let (file, tree) = Policy::snapshot(policy);
        let empty = Tree::default().encode();
        self.store.write_all(&[file, tree.clone(), empty.clone()])?;
        let policy = Self::check_policy(self.store, tree.id().into())?;
        for target in [TargetName::POLICY, TargetName::MAIN] {
            self.authorize(&policy, &target)?;
        }
        let genesis = |target: &TargetName, state: &EncodedObject, policy| {
            AcceptanceBody {
                target: target.id(),
                seq: 0,
                prev: None,
                state: state.id().into(),
                landed: Vec::new(),
                evidence: Vec::new(),
                policy,
            }
            .sign(self.key)
        };
        let policy = self.store.append(
            &TargetName::POLICY,
            &genesis(&TargetName::POLICY, &tree, None),
        )?;
        Ok(self.store.append(
            &TargetName::MAIN,
            &genesis(&TargetName::MAIN, &empty, Some(policy)),
        )?)
    }

    /// Lands `change`, and the changes below it that `onto` doesn't have
    /// yet, from the bottom of the stack. Stops at the first that waits,
    /// which the queue keeps as a landing of `change`: retrying it lands
    /// the rest of the stack once that one can.
    /// The statements are found through `annotations`, whose index is
    /// caught up first, so callers that keep one open don't read it again.
    ///
    /// # Errors
    ///
    /// Fails if this node isn't the target's authority, a change in the
    /// stack diverged, was abandoned or has unresolved conflicts, or the
    /// records cannot be read or written.
    pub fn land_change(
        &self,
        annotations: &mut Annotations<'a, P>,
        change: ChangeId,
        onto: &TargetName,
    ) -> Result<Vec<Landing>, LandError> {
        let mut session = self.session(annotations, onto)?;
        let stack = session.stack(change)?;
        if stack.is_empty() {
            session.forget(&Proposal::Change(change))?;
            return Ok(vec![Landing::Unchanged(Proposal::Change(change))]);
        }
        let proposal = Proposal::Change(change);
        let mut landings = Vec::new();
        for (id, revision) in stack {
            let landing = session.land_revision(&proposal, id, revision)?;
            let waits = matches!(landing, Landing::Pending(_));
            landings.push(landing);
            if waits {
                break;
            }
        }
        Ok(landings)
    }

    /// Lands the tip of `source` on `onto`, finding statements through
    /// `annotations` as [`Queue::land_change`] does.
    ///
    /// # Errors
    ///
    /// Fails if either target doesn't exist or they're the same, this node
    /// isn't the authority of `onto`, or the records cannot be read or
    /// written.
    pub fn land_target(
        &self,
        annotations: &mut Annotations<'a, P>,
        source: &TargetName,
        onto: &TargetName,
    ) -> Result<Landing, LandError> {
        if source == onto {
            return Err(LandError::SameTarget(onto.clone()));
        }
        let mut session = self.session(annotations, onto)?;
        let tip = self
            .store
            .head(source)?
            .ok_or_else(|| LandError::UnknownTarget(source.clone()))?;
        let state = self.store.get(tip.acceptance)?.body().state;
        let base = session.merge_base(tip.acceptance)?;
        let merge = self.store.merge(base, state, session.state)?;
        let proposal = Proposal::Target(source.clone());
        if !merge.conflicts.is_empty() {
            let subject = RecordId::Acceptance(tip.acceptance);
            return session.block(proposal, subject, &merge.conflicts);
        }
        if merge.tree == session.state {
            session.forget(&proposal)?;
            return Ok(Landing::Unchanged(proposal));
        }
        session.submit(Candidate {
            kept_as: proposal.clone(),
            proposal,
            source: Source::Target(tip.acceptance),
            subject: RecordId::Snapshot(merge.tree),
            state: merge.tree,
            trees: merge.trees,
            revision: None,
        })
    }

    /// Creates the target `name` at the tip of `from`, with a first
    /// acceptance landing it.
    ///
    /// # Errors
    ///
    /// Fails if `name` exists, `from` doesn't, this node wouldn't be the new
    /// target's authority, or the records cannot be read or written.
    pub fn create_target(&self, name: &TargetName, from: &TargetName) -> Result<Head, LandError> {
        let _lock = self.lock(name)?;
        if self.store.head(name)?.is_some() {
            return Err(LandError::TargetExists(name.clone()));
        }
        let (policy_head, policy) = Policy::in_force(self.store)?;
        self.authorize(&policy, name)?;
        let from = self
            .store
            .head(from)?
            .ok_or_else(|| LandError::UnknownTarget(from.clone()))?;
        let acceptance = AcceptanceBody {
            target: name.id(),
            seq: 0,
            prev: None,
            state: self.store.get(from.acceptance)?.body().state,
            landed: vec![Source::Target(from.acceptance)],
            evidence: Vec::new(),
            policy: Some(policy_head),
        }
        .sign(self.key);
        let acceptance = self.store.append(name, &acceptance)?;
        Ok(Head { seq: 0, acceptance })
    }

    /// Every landing that waits, on every target, sorted by target.
    ///
    /// # Errors
    ///
    /// Fails if the queues cannot be listed, or a landing cannot be read or
    /// is malformed.
    pub fn pending(store: &Store<P>) -> Result<Vec<Pending>, StoreError> {
        let root = store.queue_dir();
        let list = |dir: &PathBuf| match fs::read_dir(dir) {
            Ok(entries) => entries
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<io::Result<Vec<_>>>()
                .map_err(|source| StoreError::io(dir, source)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(source) => Err(StoreError::io(dir, source)),
        };
        let mut pending = Vec::new();
        for dir in list(&root)? {
            for path in list(&dir)? {
                let name = path.file_name().unwrap_or_default();
                if name == Self::LOCK || DurableWrite::<P>::is_temporary(name) {
                    continue;
                }
                let text =
                    fs::read_to_string(&path).map_err(|source| StoreError::io(&path, source))?;
                let mut landing = PendingFile::decode(&text)
                    .ok_or_else(|| StoreError::Malformed(path.clone()))?;
                if let Blocked::Conflict { statement, paths } = &mut landing.blocked {
                    *paths = Conflicts::listed(store, &store.get(*statement)?)?.paths();
                }
                pending.push(landing);
            }
        }
        pending.sort_by(|a, b| a.onto.cmp(&b.onto));
        Ok(pending)
    }

    /// The policy in `state`, about to become the policy in force, if it's
    /// a valid one that names an authority for `policy`, so the target
    /// never locks up.
    fn check_policy(store: &Store<P>, state: TreeId) -> Result<Policy, LandError> {
        let policy = Policy::of_snapshot(store, state)
            .and_then(|policy| policy.ok_or(PolicyError::Missing))
            .map_err(LandError::InvalidPolicy)?;
        if policy.authority(&TargetName::POLICY).is_none() {
            return Err(LandError::NoAuthority(TargetName::POLICY));
        }
        Ok(policy)
    }

    /// The name of the lock file in each target's queue.
    const LOCK: &str = "lock";

    /// The directory of `target`'s queue, created durably if it's missing.
    fn queue_dir(&self, target: TargetId) -> Result<PathBuf, StoreError> {
        let root = self.store.queue_dir();
        let dir = root.join(target.to_hex());
        if !dir.is_dir() {
            fs::create_dir_all(&dir).map_err(|source| StoreError::io(&dir, source))?;
            for parent in [&root, root.parent().unwrap_or(&root)] {
                self.store.sync_dir(parent)?;
            }
        }
        Ok(dir)
    }

    /// Locks the queue of `target`, waiting for the landing in progress.
    fn lock(&self, target: &TargetName) -> Result<LockedFile, StoreError> {
        let path = self.queue_dir(target.id())?.join(Self::LOCK);
        Ok(LockedFile::open(
            &self.store.platform,
            &path,
            LockMode::Exclusive,
        )?)
    }

    /// Checks that this node is the authority of `target` under `policy`,
    /// with the key it lists.
    fn authorize(&self, policy: &Policy, target: &TargetName) -> Result<(), LandError> {
        let authority = policy
            .authority(target)
            .ok_or_else(|| LandError::NoAuthority(target.clone()))?;
        if *authority != self.node {
            return Err(LandError::NotAuthority {
                target: target.clone(),
                authority: authority.clone(),
                node: self.node.clone(),
            });
        }
        if policy.node_key(authority) != Some(&self.key.public()) {
            return Err(LandError::WrongKey(self.node.clone()));
        }
        Ok(())
    }

    /// Starts landing on `onto`: locks its queue, then reads the policy in
    /// force and the target's tip, and catches `annotations` up.
    fn session<'q>(
        &'q self,
        annotations: &'q mut Annotations<'a, P>,
        onto: &TargetName,
    ) -> Result<Session<'q, 'a, P>, LandError> {
        let lock = self.lock(onto)?;
        annotations.refresh()?;
        let tip = self
            .store
            .head(onto)?
            .ok_or_else(|| LandError::UnknownTarget(onto.clone()))?;
        // A landing on `policy` is governed by the acceptance before it: its
        // tip, whatever the policy it lands says.
        let (policy_head, policy) = if *onto == TargetName::POLICY {
            let state = self.store.get(tip.acceptance)?.body().state;
            (tip.acceptance, Self::check_policy(self.store, state)?)
        } else {
            Policy::in_force(self.store)?
        };
        self.authorize(&policy, onto)?;
        Ok(Session {
            queue: self,
            onto: onto.clone(),
            policy,
            policy_head,
            state: self.store.get(tip.acceptance)?.body().state,
            tip,
            annotations,
            _lock: lock,
        })
    }
}

impl<P: Platform> Session<'_, '_, P> {
    fn store(&self) -> &Store<P> {
        self.queue.store
    }

    /// The heads to land for `change`, bottom first: the change's head, and
    /// below it, following each one's base, the heads of the changes the
    /// target doesn't have yet.
    fn stack(&self, change: ChangeId) -> Result<Vec<(RevisionId, Revision)>, LandError> {
        let index = self.annotations.index();
        let mut stack = Vec::new();
        let mut seen = HashSet::new();
        let mut next = Some(change);
        while let Some(change) = next.take() {
            if !seen.insert(change) {
                break;
            }
            let head = match index.heads(change)[..] {
                [head] => head,
                [] => return Err(LandError::UnknownChange(change)),
                ref heads => {
                    return Err(LandError::Divergent {
                        change,
                        heads: heads.to_vec(),
                    });
                }
            };
            let revision: Revision = self.store().get(head)?;
            let landing = self.store().landing(head, &revision)?;
            if landing.is_some_and(|landing| landing.body().target == self.onto.id()) {
                break;
            }
            let claims = self.annotations.claims(RecordId::Revision(head))?;
            for claim in &claims {
                let statement = claim.statement();
                if statement.kind == Kind::ABANDON && self.store().authorizes(statement)? {
                    return Err(LandError::Abandoned(change));
                }
            }
            let conflicts = Conflicts::of(self.store(), &claims, &revision)?;
            if !conflicts.is_empty() {
                return Err(LandError::Unresolved {
                    change,
                    paths: conflicts.paths(),
                });
            }
            if let Base::Revision(below) = revision.base {
                next = Some(self.store().get(below)?.change);
            }
            stack.push((head, revision));
        }
        stack.reverse();
        Ok(stack)
    }

    /// Rebases the revision `id` onto the tip and lands it, as part of the
    /// landing of `stack`, which is kept if it waits.
    fn land_revision(
        &mut self,
        stack: &Proposal,
        id: RevisionId,
        revision: Revision,
    ) -> Result<Landing, LandError> {
        let proposal = Proposal::Change(revision.change);
        let base = match revision.base {
            Base::Acceptance(id) => self.store().get(id)?.body().state,
            Base::Revision(id) => self.store().get(id)?.snapshot,
        };
        let merge = self
            .store()
            .merge(Some(base), revision.snapshot, self.state)?;
        if !merge.conflicts.is_empty() {
            return self.block(stack.clone(), RecordId::Revision(id), &merge.conflicts);
        }
        if merge.tree == self.state {
            self.forget(&proposal)?;
            return Ok(Landing::Unchanged(proposal));
        }
        let tip = Base::Acceptance(self.tip.acceptance);
        if revision.base == tip {
            return self.submit(Candidate {
                proposal,
                kept_as: stack.clone(),
                source: Source::Revision(id),
                subject: RecordId::Revision(id),
                state: merge.tree,
                trees: merge.trees,
                revision: None,
            });
        }
        // Held until the rebased revision is published, so no push makes
        // the change diverge meanwhile.
        let _publishing = self.publishing(revision.change, id)?;
        let rebased = Revision {
            predecessors: vec![id],
            base: tip,
            snapshot: merge.tree,
            author: self.queue.principal(),
            ..revision
        }
        .encode();
        let rebased_id = RevisionId::from(rebased.id());
        self.submit(Candidate {
            proposal,
            kept_as: stack.clone(),
            source: Source::Revision(rebased_id),
            subject: RecordId::Revision(rebased_id),
            state: merge.tree,
            trees: merge.trees,
            revision: Some(rebased),
        })
    }

    /// Takes the lock every publication of revisions takes (see
    /// [`Store::lock_pushes`]), once `head` is still the only head of
    /// `change`.
    fn publishing(&mut self, change: ChangeId, head: RevisionId) -> Result<LockedFile, LandError> {
        let lock = self.store().lock_pushes()?;
        self.annotations.refresh()?;
        if self.annotations.index().heads(change) != [head] {
            return Err(LandError::Moved(change));
        }
        Ok(lock)
    }

    /// Checks the policy for `candidate`, and lands it if its evidence is
    /// there, or keeps it waiting. Writes the records in layers, each
    /// durable before the next refers to it: the trees, then the rebased
    /// revision, then the promoted statements and the acceptance.
    fn submit(&mut self, candidate: Candidate) -> Result<Landing, LandError> {
        let store = self.queue.store;
        if !candidate.trees.is_empty() {
            store.write_all(&candidate.trees)?;
        }
        // What a landing on `policy` lands governs the next one, which
        // this node must still be the authority of.
        let landed_policy = if self.onto == TargetName::POLICY {
            self.queue.authorize(&self.policy, &self.onto)?;
            Some(Queue::check_policy(store, candidate.state)?)
        } else {
            None
        };
        let mut records: Vec<EncodedObject> = candidate.revision.into_iter().collect();
        let requirements = self.policy.requirements(&self.onto).to_vec();
        let mut evidence = Vec::new();
        let mut missing = Vec::new();
        if !requirements.is_empty() {
            // Promotion reads the revision.
            store.write_all(&records)?;
            records.clear();
            let promotions = self
                .annotations
                .promotions(candidate.subject, &self.queue.principal())?;
            let mut claims = self.annotations.claims(candidate.subject)?;
            for promotion in promotions {
                let encoded = promotion.encode();
                let claim = store.claim(encoded.id().into(), promotion)?;
                claims.push(claim.map_err(AnnotateError::from)?);
                records.push(encoded);
            }
            for requirement in requirements {
                match self.evidence(&claims, &requirement) {
                    Some(id) if !evidence.contains(&id) => evidence.push(id),
                    Some(_) => {}
                    None => missing.push(requirement),
                }
            }
        }
        if !missing.is_empty() {
            store.write_all(&records)?;
            let pending = Pending {
                onto: self.onto.clone(),
                proposal: candidate.kept_as,
                subject: candidate.subject,
                blocked: Blocked::Evidence(missing),
            };
            self.keep(&pending)?;
            return Ok(Landing::Pending(pending));
        }

        let acceptance = AcceptanceBody {
            target: self.onto.id(),
            seq: self.tip.seq + 1,
            prev: Some(self.tip.acceptance),
            state: candidate.state,
            landed: vec![candidate.source],
            evidence,
            policy: Some(self.policy_head),
        }
        .sign(self.queue.key);
        let encoded = acceptance.encode();
        if Policy::governing(encoded.id().into(), acceptance.body())? != self.policy_head {
            return Err(PolicyError::NotPredecessor(acceptance.body().seq).into());
        }
        let id = encoded.id().into();
        store.append_all(&self.onto, slice::from_ref(&acceptance), &records)?;
        self.tip = Head {
            seq: acceptance.body().seq,
            acceptance: id,
        };
        self.state = candidate.state;
        if let Some(policy) = landed_policy {
            self.policy = policy;
            self.policy_head = id;
        }
        self.forget(&candidate.proposal)?;
        Ok(Landing::Landed {
            proposal: candidate.proposal,
            source: candidate.source,
            acceptance: self.tip,
        })
    }

    /// The first of `claims` that meets `requirement`: of the kind and
    /// verdict required, from an issuer the policy admits, which for a
    /// promotion is its original's.
    fn evidence(&self, claims: &[Claim], requirement: &Requirement) -> Option<StatementId> {
        claims
            .iter()
            .find(|claim| {
                let claimed = claim.claimed();
                claimed.kind == requirement.kind
                    && claimed.verdict == Some(requirement.verdict)
                    && self.policy.admits(&requirement.issuer, &claimed.issuer)
            })
            .map(Claim::id)
    }

    /// Blocks a landing on `conflicts`: issues a `core.conflict` statement
    /// about `subject` listing them, unless this node's latest one says
    /// the same, and keeps the landing waiting.
    fn block(
        &mut self,
        proposal: Proposal,
        subject: RecordId,
        conflicts: &Conflicts,
    ) -> Result<Landing, LandError> {
        let issuer = self.queue.principal();
        let current = self
            .annotations
            .current(subject, &Kind::CONFLICT, &issuer)?;
        let (payload, conflict) = conflicts.statement(subject, issuer, current);
        let unchanged = match current {
            Some(id) => self.store().get(id)?.payload == conflict.payload,
            None => false,
        };
        let statement = match current {
            Some(id) if unchanged => id,
            _ => self.annotations.issue(&conflict, Some(payload))?,
        };
        let pending = Pending {
            onto: self.onto.clone(),
            proposal,
            subject,
            blocked: Blocked::Conflict {
                statement,
                paths: conflicts.paths(),
            },
        };
        self.keep(&pending)?;
        Ok(Landing::Pending(pending))
    }

    /// The file keeping a waiting landing of `proposal`.
    fn pending_path(&self, proposal: &Proposal) -> Result<PathBuf, StoreError> {
        let name = match proposal {
            Proposal::Change(change) => format!("change-{change}"),
            Proposal::Target(target) => format!("target-{}", target.id()),
        };
        Ok(self.queue.queue_dir(self.onto.id())?.join(name))
    }

    /// Durably keeps a waiting landing, replacing the last attempt's, and
    /// logs that it waits.
    fn keep(&self, pending: &Pending) -> Result<(), StoreError> {
        let write = self.store().durable_write();
        write.stage(
            self.pending_path(&pending.proposal)?,
            PendingFile::encode(pending).as_bytes(),
        )?;
        let waiting = Effect::Waiting {
            onto: pending.onto.clone(),
            proposal: pending.proposal.clone(),
        };
        self.store().commit_logged(write, &[waiting])?;
        Ok(())
    }

    /// Durably forgets a landing that waited, if one did.
    fn forget(&self, proposal: &Proposal) -> Result<(), StoreError> {
        let path = self.pending_path(proposal)?;
        match fs::remove_file(&path) {
            Ok(()) => self.store().sync_dir(path.parent().unwrap_or(&path)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::io(&path, source)),
        }
    }

    /// The snapshot to merge another target's tip, `tip`, from: the state
    /// of the newest acceptance both its history and the target's share,
    /// or the [merge](Ancestry::merge_base) of several, if each lacks
    /// another, as after targets landed each other. An acceptance's
    /// history is the one before it, and the acceptances of other targets
    /// it landed: a target's first lands the one it was created from, so
    /// targets created from the same one share it. `None` if they share
    /// none.
    ///
    /// The nearest shared acceptance can be an older one, through a target
    /// that forked early and landed recently, so both histories are read
    /// whole, each acceptance once.
    fn merge_base(&self, tip: AcceptanceId) -> Result<Option<TreeId>, StoreError> {
        let tips = [self.tip.acceptance, tip];
        let ancestry = Ancestry::walk(&tips, |id| {
            let acceptance = self.store().get(id)?;
            let body = acceptance.body();
            let landed = body.landed.iter().filter_map(|source| match *source {
                Source::Target(landed) => Some(landed),
                Source::Revision(_) => None,
            });
            Ok::<_, StoreError>((landed.chain(body.prev).collect(), body.state))
        })?;
        ancestry.merge_base(self.store(), &tips)
    }
}

/// The format of the files keeping waiting landings.
struct PendingFile;

impl PendingFile {
    /// The first line of every file keeping a waiting landing.
    const HEADER: &str = "origin pending 1";

    /// Encodes the landing as lines: the header, `onto <target>`, `change
    /// <id>` or `target <name>`, `subject <kind> <id>`, then a `missing
    /// <kind> <verdict> <issuer>` line for each requirement it lacks, or
    /// `conflict <statement>`, whose payload lists the paths.
    fn encode(pending: &Pending) -> String {
        let mut lines = vec![Self::HEADER.to_owned(), format!("onto {}", pending.onto)];
        lines.push(match &pending.proposal {
            Proposal::Change(change) => format!("change {change}"),
            Proposal::Target(target) => format!("target {target}"),
        });
        let (kind, id) = match pending.subject {
            RecordId::Snapshot(id) => ("snapshot", id.to_hex()),
            RecordId::Revision(id) => ("revision", id.to_hex()),
            RecordId::Statement(id) => ("statement", id.to_hex()),
            RecordId::Acceptance(id) => ("acceptance", id.to_hex()),
        };
        lines.push(format!("subject {kind} {id}"));
        match &pending.blocked {
            Blocked::Evidence(missing) => lines.extend(missing.iter().map(|requirement| {
                format!(
                    "missing {} {} {}",
                    requirement.kind, requirement.verdict, requirement.issuer
                )
            })),
            Blocked::Conflict { statement, .. } => lines.push(format!("conflict {statement}")),
        }
        lines.join("\n") + "\n"
    }

    /// Decodes what [`PendingFile::encode`] wrote, without the conflicting
    /// paths, or `None` if it's malformed.
    fn decode(text: &str) -> Option<Pending> {
        let mut lines = text.lines();
        if lines.next()? != Self::HEADER {
            return None;
        }
        let onto = lines.next()?.strip_prefix("onto ")?.parse().ok()?;
        let proposal = match lines.next()?.split_once(' ')? {
            ("change", id) => Proposal::Change(id.parse().ok()?),
            ("target", name) => Proposal::Target(name.parse().ok()?),
            _ => return None,
        };
        let subject = match lines.next()?.strip_prefix("subject ")?.split_once(' ')? {
            ("snapshot", id) => RecordId::Snapshot(id.parse().ok()?),
            ("revision", id) => RecordId::Revision(id.parse().ok()?),
            ("statement", id) => RecordId::Statement(id.parse().ok()?),
            ("acceptance", id) => RecordId::Acceptance(id.parse().ok()?),
            _ => return None,
        };
        let mut missing = Vec::new();
        let mut conflict = None;
        for line in lines {
            match line.split_once(' ')? {
                ("missing", requirement) => {
                    let mut fields = requirement.split(' ');
                    missing.push(Requirement {
                        kind: fields.next()?.parse().ok()?,
                        verdict: fields.next()?.parse().ok()?,
                        issuer: fields.next()?.parse().ok()?,
                    });
                }
                ("conflict", id) => conflict = Some(id.parse().ok()?),
                _ => return None,
            }
        }
        let blocked = match conflict {
            Some(statement) => Blocked::Conflict {
                statement,
                paths: Vec::new(),
            },
            None => Blocked::Evidence(missing),
        };
        Some(Pending {
            onto,
            proposal,
            subject,
            blocked,
        })
    }
}

/// A failure to land.
#[derive(Debug, thiserror::Error)]
pub enum LandError {
    /// No target has this name.
    #[error("no target is named `{0}`")]
    UnknownTarget(TargetName),
    /// `target new` was given a name already taken.
    #[error("a target is already named `{0}`")]
    TargetExists(TargetName),
    /// A target was landed on itself.
    #[error("cannot land `{0}` on itself")]
    SameTarget(TargetName),
    /// The policy names no authority for the target.
    #[error("policy.toml names no authority for `{0}`: give it, or `policy`, a [targets] table")]
    NoAuthority(TargetName),
    /// Another node orders the target.
    #[error(
        "node:{authority} is the authority of `{target}`, so only it lands there, and this is \
         node:{node}"
    )]
    NotAuthority {
        /// The target.
        target: TargetName,
        /// Its authority.
        authority: PrincipalName,
        /// This node.
        node: PrincipalName,
    },
    /// The policy lists another key for this node.
    #[error("policy.toml lists another key for node:{0} than this node's")]
    WrongKey(PrincipalName),
    /// No revision of the change is known.
    #[error("change {} has no revisions", .0.short())]
    UnknownChange(ChangeId),
    /// The change has several heads.
    #[error(
        "change {} is divergent, so it cannot land: its heads are {}. Run `origin converge {}` \
         first",
        change.short(),
        heads.iter().map(RevisionId::short).collect::<Vec<_>>().join(", "),
        change.short()
    )]
    Divergent {
        /// The change.
        change: ChangeId,
        /// Its heads.
        heads: Vec<RevisionId>,
    },
    /// A revision of the change was published while it was rebased.
    #[error("change {} moved while it was landing: land it again", .0.short())]
    Moved(ChangeId),
    /// The change was given up.
    #[error("change {} was abandoned, so it cannot land", .0.short())]
    Abandoned(ChangeId),
    /// The change's head has unresolved conflicts.
    #[error(
        "change {} has unresolved conflicts, so it cannot land: edit {}, or accept them as they \
         are with `origin resolve`",
        change.short(),
        paths.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
    )]
    Unresolved {
        /// The change.
        change: ChangeId,
        /// The unresolved paths.
        paths: Vec<RepoPath>,
    },
    /// Landing on `policy` would leave it without a valid policy.
    #[error("landing would leave `policy` without a valid policy.toml")]
    InvalidPolicy(#[source] PolicyError),
    /// The policy in force cannot be read.
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// A conflict cannot be recorded.
    #[error(transparent)]
    Annotate(#[from] AnnotateError),
    /// Records cannot be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;
    use std::thread;

    use origin_shared::{BlobId, Digest, Inputs, Statement, Verdict};

    use super::*;
    use crate::conflict::Conflicted;
    use crate::tree::tests::build;

    /// A repository whose `policy` and `main` the node `queue` orders, with
    /// `extra` added to its `policy.toml`.
    pub(crate) struct Repo {
        _dir: tempfile::TempDir,
        pub(crate) store: Store,
        key: NodeKey,
    }

    impl Repo {
        pub(crate) fn new(extra: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::new(dir.path().join(".origin"), Native::default());
            store.init().unwrap();
            let key = NodeKey::from_bytes(&[7; 32]);
            let repo = Self {
                _dir: dir,
                store,
                key,
            };
            repo.queue().init(&repo.policy_toml(extra)).unwrap();
            repo
        }

        /// A `policy.toml` naming this node the authority of `policy`, then
        /// `extra`.
        fn policy_toml(&self, extra: &str) -> String {
            format!(
                "[nodes]\nqueue = \"{}\"\n[targets.policy]\nauthority = \"node:queue\"\n{extra}",
                self.key.public()
            )
        }

        /// Writes a snapshot of `files`, contents included.
        pub(crate) fn files(&self, files: &[(&str, &str)]) -> TreeId {
            let blobs: Vec<_> = files
                .iter()
                .map(|(_, content)| EncodedObject::blob(content.as_bytes()))
                .collect();
            self.store.write_all(&blobs).unwrap();
            build(&self.store, files)
        }

        pub(crate) fn queue(&self) -> Queue<'_> {
            Queue::new(&self.store, &self.key, PrincipalName::new("queue").unwrap())
        }

        pub(crate) fn tip(&self, target: &TargetName) -> Head {
            self.store.head(target).unwrap().unwrap()
        }

        fn state(&self, target: &TargetName) -> TreeId {
            self.store
                .get(self.tip(target).acceptance)
                .unwrap()
                .body()
                .state
        }

        /// Writes a revision of `change` with `files`, made on `base`.
        pub(crate) fn revision(
            &self,
            change: u8,
            predecessors: &[RevisionId],
            base: Base,
            files: &[(&str, &str)],
        ) -> RevisionId {
            let encoded = Revision {
                change: ChangeId::from_bytes([change; 16]),
                predecessors: predecessors.to_vec(),
                base,
                deps: Vec::new(),
                snapshot: self.files(files),
                description: BlobId::from(EncodedObject::blob(b"").id()),
                author: "user:ada".parse().unwrap(),
            }
            .encode();
            self.store
                .write_all(std::slice::from_ref(&encoded))
                .unwrap();
            encoded.id().into()
        }

        /// A revision of a new change `change` on the tip of `main`, adding
        /// `files` to it.
        pub(crate) fn change_on_main(&self, change: u8, files: &[(&str, &str)]) -> RevisionId {
            let base = Base::Acceptance(self.tip(&TargetName::MAIN).acceptance);
            let mut all = self.main_files();
            all.extend(
                files
                    .iter()
                    .map(|&(path, content)| (path.to_owned(), content.to_owned())),
            );
            let all: Vec<_> = all.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
            self.revision(change, &[], base, &all)
        }

        /// The files of `main`'s tip, with their contents.
        fn main_files(&self) -> Vec<(String, String)> {
            let empty = build(&self.store, &[]);
            self.store
                .diff(empty, self.state(&TargetName::MAIN))
                .unwrap()
                .into_iter()
                .map(|change| {
                    let content = self
                        .store
                        .try_blob(change.new.unwrap().id)
                        .unwrap()
                        .unwrap();
                    (
                        change.path.as_str().to_owned(),
                        String::from_utf8(content).unwrap(),
                    )
                })
                .collect()
        }

        pub(crate) fn land(&self, change: u8) -> Vec<Landing> {
            self.queue()
                .land_change(
                    &mut self.annotations(),
                    ChangeId::from_bytes([change; 16]),
                    &TargetName::MAIN,
                )
                .unwrap()
        }

        pub(crate) fn annotations(&self) -> Annotations<'_> {
            Annotations::open(&self.store).unwrap()
        }
    }

    fn change(id: u8) -> Proposal {
        Proposal::Change(ChangeId::from_bytes([id; 16]))
    }

    /// The revision a landing landed.
    fn landed(landing: &Landing) -> RevisionId {
        match landing {
            Landing::Landed {
                source: Source::Revision(revision),
                ..
            } => *revision,
            other => panic!("expected a landed revision, got {other:?}"),
        }
    }

    #[test]
    fn lands_rebases_and_skips_what_the_target_has() {
        let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        let a = repo.change_on_main(1, &[("a", "a")]);
        let b = repo.change_on_main(2, &[("b", "b")]);

        let [landing] = &repo.land(1)[..] else {
            panic!()
        };
        assert_eq!(landed(landing), a, "made on the tip, it lands as it is");
        let acceptance = repo
            .store
            .get(repo.tip(&TargetName::MAIN).acceptance)
            .unwrap();
        acceptance.verify(&repo.key.public()).unwrap();
        assert_eq!(
            acceptance.body().policy,
            Some(repo.tip(&TargetName::POLICY).acceptance)
        );

        let [landing] = &repo.land(2)[..] else {
            panic!()
        };
        let rebased = landed(landing);
        assert_ne!(rebased, b, "made on an older tip, it is rebased");
        let revision = repo.store.get(rebased).unwrap();
        assert_eq!(revision.predecessors, [b]);
        assert_eq!(revision.author, "node:queue".parse().unwrap());
        let log = repo.store.log(&TargetName::MAIN).unwrap();
        assert_eq!(
            revision.base,
            Base::Acceptance(log[1]),
            "on the tip it landed after"
        );
        assert_eq!(
            repo.state(&TargetName::MAIN),
            repo.files(&[("a", "a"), ("b", "b")])
        );

        // Both changes are on `main` now: neither lands again, though `b`'s
        // head is the rebased revision and `a`'s the original.
        assert_eq!(repo.land(1), [Landing::Unchanged(change(1))]);
        assert_eq!(repo.land(2), [Landing::Unchanged(change(2))]);
        assert_eq!(repo.tip(&TargetName::MAIN).seq, 2);
    }

    #[test]
    fn stacks_land_from_the_bottom() {
        let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        let below = repo.change_on_main(1, &[("a", "a")]);
        let above = repo.revision(2, &[], Base::Revision(below), &[("a", "a"), ("b", "b")]);
        let landings = repo.land(2);
        assert_eq!(landings.len(), 2, "{landings:?}");
        assert_eq!(landed(&landings[0]), below);
        assert_ne!(landed(&landings[1]), above, "rebased onto the first");
        assert_eq!(
            repo.state(&TargetName::MAIN),
            repo.files(&[("a", "a"), ("b", "b")])
        );
    }

    #[test]
    fn conflicts_block_until_a_revision_merges() {
        let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        repo.change_on_main(1, &[("shared", "one")]);
        let two = repo.change_on_main(2, &[("shared", "two")]);
        repo.land(1);

        let blocked = repo.land(2);
        let [Landing::Pending(pending)] = &blocked[..] else {
            panic!("{blocked:?}")
        };
        assert_eq!(pending.subject, RecordId::Revision(two));
        let Blocked::Conflict { statement, paths } = &pending.blocked else {
            panic!("{pending:?}")
        };
        assert_eq!(paths, &[RepoPath::new("shared").unwrap()]);
        let conflict = repo.store.get(*statement).unwrap();
        assert_eq!(conflict.kind, Kind::CONFLICT);
        assert_eq!(conflict.subject, RecordId::Revision(two));
        assert_eq!(repo.land(2), blocked, "retrying issues the same statement");
        assert_eq!(
            Queue::pending(&repo.store).unwrap(),
            std::slice::from_ref(pending)
        );

        // A new revision that takes `main`'s version merges.
        let tip = Base::Acceptance(repo.tip(&TargetName::MAIN).acceptance);
        repo.revision(2, &[two], tip, &[("shared", "one"), ("more", "m")]);
        assert!(matches!(repo.land(2)[..], [Landing::Landed { .. }]));
        assert_eq!(Queue::pending(&repo.store).unwrap(), []);
    }

    /// A `ci.test` result about `subject` from `issuer`, reading `src`.
    pub(crate) fn ci_test(subject: RevisionId, issuer: &str) -> Statement {
        Statement {
            subject: RecordId::Revision(subject),
            kind: Kind::new("ci.test").unwrap(),
            verdict: Some(Verdict::Pass),
            issuer: issuer.parse().unwrap(),
            inputs: Some(Inputs {
                paths: vec![RepoPath::new("src").unwrap()],
                definition: Digest::of(b"cargo test"),
                toolchain: Digest::of(b"rustc"),
                external: false,
            }),
            payload: None,
            supersedes: None,
            promoted_from: None,
        }
    }

    #[test]
    fn missing_evidence_waits_and_promoted_results_count() {
        let repo = Repo::new(
            "[groups]\nrunners = [\"node:ci\"]\n[targets.main]\nauthority = \"node:queue\"\n\
             require = [{ kind = \"ci.test\", verdict = \"pass\", issuer = \"group:runners\" }]\n",
        );
        let code = repo.change_on_main(1, &[("src/lib.rs", "fn a() {}")]);
        let docs = repo.change_on_main(2, &[("docs/guide.md", "# Guide")]);
        let waiting = repo.land(1);
        let [Landing::Pending(pending)] = &waiting[..] else {
            panic!("{waiting:?}")
        };
        assert!(matches!(&pending.blocked, Blocked::Evidence(missing) if missing.len() == 1));
        assert_eq!(
            Queue::pending(&repo.store).unwrap(),
            std::slice::from_ref(pending)
        );

        // A user outside the group doesn't count; the runner does.
        let annotations = Annotations::open(&repo.store).unwrap();
        annotations
            .issue(&ci_test(code, "user:mallory"), None)
            .unwrap();
        assert_eq!(repo.land(1), waiting);
        let result = annotations.issue(&ci_test(code, "node:ci"), None).unwrap();
        let [landing] = &repo.land(1)[..] else {
            panic!()
        };
        let acceptance = repo
            .store
            .get(repo.tip(&TargetName::MAIN).acceptance)
            .unwrap();
        assert_eq!(landed(landing), code);
        assert_eq!(acceptance.body().evidence, [result]);
        assert_eq!(Queue::pending(&repo.store).unwrap(), []);

        // The docs change is rebased onto `src` as tested, so the result is
        // promoted to it, and counts as the runner's.
        let [landing] = &repo.land(2)[..] else {
            panic!()
        };
        let rebased = landed(landing);
        assert_ne!(rebased, docs);
        let acceptance = repo
            .store
            .get(repo.tip(&TargetName::MAIN).acceptance)
            .unwrap();
        let [evidence] = acceptance.body().evidence[..] else {
            panic!()
        };
        let promoted = repo.store.get(evidence).unwrap();
        assert_eq!(promoted.subject, RecordId::Revision(rebased));
        assert_eq!(promoted.promoted_from, Some(result));
        assert_eq!(promoted.issuer, "node:queue".parse().unwrap());
    }

    #[test]
    fn refuses_divergent_and_abandoned_changes() {
        let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        let first = repo.change_on_main(1, &[("a", "a")]);
        let base = Base::Acceptance(repo.tip(&TargetName::MAIN).acceptance);
        repo.revision(1, &[first], base, &[("a", "left")]);
        repo.revision(1, &[first], base, &[("a", "right")]);
        let main = TargetName::MAIN;
        let error = repo
            .queue()
            .land_change(
                &mut repo.annotations(),
                ChangeId::from_bytes([1; 16]),
                &main,
            )
            .unwrap_err();
        assert!(matches!(error, LandError::Divergent { .. }), "{error}");

        let given_up = repo.change_on_main(2, &[("b", "b")]);
        let abandon = Statement {
            subject: RecordId::Revision(given_up),
            kind: Kind::ABANDON,
            verdict: None,
            issuer: "user:ada".parse().unwrap(),
            inputs: None,
            payload: None,
            supersedes: None,
            promoted_from: None,
        };
        repo.store.write_all(&[abandon.encode()]).unwrap();
        let error = repo
            .queue()
            .land_change(
                &mut repo.annotations(),
                ChangeId::from_bytes([2; 16]),
                &main,
            )
            .unwrap_err();
        assert!(matches!(error, LandError::Abandoned(_)), "{error}");
    }

    #[test]
    fn only_the_authority_lands() {
        let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        repo.change_on_main(1, &[("a", "a")]);
        let change = ChangeId::from_bytes([1; 16]);
        let other = Queue::new(&repo.store, &repo.key, PrincipalName::new("other").unwrap());
        let error = other
            .land_change(&mut repo.annotations(), change, &TargetName::MAIN)
            .unwrap_err();
        assert!(matches!(error, LandError::NotAuthority { .. }), "{error}");

        let impostor = NodeKey::from_bytes(&[8; 32]);
        let queue = Queue::new(&repo.store, &impostor, PrincipalName::new("queue").unwrap());
        let error = queue
            .land_change(&mut repo.annotations(), change, &TargetName::MAIN)
            .unwrap_err();
        assert!(matches!(error, LandError::WrongKey(_)), "{error}");
        assert_eq!(repo.tip(&TargetName::MAIN).seq, 0);
    }

    #[test]
    fn concurrent_landings_serialize() {
        let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        let (proposers, each) = (6_u8, 3_u8);
        let changes: Vec<u8> = (0..proposers * each).map(|change| change + 1).collect();
        for &change in &changes {
            repo.change_on_main(change, &[(&format!("file-{change}"), "x")]);
        }
        let landed = Mutex::new(0);
        thread::scope(|scope| {
            for chunk in changes.chunks(usize::from(each)) {
                let (repo, landed) = (&repo, &landed);
                scope.spawn(move || {
                    for &change in chunk {
                        assert!(matches!(repo.land(change)[..], [Landing::Landed { .. }]));
                        *landed.lock().unwrap() += 1;
                    }
                });
            }
        });
        assert_eq!(*landed.lock().unwrap(), changes.len());
        let log = repo.store.log(&TargetName::MAIN).unwrap();
        assert_eq!(log.len(), changes.len() + 1);
        for pair in log.windows(2) {
            assert_eq!(repo.store.get(pair[1]).unwrap().body().prev, Some(pair[0]));
        }
        assert_eq!(repo.main_files().len(), changes.len(), "every file landed");
    }

    #[test]
    fn targets_land_on_each_other() {
        let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        let (main, overhaul) = (TargetName::MAIN, TargetName::new("overhaul").unwrap());
        repo.change_on_main(1, &[("shared", "v1")]);
        repo.land(1);
        let queue = repo.queue();
        queue.create_target(&overhaul, &main).unwrap();
        assert!(matches!(
            queue.create_target(&overhaul, &main),
            Err(LandError::TargetExists(_))
        ));
        assert_eq!(repo.state(&overhaul), repo.state(&main));

        // Work lands on the overhaul while `main` moves on elsewhere.
        let on_overhaul = Base::Acceptance(repo.tip(&overhaul).acceptance);
        repo.revision(2, &[], on_overhaul, &[("shared", "v2"), ("new", "n")]);
        queue
            .land_change(
                &mut repo.annotations(),
                ChangeId::from_bytes([2; 16]),
                &overhaul,
            )
            .unwrap();
        repo.change_on_main(3, &[("other", "o")]);
        repo.land(3);

        // Catching up merges `main`'s change since the fork; finishing
        // lands the overhaul on `main`.
        assert!(matches!(
            queue
                .land_target(&mut repo.annotations(), &main, &overhaul)
                .unwrap(),
            Landing::Landed { .. }
        ));
        let landing = queue
            .land_target(&mut repo.annotations(), &overhaul, &main)
            .unwrap();
        assert!(matches!(
            landing,
            Landing::Landed {
                source: Source::Target(_),
                ..
            }
        ));
        let expected = repo.files(&[("shared", "v2"), ("new", "n"), ("other", "o")]);
        assert_eq!(repo.state(&main), expected);
        assert_eq!(repo.state(&overhaul), expected);
        assert_eq!(
            queue
                .land_target(&mut repo.annotations(), &overhaul, &main)
                .unwrap(),
            Landing::Unchanged(Proposal::Target(overhaul.clone()))
        );
        assert!(matches!(
            queue.land_target(&mut repo.annotations(), &main, &main),
            Err(LandError::SameTarget(_))
        ));
    }

    #[test]
    fn policy_landings_keep_a_valid_policy() {
        let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        let policy = TargetName::POLICY;
        let tip = Base::Acceptance(repo.tip(&policy).acceptance);
        for (change, toml) in [(1, "not = [valid"), (2, "[nodes]\n")] {
            repo.revision(change, &[], tip, &[("policy.toml", toml)]);
            let result = repo.queue().land_change(
                &mut repo.annotations(),
                ChangeId::from_bytes([change; 16]),
                &policy,
            );
            assert!(
                matches!(
                    result,
                    Err(LandError::InvalidPolicy(_) | LandError::NoAuthority(_))
                ),
                "{result:?}"
            );
        }
        // A valid edit lands, and later landings are under it.
        let requiring = repo.policy_toml(
            "[targets.main]\nauthority = \"node:queue\"\n\
             require = [{ kind = \"review\", verdict = \"pass\", issuer = \"user:bob\" }]\n",
        );
        let encoded = Revision {
            change: ChangeId::from_bytes([3; 16]),
            predecessors: Vec::new(),
            base: tip,
            deps: Vec::new(),
            snapshot: repo.files(&[("policy.toml", &requiring)]),
            description: BlobId::from(EncodedObject::blob(b"").id()),
            author: "user:ada".parse().unwrap(),
        }
        .encode();
        repo.store
            .write_all(std::slice::from_ref(&encoded))
            .unwrap();
        let landings = repo
            .queue()
            .land_change(
                &mut repo.annotations(),
                ChangeId::from_bytes([3; 16]),
                &policy,
            )
            .unwrap();
        assert!(
            matches!(landings[..], [Landing::Landed { .. }]),
            "{landings:?}"
        );
        repo.change_on_main(4, &[("a", "a")]);
        assert!(matches!(repo.land(4)[..], [Landing::Pending(_)]));
    }

    #[test]
    fn pending_landings_round_trip() {
        let pending = Pending {
            onto: TargetName::new("release/v3").unwrap(),
            proposal: Proposal::Target(TargetName::MAIN),
            subject: RecordId::Snapshot(Tree::default().encode().id().into()),
            blocked: Blocked::Evidence(vec![Requirement {
                kind: Kind::new("ci.test").unwrap(),
                verdict: Verdict::Pass,
                issuer: "group:runners".parse().unwrap(),
            }]),
        };
        assert_eq!(
            PendingFile::decode(&PendingFile::encode(&pending)),
            Some(pending.clone())
        );
        let conflict = Pending {
            proposal: change(9),
            blocked: Blocked::Conflict {
                statement: EncodedObject::blob(b"conflict").id().into(),
                paths: Vec::new(),
            },
            ..pending
        };
        assert_eq!(
            PendingFile::decode(&PendingFile::encode(&conflict)),
            Some(conflict)
        );
        assert_eq!(PendingFile::decode("origin pending 2\n"), None);
    }

    /// A repository whose `main` requires a passing `ci.test` from
    /// `user:ci`.
    fn requiring_ci() -> Repo {
        Repo::new(
            "[targets.main]\nauthority = \"node:queue\"\n\
             require = [{ kind = \"ci.test\", verdict = \"pass\", issuer = \"user:ci\" }]\n",
        )
    }

    #[test]
    fn forged_promotions_are_no_evidence() {
        let repo = requiring_ci();
        let failed = repo.change_on_main(1, &[("src/lib.rs", "bad")]);
        let mut original = ci_test(failed, "user:ci");
        original.verdict = Some(Verdict::Fail);
        let id = Annotations::open(&repo.store)
            .unwrap()
            .issue(&original, None)
            .unwrap();
        // Eve wraps the trusted failure as a pass, written as a fetch would.
        let forged = Statement {
            verdict: Some(Verdict::Pass),
            issuer: "user:eve".parse().unwrap(),
            promoted_from: Some(id),
            ..original.clone()
        };
        // And carries a result that read the network to another change.
        let old = repo.change_on_main(3, &[("src/lib.rs", "bad")]);
        let docs = repo.change_on_main(2, &[("src/lib.rs", "bad"), ("docs", "d")]);
        let mut external = ci_test(old, "user:ci");
        external.inputs.as_mut().unwrap().external = true;
        let external_id: StatementId = external.encode().id().into();
        let reused = Statement {
            subject: RecordId::Revision(docs),
            issuer: "user:eve".parse().unwrap(),
            promoted_from: Some(external_id),
            ..external.clone()
        };
        repo.store.write_all(&[external.encode()]).unwrap();
        repo.store
            .write_all(&[forged.encode(), reused.encode()])
            .unwrap();
        assert!(matches!(repo.land(1)[..], [Landing::Pending(_)]));
        assert!(matches!(repo.land(2)[..], [Landing::Pending(_)]));
    }

    #[test]
    fn replaced_results_are_promoted_again() {
        let repo = requiring_ci();
        let old = repo.change_on_main(1, &[("src/lib.rs", "code")]);
        let new = repo.change_on_main(2, &[("src/lib.rs", "code"), ("docs", "new")]);
        let original = ci_test(old, "user:ci");
        let annotations = Annotations::open(&repo.store).unwrap();
        let id = annotations.issue(&original, None).unwrap();
        Annotations::open(&repo.store)
            .unwrap()
            .promote(RecordId::Revision(new), &"node:queue".parse().unwrap())
            .unwrap();
        // The same passing result runs again, replacing the one promoted.
        let rerun = Statement {
            supersedes: Some(id),
            ..original
        };
        annotations.issue(&rerun, None).unwrap();
        let landings = repo.land(2);
        assert!(
            matches!(landings[..], [Landing::Landed { .. }]),
            "{landings:?}"
        );
    }

    #[test]
    fn only_the_creator_of_a_change_abandons_it() {
        let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        let revision = repo.change_on_main(1, &[("file", "work")]);
        let abandon = |issuer: &str| Statement {
            subject: RecordId::Revision(revision),
            kind: Kind::ABANDON,
            verdict: None,
            issuer: issuer.parse().unwrap(),
            inputs: None,
            payload: None,
            supersedes: None,
            promoted_from: None,
        };
        repo.store
            .write_all(&[abandon("user:eve").encode()])
            .unwrap();
        assert!(matches!(repo.land(1)[..], [Landing::Landed { .. }]));
    }

    #[test]
    fn unresolved_conflicts_never_land() {
        let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        let marked = "<<<<<<< a\n=======\n>>>>>>> b\n";
        let revision = repo.change_on_main(1, &[("file", marked)]);
        let path = RepoPath::new("file").unwrap();
        let conflicts = |content: &str| {
            Conflicts::new(vec![Conflicted {
                path: path.clone(),
                written: Some(EncodedObject::blob(content.as_bytes()).id()),
            }])
        };
        // From someone else, it says nothing about the revision's files.
        let foreign = conflicts(marked).statement(
            RecordId::Revision(revision),
            "user:eve".parse().unwrap(),
            None,
        );
        // From its author, it does.
        let (payload, statement) = conflicts(marked).statement(
            RecordId::Revision(revision),
            "user:ada".parse().unwrap(),
            None,
        );
        repo.store
            .write_all(&[foreign.0, foreign.1.encode()])
            .unwrap();
        let mut annotations = Annotations::open(&repo.store).unwrap();
        assert_eq!(
            annotations
                .claims(RecordId::Revision(revision))
                .unwrap()
                .len(),
            1
        );
        annotations.issue(&statement, Some(payload)).unwrap();
        annotations = Annotations::open(&repo.store).unwrap();
        assert_eq!(
            annotations
                .claims(RecordId::Revision(revision))
                .unwrap()
                .len(),
            2
        );
        let error = repo
            .queue()
            .land_change(
                &mut repo.annotations(),
                ChangeId::from_bytes([1; 16]),
                &TargetName::MAIN,
            )
            .unwrap_err();
        assert!(
            matches!(&error, LandError::Unresolved { paths, .. } if *paths == [path.clone()]),
            "{error}"
        );

        // A revision whose file is no longer what the conflict wrote there
        // is resolved, whatever the file holds.
        let edited = repo.change_on_main(2, &[("file", "Heading\n=======\n")]);
        let (payload, statement) = conflicts(marked).statement(
            RecordId::Revision(edited),
            "user:ada".parse().unwrap(),
            None,
        );
        annotations.issue(&statement, Some(payload)).unwrap();
        assert!(matches!(repo.land(2)[..], [Landing::Landed { .. }]));
    }

    #[test]
    fn sibling_targets_merge_from_where_they_forked() {
        let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        repo.change_on_main(1, &[("keep", "k"), ("delete", "d")]);
        repo.land(1);
        let (a, b) = (TargetName::new("a").unwrap(), TargetName::new("b").unwrap());
        let queue = repo.queue();
        queue.create_target(&a, &TargetName::MAIN).unwrap();
        queue.create_target(&b, &TargetName::MAIN).unwrap();
        // `a` deletes a file, `b` adds one: neither ever landed the other.
        let on = |target| Base::Acceptance(repo.tip(target).acceptance);
        repo.revision(2, &[], on(&a), &[("keep", "k")]);
        queue
            .land_change(&mut repo.annotations(), ChangeId::from_bytes([2; 16]), &a)
            .unwrap();
        repo.revision(
            3,
            &[],
            on(&b),
            &[("keep", "k"), ("delete", "d"), ("other", "o")],
        );
        queue
            .land_change(&mut repo.annotations(), ChangeId::from_bytes([3; 16]), &b)
            .unwrap();
        assert!(matches!(
            queue.land_target(&mut repo.annotations(), &a, &b).unwrap(),
            Landing::Landed { .. }
        ));
        assert_eq!(repo.state(&b), repo.files(&[("keep", "k"), ("other", "o")]));
    }

    #[test]
    fn targets_merge_from_their_newest_shared_acceptance() {
        let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        let queue = repo.queue();
        let [a, b, c, d] = ["a", "b", "c", "d"].map(|name| TargetName::new(name).unwrap());
        let land = |change: u8, target: &TargetName, files: &[(&str, &str)]| {
            let on = Base::Acceptance(repo.tip(target).acceptance);
            repo.revision(change, &[], on, files);
            queue
                .land_change(
                    &mut repo.annotations(),
                    ChangeId::from_bytes([change; 16]),
                    target,
                )
                .unwrap();
        };
        // `c` and `d` fork from an empty `main`, `a` and `b` from the next
        // `main`, which adds a file.
        queue.create_target(&c, &TargetName::MAIN).unwrap();
        queue.create_target(&d, &TargetName::MAIN).unwrap();
        repo.change_on_main(1, &[("delete", "d")]);
        repo.land(1);
        queue.create_target(&a, &TargetName::MAIN).unwrap();
        queue.create_target(&b, &TargetName::MAIN).unwrap();
        // `a` deletes it, `b` keeps it, and both move on.
        for n in 0..4 {
            let counter = n.to_string();
            land(10 + n, &a, &[("a", &counter)]);
            land(20 + n, &b, &[("delete", "d"), ("b", &counter)]);
        }
        // Then each lands a side line: the empty `main` is nearer.
        land(30, &c, &[("c", "c")]);
        land(31, &d, &[("d", "d")]);
        assert!(matches!(
            queue.land_target(&mut repo.annotations(), &c, &a).unwrap(),
            Landing::Landed { .. }
        ));
        assert!(matches!(
            queue.land_target(&mut repo.annotations(), &d, &b).unwrap(),
            Landing::Landed { .. }
        ));

        assert!(matches!(
            queue.land_target(&mut repo.annotations(), &a, &b).unwrap(),
            Landing::Landed { .. }
        ));
        assert_eq!(
            repo.state(&b),
            repo.files(&[("a", "3"), ("b", "3"), ("c", "c"), ("d", "d")]),
            "the deletion stands"
        );
    }

    #[test]
    fn criss_crossed_targets_merge_what_their_bases_agree_on() {
        let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        let queue = repo.queue();
        let [a, b] = ["a", "b"].map(|name| TargetName::new(name).unwrap());
        let land = |change: u8, target: &TargetName, files: &[(&str, &str)]| {
            let on = Base::Acceptance(repo.tip(target).acceptance);
            repo.revision(change, &[], on, files);
            queue
                .land_change(
                    &mut repo.annotations(),
                    ChangeId::from_bytes([change; 16]),
                    target,
                )
                .unwrap();
        };
        queue.create_target(&a, &TargetName::MAIN).unwrap();
        queue.create_target(&b, &TargetName::MAIN).unwrap();
        land(1, &a, &[("a", "0"), ("shared", "a")]);
        land(2, &b, &[("b", "0"), ("shared", "b")]);
        // Each lands the other's tip at once, resolving `shared` its own
        // way: two merge bases, which disagree on it.
        let tips = [repo.tip(&a), repo.tip(&b)];
        let policy = Some(repo.tip(&TargetName::POLICY).acceptance);
        for (target, prev, other, shared) in
            [(&a, tips[0], tips[1], "a"), (&b, tips[1], tips[0], "b")]
        {
            let merged = AcceptanceBody {
                target: target.id(),
                seq: prev.seq + 1,
                prev: Some(prev.acceptance),
                state: repo.files(&[("a", "0"), ("b", "0"), ("shared", shared)]),
                landed: vec![Source::Target(other.acceptance)],
                evidence: vec![],
                policy,
            }
            .sign(&repo.key);
            repo.store.append(target, &merged).unwrap();
        }
        // Then each changes another file.
        land(3, &a, &[("a", "1"), ("b", "0"), ("shared", "a")]);
        land(4, &b, &[("a", "0"), ("b", "1"), ("shared", "b")]);

        let Landing::Pending(pending) = queue.land_target(&mut repo.annotations(), &a, &b).unwrap()
        else {
            panic!("the bases disagree on `shared`, so it can't be merged blindly");
        };
        assert!(
            matches!(&pending.blocked, Blocked::Conflict { paths, .. } if paths.iter().map(RepoPath::as_str).eq(["shared"])),
            "{pending:?}"
        );
        // Once `a` takes `b`'s side, the disjoint edits merge.
        land(5, &a, &[("a", "1"), ("b", "0"), ("shared", "b")]);
        assert!(matches!(
            queue.land_target(&mut repo.annotations(), &a, &b).unwrap(),
            Landing::Landed { .. }
        ));
        assert_eq!(
            repo.state(&b),
            repo.files(&[("a", "1"), ("b", "1"), ("shared", "b")])
        );
    }

    #[test]
    fn bases_that_disagree_on_a_kind_conflict_on_any_change_under_it() {
        let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        let queue = repo.queue();
        let land = |change: u8, target: &TargetName, files: &[(&str, &str)]| {
            let on = Base::Acceptance(repo.tip(target).acceptance);
            repo.revision(change, &[], on, files);
            queue
                .land_change(
                    &mut repo.annotations(),
                    ChangeId::from_bytes([change; 16]),
                    target,
                )
                .unwrap();
        };
        let [a, b] = ["a", "b"].map(|name| TargetName::new(name).unwrap());
        queue.create_target(&a, &TargetName::MAIN).unwrap();
        queue.create_target(&b, &TargetName::MAIN).unwrap();
        land(1, &a, &[("p", "file")]);
        land(2, &b, &[("p/delete", "original")]);
        // Each lands the other's tip at once, both choosing the directory:
        // two merge bases, one with a file at `p`, the other a directory.
        let tips = [repo.tip(&a), repo.tip(&b)];
        let policy = Some(repo.tip(&TargetName::POLICY).acceptance);
        for (target, prev, other) in [(&a, tips[0], tips[1]), (&b, tips[1], tips[0])] {
            let merged = AcceptanceBody {
                target: target.id(),
                seq: prev.seq + 1,
                prev: Some(prev.acceptance),
                state: repo.files(&[("p/delete", "original")]),
                landed: vec![Source::Target(other.acceptance)],
                evidence: vec![],
                policy,
            }
            .sign(&repo.key);
            repo.store.append(target, &merged).unwrap();
        }
        // `a` deletes `p/delete`, `b` keeps it and adds beside it.
        land(3, &a, &[("a", "new")]);
        land(4, &b, &[("p/delete", "original"), ("p/new", "new")]);

        // What `p` was is unknown, so the deletion can't be told from an
        // addition: it conflicts, rather than bring `p/delete` back.
        let Landing::Pending(pending) = queue.land_target(&mut repo.annotations(), &a, &b).unwrap()
        else {
            panic!("the bases disagree on `p`, so it can't be merged blindly");
        };
        assert!(
            matches!(&pending.blocked, Blocked::Conflict { paths, .. } if paths.iter().map(RepoPath::as_str).eq(["p"])),
            "{pending:?}"
        );
    }

    #[test]
    fn a_waiting_stack_lands_whole_once_it_can() {
        let repo = requiring_ci();
        let lower = repo.change_on_main(1, &[("a", "a")]);
        repo.revision(2, &[], Base::Revision(lower), &[("a", "a"), ("b", "b")]);
        let [Landing::Pending(pending)] = &repo.land(2)[..] else {
            panic!("the lower change waits")
        };
        assert_eq!(pending.subject, RecordId::Revision(lower));
        assert_eq!(
            Queue::pending(&repo.store).unwrap(),
            [Pending {
                proposal: change(2),
                ..pending.clone()
            }],
            "kept as the landing of the whole stack"
        );

        // Retrying it, as once the evidence arrives, lands both: the result
        // is promoted to the upper change, which doesn't change `src`.
        let annotations = Annotations::open(&repo.store).unwrap();
        annotations.issue(&ci_test(lower, "user:ci"), None).unwrap();
        let landings = repo.land(2);
        assert_eq!(landed(&landings[0]), lower);
        assert!(
            matches!(landings[1], Landing::Landed { proposal: ref p, .. } if *p == change(2)),
            "{landings:?}"
        );
        assert_eq!(Queue::pending(&repo.store).unwrap(), []);
    }

    #[test]
    fn each_policy_landing_is_governed_by_the_one_before() {
        let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        let policy = TargetName::POLICY;
        // The first edit requires a review to land on `policy`; the second,
        // stacked on it, must then wait for one.
        let requiring = repo
            .policy_toml("[targets.main]\nauthority = \"node:queue\"\n")
            .replace(
                "[targets.policy]\nauthority = \"node:queue\"\n",
                "[targets.policy]\nauthority = \"node:queue\"\n\
             require = [{ kind = \"review\", verdict = \"pass\", issuer = \"user:bob\" }]\n",
            );
        let first = repo.revision(
            1,
            &[],
            Base::Acceptance(repo.tip(&policy).acceptance),
            &[("policy.toml", &requiring)],
        );
        let open = repo.policy_toml("[targets.main]\nauthority = \"node:queue\"\n");
        repo.revision(2, &[], Base::Revision(first), &[("policy.toml", &open)]);
        let landings = repo
            .queue()
            .land_change(
                &mut repo.annotations(),
                ChangeId::from_bytes([2; 16]),
                &policy,
            )
            .unwrap();
        assert_eq!(landed(&landings[0]), first);
        assert!(matches!(landings[1], Landing::Pending(_)), "{landings:?}");

        let log = repo.store.log(&policy).unwrap();
        for (seq, &id) in log.iter().enumerate() {
            let body = repo.store.get(id).unwrap().body().clone();
            let governing = Policy::governing(id, &body).unwrap();
            assert_eq!(governing, if seq == 0 { id } else { log[seq - 1] });
        }
        // An acceptance naming any other policy than the one before it is
        // refused, its own included.
        let mut body = repo.store.get(log[1]).unwrap().body().clone();
        body.policy = Some(log[1]);
        assert!(matches!(
            Policy::governing(log[1], &body),
            Err(PolicyError::NotPredecessor(1))
        ));
    }
}
