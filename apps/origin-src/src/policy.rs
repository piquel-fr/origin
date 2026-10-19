//! `policy.toml`, in the snapshot of the `policy` target: the nodes and
//! groups of a repository, who orders each target and what landing on it
//! requires, and how long payloads are kept. The policy in force is the one
//! at the target's head.
//!
//! ```toml
//! [nodes]                     # each node's ed25519 public key, in hex
//! hosted = "3b6a27bc..."
//!
//! [groups]                    # users and nodes
//! maintainers = ["user:alice"]
//! runners     = ["node:ci-1", "node:ci-2"]
//!
//! [targets.main]
//! authority = "node:hosted"   # the one node that orders the target
//! require = [
//!   { kind = "ci.test", verdict = "pass", issuer = "group:runners" },
//!   { kind = "review",  verdict = "pass", issuer = "group:maintainers" },
//! ]
//!
//! [retention]
//! "ci.*" = "90d"
//!
//! [grants]                    # capabilities, but path access
//! "group:maintainers" = ["admin", "land *", "annotate review"]
//! "group:runners"     = ["annotate ci.*"]
//!
//! [[paths]]                   # path access: none, read or write
//! path   = "secrets"
//! grants = { all = "none", "group:maintainers" = "write" }
//! ```
//!
//! A target without a table of its own has the authority of `policy`, and
//! requires nothing. Every name a policy refers to must be defined in it,
//! and unknown keys are errors, so a typo never drops a requirement.
//!
//! What each caller may do, their [`Rights`], comes from the last two
//! tables. Under a path, the most specific `[[paths]]` rule that mentions
//! someone decides, and paths no rule mentions them for are writable. A
//! policy without `[grants]` lets everyone land on every target but
//! `policy` and issue every kind of statement. With `[grants]`, each has
//! only what it grants them. Only `admin` lands on `policy`, so no one
//! grants themselves rights they don't have, and no one is `admin` until
//! the policy says so: the node's own user, on its command line, is.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use origin_platform::Platform;
use origin_shared::access::{Capability, Level, PathAccess, Rights, Targets};
use origin_shared::policy::{KindPattern, Principals, Requirement};
use origin_shared::{
    AcceptanceBody, AcceptanceId, EncodedObject, EntryKind, EntryName, InvalidName, InvalidPeriod,
    Kind, NodePublicKey, Period, Principal, PrincipalName, Record, RepoPath, Statement, TargetName,
    Tree, TreeEntry, TreeId,
};
use serde::Deserialize;

use crate::annotations::RevisionView;
use crate::store::{Store, StoreError};

/// The policy of a repository.
#[derive(Debug, Default, Deserialize)]
#[serde(try_from = "PolicyFile")]
pub struct Policy {
    nodes: BTreeMap<PrincipalName, NodePublicKey>,
    groups: BTreeMap<PrincipalName, Vec<Principal>>,
    targets: BTreeMap<TargetName, TargetRules>,
    /// How long payloads are kept.
    pub retention: Retention,
    /// `[grants]`, if the policy has the table.
    grants: Option<Vec<(Grantee, Vec<Capability>)>>,
    paths: Vec<PathRule>,
}

/// Who orders a target, and what landing on it requires.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TargetRules {
    authority: PrincipalName,
    require: Vec<Requirement>,
}

/// A `[[paths]]` rule: what each grantee may do under a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathRule {
    /// The file or directory the rule covers, with everything under it.
    pub path: RepoPath,
    /// What each grantee may do there.
    pub grants: BTreeMap<Grantee, Level>,
}

/// Who a path rule grants access to.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Grantee {
    /// `all`: everyone.
    All,
    /// A user, a node or a group's members.
    Principals(Principals),
}

impl Policy {
    /// The file holding the policy, at the root of the `policy` target.
    pub const FILE: &str = "policy.toml";

    /// The acceptance of `policy` whose `policy.toml` governs the acceptance
    /// `body`, `id`: who must have signed it, and what it had to meet. An
    /// acceptance of `policy` is governed by the one before it, which it
    /// must name, so a policy never authorizes its own landing. Only the
    /// first, which has none before it, is governed by its own. Any other
    /// acceptance is governed by the policy it names, which the caller
    /// checks is on `policy`'s chain. The landing queue checks what it
    /// signs with it, and so must whatever verifies a chain.
    ///
    /// # Errors
    ///
    /// Fails if the acceptance names another policy than that rule's, or
    /// none.
    pub fn governing(id: AcceptanceId, body: &AcceptanceBody) -> Result<AcceptanceId, PolicyError> {
        if body.target != TargetName::POLICY.id() {
            return body.policy.ok_or(PolicyError::Unnamed(body.seq));
        }
        match (body.prev, body.policy) {
            (None, None) => Ok(id),
            (Some(prev), Some(policy)) if policy == prev => Ok(prev),
            _ => Err(PolicyError::NotPredecessor(body.seq)),
        }
    }

    /// The policy in force, at the head of the `policy` target, and the
    /// acceptance holding it.
    ///
    /// # Errors
    ///
    /// Fails if the repository has no policy, its records cannot be read,
    /// or it is invalid.
    pub fn in_force<P: Platform>(store: &Store<P>) -> Result<(AcceptanceId, Self), PolicyError> {
        let head = store
            .head(&TargetName::POLICY)?
            .ok_or(PolicyError::Missing)?;
        let state = store.get(head.acceptance)?.body().state;
        let policy = Self::of_snapshot(store, state)?.ok_or(PolicyError::Missing)?;
        Ok((head.acceptance, policy))
    }

    /// The policy in force, or the default if the repository has none.
    ///
    /// # Errors
    ///
    /// Fails like [`Policy::in_force`], but for a missing policy.
    pub fn load<P: Platform>(store: &Store<P>) -> Result<Self, PolicyError> {
        match Self::in_force(store) {
            Ok((_, policy)) => Ok(policy),
            Err(PolicyError::Missing) => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    /// The policy in `snapshot`, or `None` if it has no `policy.toml`.
    ///
    /// # Errors
    ///
    /// Fails if the snapshot cannot be read, or its `policy.toml` is
    /// invalid.
    pub fn of_snapshot<P: Platform>(
        store: &Store<P>,
        snapshot: TreeId,
    ) -> Result<Option<Self>, PolicyError> {
        let Some(&entry) = store.get(snapshot)?.get(Self::FILE) else {
            return Ok(None);
        };
        let content = store
            .try_blob(entry.id)?
            .ok_or(StoreError::NotFound(entry.id))?;
        let content = String::from_utf8(content).map_err(|_| PolicyError::NotText(snapshot))?;
        toml::from_str(&content)
            .map(Some)
            .map_err(|source| PolicyError::Invalid {
                snapshot,
                source: Box::new(source),
            })
    }

    /// A snapshot holding only `policy` as its `policy.toml`: the file and
    /// the tree.
    pub(crate) fn snapshot(policy: &str) -> (EncodedObject, EncodedObject) {
        let file = EncodedObject::blob(policy.as_bytes());
        let entry = TreeEntry {
            kind: EntryKind::File,
            id: file.id(),
        };
        let name = EntryName::new(Self::FILE).expect("`policy.toml` is a valid name");
        let tree: Tree = [(name, entry)].into_iter().collect();
        (file, tree.encode())
    }

    /// The node that orders `target`: the one its table names, or else the
    /// authority of `policy`. `None` if neither has a table.
    #[must_use]
    pub fn authority(&self, target: &TargetName) -> Option<&PrincipalName> {
        self.targets
            .get(target)
            .or_else(|| self.targets.get(&TargetName::POLICY))
            .map(|rules| &rules.authority)
    }

    /// What landing on `target` requires: nothing, if it has no table.
    #[must_use]
    pub fn requirements(&self, target: &TargetName) -> &[Requirement] {
        self.targets
            .get(target)
            .map_or(&[], |rules| rules.require.as_slice())
    }

    /// The public key of `node`, if the policy lists it.
    #[must_use]
    pub fn node_key(&self, node: &PrincipalName) -> Option<&NodePublicKey> {
        self.nodes.get(node)
    }

    /// Whether `principal` is one of `principals`.
    #[must_use]
    pub fn admits(&self, principals: &Principals, principal: &Principal) -> bool {
        match principals {
            Principals::One(one) => one == principal,
            Principals::Group(group) => self
                .groups
                .get(group)
                .is_some_and(|members| members.contains(principal)),
        }
    }

    /// The `[[paths]]` rules, in order.
    #[must_use]
    pub fn paths(&self) -> &[PathRule] {
        &self.paths
    }

    /// Whether `grantee` covers `principal`, if there's one: `all` covers
    /// anonymous callers too.
    fn covers(&self, grantee: &Grantee, principal: Option<&Principal>) -> bool {
        match (grantee, principal) {
            (Grantee::All, _) => true,
            (Grantee::Principals(principals), Some(principal)) => {
                self.admits(principals, principal)
            }
            (Grantee::Principals(_), None) => false,
        }
    }

    /// Whether `statement`'s issuer may issue a statement of its kind: as
    /// [`Store::authorizes`] says, or for `core.abandon`, if the policy
    /// makes them `admin`.
    ///
    /// # Errors
    ///
    /// Fails if a revision cannot be read.
    pub fn authorizes(
        &self,
        revisions: &impl RevisionView,
        statement: &Statement,
    ) -> Result<bool, StoreError> {
        let admin = self.rights(Some(&statement.issuer)).admin;
        Self::authorizes_admin(revisions, statement, admin)
    }

    /// [`Policy::authorizes`], for an issuer who is `admin` or not: as the
    /// token they put the statement with narrows their rights, say.
    ///
    /// # Errors
    ///
    /// Fails if a revision cannot be read.
    pub fn authorizes_admin(
        revisions: &impl RevisionView,
        statement: &Statement,
        admin: bool,
    ) -> Result<bool, StoreError> {
        if statement.kind == Kind::ABANDON && statement.promoted_from.is_none() && admin {
            return Ok(true);
        }
        revisions.authorizes(statement)
    }

    /// What `principal`, or an anonymous caller, may do, as the module
    /// explains. Anonymous callers never write.
    #[must_use]
    pub fn rights(&self, principal: Option<&Principal>) -> Rights {
        let mut rules: BTreeMap<RepoPath, Level> = BTreeMap::new();
        for rule in &self.paths {
            let granted = rule
                .grants
                .iter()
                .filter(|(grantee, _)| self.covers(grantee, principal))
                .map(|(_, &level)| level)
                .max();
            if let Some(level) = granted {
                let at = rules.entry(rule.path.clone()).or_insert(level);
                *at = (*at).max(level);
            }
        }
        let mut paths = PathAccess::new(Level::Write, rules);
        if principal.is_none() {
            paths = paths.min(&PathAccess::uniform(Level::Read));
        }
        let mut rights = Rights {
            principal: principal.cloned(),
            groups: principal.map_or_else(Vec::new, |principal| {
                self.groups
                    .iter()
                    .filter(|(_, members)| members.contains(principal))
                    .map(|(group, _)| group.clone())
                    .collect()
            }),
            scope: None,
            paths,
            annotate: vec![KindPattern::Any],
            land: vec![Targets::Any],
            admin: false,
        };
        if let Some(grants) = &self.grants {
            rights.annotate.clear();
            rights.land.clear();
            let granted = grants
                .iter()
                .filter(|(grantee, _)| self.covers(grantee, principal))
                .flat_map(|(_, capabilities)| capabilities);
            for capability in granted {
                match capability {
                    Capability::Annotate(kinds) => rights.annotate.push(kinds.clone()),
                    Capability::Land(targets) => rights.land.push(targets.clone()),
                    Capability::Admin => rights.admin = true,
                    // Refused when parsing: paths are in `[[paths]]`.
                    Capability::Read(_) | Capability::Write(_) => {}
                }
            }
        }
        rights
    }
}

/// `policy.toml` as written, which [`Policy::try_from`] parses and checks.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    #[serde(default)]
    nodes: BTreeMap<String, String>,
    #[serde(default)]
    groups: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    targets: BTreeMap<String, TargetFile>,
    #[serde(default)]
    retention: Retention,
    grants: Option<BTreeMap<String, Vec<String>>>,
    #[serde(default)]
    paths: Vec<PathRuleFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetFile {
    authority: String,
    #[serde(default)]
    require: Vec<RequirementFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequirementFile {
    kind: String,
    verdict: String,
    issuer: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathRuleFile {
    path: String,
    grants: BTreeMap<String, String>,
}

impl PolicyFile {
    /// Parses `value`, which is a `what`.
    fn parse<T>(what: &'static str, value: String) -> Result<T, RuleError>
    where
        T: FromStr,
        T::Err: fmt::Display,
    {
        value.parse().map_err(|error: T::Err| RuleError::Invalid {
            what,
            reason: error.to_string(),
            value,
        })
    }
}

impl TryFrom<PolicyFile> for Policy {
    type Error = RuleError;

    fn try_from(file: PolicyFile) -> Result<Self, Self::Error> {
        let nodes = file
            .nodes
            .into_iter()
            .map(|(name, key)| {
                Ok((
                    PolicyFile::parse("node name", name)?,
                    PolicyFile::parse("node key", key)?,
                ))
            })
            .collect::<Result<_, RuleError>>()?;
        let groups = file
            .groups
            .into_iter()
            .map(|(name, members)| {
                let members = members
                    .into_iter()
                    .map(|member| PolicyFile::parse("group member", member))
                    .collect::<Result<_, _>>()?;
                Ok((PolicyFile::parse("group name", name)?, members))
            })
            .collect::<Result<_, RuleError>>()?;
        let mut policy = Self {
            nodes,
            groups,
            targets: BTreeMap::new(),
            retention: file.retention,
            grants: None,
            paths: Vec::new(),
        };
        for (name, target) in file.targets {
            let name: TargetName = PolicyFile::parse("target name", name)?;
            let authority = match PolicyFile::parse("authority", target.authority)? {
                Principal::Node(node) if policy.nodes.contains_key(&node) => node,
                Principal::Node(node) => return Err(RuleError::UnknownNode { target: name, node }),
                user @ Principal::User(_) => {
                    return Err(RuleError::NotANode {
                        target: name,
                        authority: user,
                    });
                }
            };
            let require = target
                .require
                .into_iter()
                .map(|requirement| {
                    Ok(Requirement {
                        kind: PolicyFile::parse("kind", requirement.kind)?,
                        verdict: PolicyFile::parse("verdict", requirement.verdict)?,
                        issuer: policy.check(PolicyFile::parse("issuer", requirement.issuer)?)?,
                    })
                })
                .collect::<Result<_, RuleError>>()?;
            policy
                .targets
                .insert(name, TargetRules { authority, require });
        }
        if let Some(grants) = file.grants {
            let grants = grants
                .into_iter()
                .map(|(grantee, capabilities)| {
                    let grantee = policy.grantee(PolicyFile::parse("grantee", grantee)?)?;
                    let capabilities = capabilities
                        .into_iter()
                        .map(
                            |capability| match PolicyFile::parse("capability", capability)? {
                                Capability::Read(path) | Capability::Write(path) => {
                                    Err(RuleError::PathGrant(path))
                                }
                                capability => Ok(capability),
                            },
                        )
                        .collect::<Result<_, _>>()?;
                    Ok((grantee, capabilities))
                })
                .collect::<Result<_, RuleError>>()?;
            policy.grants = Some(grants);
        }
        for rule in file.paths {
            let grants = rule
                .grants
                .into_iter()
                .map(|(grantee, level)| {
                    let grantee = policy.grantee(PolicyFile::parse("grantee", grantee)?)?;
                    Ok((grantee, PolicyFile::parse("access", level)?))
                })
                .collect::<Result<_, RuleError>>()?;
            policy.paths.push(PathRule {
                path: PolicyFile::parse("path", rule.path)?,
                grants,
            });
        }
        Ok(policy)
    }
}

impl Policy {
    /// The first `policy.toml` of a repository: `node`, whose public key is
    /// `key`, is the authority of `policy` and `main`, and neither requires
    /// anything, so a solo project lands instantly.
    #[must_use]
    pub fn initial(node: &PrincipalName, key: &NodePublicKey) -> String {
        format!(
            "# Who may land what. Changing it is a landing on `policy`.\n\
             \n\
             # Each node's public key.\n\
             [nodes]\n\
             \"{node}\" = \"{key}\"\n\
             \n\
             # Groups of users and nodes, for requirements:\n\
             # [groups]\n\
             # runners = [\"node:ci-1\"]\n\
             \n\
             # The node ordering each target, and the statements landing needs.\n\
             [targets.policy]\n\
             authority = \"node:{node}\"\n\
             \n\
             [targets.main]\n\
             authority = \"node:{node}\"\n\
             # require = [{{ kind = \"ci.test\", verdict = \"pass\", issuer = \"group:runners\" }}]\n\
             \n\
             # Who may land, annotate, and change this policy (`admin`). Without\n\
             # [grants], everyone lands and annotates, and no one is admin\n\
             # remotely.\n\
             # [grants]\n\
             # \"user:alice\" = [\"admin\", \"land *\"]\n\
             \n\
             # Who may read and write paths: everyone writes everything by default.\n\
             # [[paths]]\n\
             # path   = \"secrets\"\n\
             # grants = {{ all = \"none\", \"user:alice\" = \"write\" }}\n"
        )
    }

    /// Checks that the group `grantee` names, if any, is defined.
    fn grantee(&self, grantee: Grantee) -> Result<Grantee, RuleError> {
        match grantee {
            Grantee::Principals(principals) => Ok(Grantee::Principals(self.check(principals)?)),
            Grantee::All => Ok(Grantee::All),
        }
    }

    /// Checks that the group `principals` names, if any, is defined.
    fn check(&self, principals: Principals) -> Result<Principals, RuleError> {
        match &principals {
            Principals::Group(group) if !self.groups.contains_key(group) => {
                Err(RuleError::UnknownGroup(group.clone()))
            }
            _ => Ok(principals),
        }
    }
}

impl FromStr for Grantee {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input == "all" {
            return Ok(Self::All);
        }
        input
            .parse()
            .map(Self::Principals)
            .map_err(|error| format!("{error}, or `all`"))
    }
}

/// An invalid rule in `policy.toml`.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RuleError {
    /// A value doesn't parse.
    #[error("{what}: {reason}")]
    Invalid {
        /// What the value is.
        what: &'static str,
        /// The value.
        value: String,
        /// Why it's invalid.
        reason: String,
    },
    /// A target's authority isn't a node.
    #[error("the authority of `{target}` is {authority}, but only a node can be one")]
    NotANode {
        /// The target.
        target: TargetName,
        /// Its authority.
        authority: Principal,
    },
    /// A target's authority has no key in `[nodes]`.
    #[error("the authority of `{target}`, node:{node}, has no key in [nodes]")]
    UnknownNode {
        /// The target.
        target: TargetName,
        /// Its authority.
        node: PrincipalName,
    },
    /// A rule names a group `[groups]` doesn't define.
    #[error("group:{0} is not defined in [groups]")]
    UnknownGroup(PrincipalName),
    /// `[grants]` grants path access, which `[[paths]]` rules set.
    #[error("[grants] cannot grant access to `{0}`: add a [[paths]] rule")]
    PathGrant(RepoPath),
}

/// How long the payloads of each kind of statement are kept, measured from
/// when this node received the statement: `"ci.*" = "90d"` keeps those of
/// every `ci.` kind for 90 days. A rule names a kind, a prefix ending in
/// `.*`, or `*` for every kind, and the most specific rule that matches
/// wins. Kinds no rule matches keep their payloads, and so do the reserved
/// `core.*` kinds, which core reads, whatever `*` says: a `core.conflict`'s
/// lists its conflicts. A rule naming them is refused.
///
/// Statements themselves are permanent: only their payloads expire.
#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "BTreeMap<String, String>")]
pub struct Retention {
    rules: Vec<(KindPattern, Duration)>,
}

impl Retention {
    /// How long payloads of `kind` are kept, or `None` for ever.
    #[must_use]
    pub fn period(&self, kind: &Kind) -> Option<Duration> {
        if kind.is_reserved() {
            return None;
        }
        self.rules
            .iter()
            .filter_map(|(pattern, period)| Some((pattern.specificity(kind)?, *period)))
            .max_by_key(|&(specificity, _)| specificity)
            .map(|(_, period)| period)
    }

    /// Whether no payload ever expires.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

impl TryFrom<BTreeMap<String, String>> for Retention {
    type Error = RetentionError;

    fn try_from(table: BTreeMap<String, String>) -> Result<Self, Self::Error> {
        let rules = table
            .into_iter()
            .map(|(pattern, period)| {
                let Period(duration) = period.parse()?;
                let pattern: KindPattern = pattern.parse()?;
                if pattern.is_reserved() {
                    return Err(RetentionError::Reserved(pattern));
                }
                Ok((pattern, duration))
            })
            .collect::<Result<_, RetentionError>>()?;
        Ok(Self { rules })
    }
}

/// An invalid `[retention]` rule.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RetentionError {
    /// The key is not a kind, a prefix ending in `.*`, or `*`.
    #[error(transparent)]
    Pattern(#[from] InvalidName),
    /// The value is not a period.
    #[error(transparent)]
    Period(#[from] InvalidPeriod),
    /// The rule names reserved kinds, whose payloads are kept.
    #[error("`{0}` names reserved kinds, whose payloads core reads and always keeps")]
    Reserved(KindPattern),
}

/// A failure to read the policy.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// The repository has no `policy` target, or it holds no `policy.toml`.
    #[error("the repository has no policy: `policy` holds no policy.toml")]
    Missing,
    /// The policy's records cannot be read.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// `policy.toml` isn't UTF-8.
    #[error("the policy.toml of snapshot {} is not text", .0.short())]
    NotText(TreeId),
    /// `policy.toml` isn't a valid policy.
    #[error("invalid policy.toml in snapshot {}", snapshot.short())]
    Invalid {
        /// The snapshot holding it.
        snapshot: TreeId,
        /// What is wrong, boxed: it's large, and rare.
        source: Box<toml::de::Error>,
    },
    /// An acceptance of `policy`, at this sequence number, names another
    /// policy than the acceptance before it.
    #[error("acceptance #{0} of `policy` names another policy than the one before it")]
    NotPredecessor(u64),
    /// An acceptance of another target, at this sequence number, names no
    /// policy.
    #[error("acceptance #{0} names no policy")]
    Unnamed(u64),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retention(toml: &str) -> Result<Retention, toml::de::Error> {
        toml::from_str::<Policy>(toml).map(|policy| policy.retention)
    }

    fn kind(kind: &str) -> Kind {
        Kind::new(kind).unwrap()
    }

    #[test]
    fn the_most_specific_rule_wins() {
        let retention = retention(
            "[retention]\n\"*\" = \"365d\"\n\"ci.*\" = \"90d\"\n\
             \"ci.bench.*\" = \"12h\"\n\"ci.bench\" = \"30m\"\n",
        )
        .unwrap();
        let days = |days: u64| Some(Duration::from_hours(days * 24));
        assert_eq!(retention.period(&kind("ci.test")), days(90));
        assert_eq!(
            retention.period(&kind("ci.bench.x")),
            Some(Duration::from_hours(12))
        );
        assert_eq!(
            retention.period(&kind("ci.bench")),
            Some(Duration::from_mins(30)),
            "the exact kind, not its children's prefix"
        );
        assert_eq!(retention.period(&kind("cix")), days(365), "whole segments");
        assert_eq!(retention.period(&kind("review")), days(365));
        assert_eq!(retention.period(&kind("core.conflict")), None);
    }

    #[test]
    fn reserved_kinds_keep_their_payloads() {
        for rule in ["core.*", "core.conflict"] {
            let error = retention(&format!("[retention]\n\"{rule}\" = \"1d\"\n")).unwrap_err();
            assert!(error.message().contains("reserved kinds"), "{error}");
        }
    }

    /// A policy with every table, whose node `hosted` has the key of the
    /// secret `[42; 32]`.
    fn full_policy() -> String {
        let key = origin_shared::NodeKey::from_bytes(&[42; 32]).public();
        format!(
            "[nodes]\nhosted = \"{key}\"\n\
             [groups]\nrunners = [\"node:ci-1\"]\nmaintainers = [\"user:alice\"]\n\
             [targets.policy]\nauthority = \"node:hosted\"\n\
             [targets.main]\nauthority = \"node:hosted\"\nrequire = [\n\
               {{ kind = \"ci.test\", verdict = \"pass\", issuer = \"group:runners\" }},\n\
               {{ kind = \"review\", verdict = \"pass\", issuer = \"user:bob\" }},\n]\n\
             [grants]\n\"group:maintainers\" = [\"admin\", \"land *\"]\n\
             \"group:runners\" = [\"annotate ci.*\"]\nall = [\"land main\"]\n\
             [[paths]]\npath = \"secrets\"\n\
             grants = {{ all = \"none\", \"group:maintainers\" = \"write\" }}\n\
             [[paths]]\npath = \"/\"\n\
             grants = {{ all = \"read\", \"node:ci-1\" = \"write\" }}\n"
        )
    }

    #[test]
    fn rights_come_from_grants_and_paths() {
        let policy: Policy = toml::from_str(&full_policy()).unwrap();
        let path = |path: &str| RepoPath::new(path).unwrap();
        let alice = policy.rights(Some(&principal("user:alice")));
        assert_eq!(alice.groups, [PrincipalName::new("maintainers").unwrap()]);
        assert!(alice.admin && alice.may_land(&TargetName::POLICY));
        assert!(alice.may_land(&TargetName::new("release").unwrap()));
        assert!(!alice.may_annotate(&Kind::new("ci.test").unwrap()));
        assert_eq!(alice.paths.level(&path("secrets/key")), Level::Write);
        assert_eq!(alice.paths.level(&path("src")), Level::Read, "`all` at `/`");

        let ci = policy.rights(Some(&principal("node:ci-1")));
        assert!(ci.may_annotate(&Kind::new("ci.test").unwrap()));
        assert!(ci.may_land(&TargetName::MAIN), "`all` lands on `main`");
        assert!(!ci.may_land(&TargetName::POLICY) && !ci.admin);
        assert_eq!(ci.paths.level(&path("src")), Level::Write);
        assert_eq!(ci.paths.level(&path("secrets/key")), Level::None);

        let anonymous = policy.rights(None);
        assert_eq!(anonymous.paths.level(&path("src")), Level::Read);
        assert!(!anonymous.paths.reads(&path("secrets")));

        // Without rules, everyone writes everything and lands but on
        // `policy`, and no one is admin.
        let open = Policy::default().rights(Some(&principal("user:bob")));
        assert!(open.paths.writes_all());
        assert!(open.may_land(&TargetName::MAIN) && !open.admin);
        assert!(open.may_annotate(&Kind::new("review").unwrap()));
        assert!(!Policy::default().rights(None).paths.writes(&path("x")));

        let error = toml::from_str::<Policy>("[grants]\nall = [\"write src\"]\n").unwrap_err();
        assert!(
            error.to_string().contains("add a [[paths]] rule"),
            "{error}"
        );
    }

    fn principal(principal: &str) -> Principal {
        principal.parse().unwrap()
    }

    #[test]
    fn parses_every_table() {
        let policy: Policy = toml::from_str(&full_policy()).unwrap();
        let hosted = PrincipalName::new("hosted").unwrap();
        assert_eq!(
            policy.node_key(&hosted),
            Some(&origin_shared::NodeKey::from_bytes(&[42; 32]).public())
        );
        let main = TargetName::MAIN;
        assert_eq!(policy.authority(&main), Some(&hosted));
        let required: Vec<_> = policy
            .requirements(&main)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            required,
            [
                "ci.test pass from group:runners",
                "review pass from user:bob"
            ]
        );
        let runners = &policy.requirements(&main)[0].issuer;
        assert!(policy.admits(runners, &principal("node:ci-1")));
        assert!(
            !policy.admits(runners, &principal("user:ci-1")),
            "a user isn't the node of the same name"
        );

        // A target without a table: the policy's authority, no requirements.
        let other = TargetName::new("overhaul").unwrap();
        assert_eq!(policy.authority(&other), Some(&hosted));
        assert_eq!(policy.requirements(&other), []);

        let [rule, _] = policy.paths() else {
            panic!("two path rules")
        };
        assert_eq!(rule.path, RepoPath::new("secrets").unwrap());
        assert_eq!(rule.grants.get(&Grantee::All), Some(&Level::None));
    }

    #[test]
    fn rejects_rules_naming_what_it_does_not_define() {
        let invalid = |from: &str, to: &str| {
            let toml = full_policy().replace(from, to);
            assert_ne!(toml, full_policy(), "{from}");
            toml::from_str::<Policy>(&toml).unwrap_err().to_string()
        };
        for (from, to, expected) in [
            (
                "node:hosted",
                "node:nobody",
                "node:nobody, has no key in [nodes]",
            ),
            (
                "authority = \"node:hosted\"\nrequire",
                "authority = \"user:hosted\"\nrequire",
                "only a node",
            ),
            (
                "group:runners",
                "group:nobody",
                "group:nobody is not defined",
            ),
            ("group:maintainers", "group:x", "group:x is not defined"),
            ("issuer", "issuers", "unknown field `issuers`"),
            (
                "verdict = \"pass\"",
                "verdict = \"ok\"",
                "verdict: invalid verdict \"ok\"",
            ),
            ("\"write\"", "\"admin\"", "access: `admin` is not `none`"),
            (
                "node:ci-1",
                "ci-1",
                "group member: invalid principal \"ci-1\"",
            ),
        ] {
            let error = invalid(from, to);
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn without_rules_payloads_stay() {
        let retention = retention("[groups]\nrunners = []\n").unwrap();
        assert!(retention.is_empty());
        assert_eq!(retention.period(&kind("ci.test")), None);
    }

    #[test]
    fn rejects_invalid_rules() {
        for invalid in [
            "\"ci.*\" = \"90\"",
            "\"ci.*\" = \"d\"",
            "\"ci.*\" = \"3y\"",
            "\"CI\" = \"1d\"",
            "\"ci.\" = \"1d\"",
            "\"*.ci\" = \"1d\"",
        ] {
            assert!(
                retention(&format!("[retention]\n{invalid}\n")).is_err(),
                "{invalid}"
            );
        }
    }
}
