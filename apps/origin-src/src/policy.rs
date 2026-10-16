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
//! [[paths]]
//! path   = "secrets"
//! grants = { all = "none", "group:maintainers" = "write" }
//! ```
//!
//! A target without a table of its own has the authority of `policy`, and
//! requires nothing. Every name a policy refers to must be defined in it,
//! and unknown keys are errors, so a typo never drops a requirement. Path
//! rules are parsed and checked, but not enforced yet.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use origin_platform::Platform;
use origin_shared::{
    AcceptanceBody, AcceptanceId, EncodedObject, EntryKind, EntryName, Kind, NodePublicKey,
    Principal, PrincipalName, Record, RepoPath, TargetName, Tree, TreeEntry, TreeId, Verdict,
};
use serde::Deserialize;

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
    paths: Vec<PathRule>,
}

/// Who orders a target, and what landing on it requires.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TargetRules {
    authority: PrincipalName,
    require: Vec<Requirement>,
}

/// A statement that landing needs: of `kind`, with `verdict`, from one of
/// `issuer`, about what lands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requirement {
    /// The statement's kind.
    pub kind: Kind,
    /// Its verdict.
    pub verdict: Verdict,
    /// Who may issue it.
    pub issuer: Principals,
}

/// One user or node, or the members of a group: `user:alice`,
/// `node:ci-1` or `group:runners`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Principals {
    /// A user or a node.
    One(Principal),
    /// The members of a group of the policy.
    Group(PrincipalName),
}

/// A `[[paths]]` rule: what each grantee may do under a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathRule {
    /// The file or directory the rule covers, with everything under it.
    pub path: RepoPath,
    /// What each grantee may do there.
    pub grants: BTreeMap<Grantee, Access>,
}

/// Who a path rule grants access to.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Grantee {
    /// `all`: everyone.
    All,
    /// A user, a node or a group's members.
    Principals(Principals),
}

/// What a path rule grants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Nothing, not even the content.
    None,
    /// Reading the content.
    Read,
    /// Proposing revisions that change it.
    Write,
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
        for rule in file.paths {
            let grants = rule
                .grants
                .into_iter()
                .map(|(grantee, access)| {
                    let grantee = match PolicyFile::parse("grantee", grantee)? {
                        Grantee::Principals(principals) => {
                            Grantee::Principals(policy.check(principals)?)
                        }
                        Grantee::All => Grantee::All,
                    };
                    Ok((grantee, PolicyFile::parse("access", access)?))
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

impl FromStr for Principals {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input.strip_prefix("group:") {
            Some(group) => PrincipalName::new(group)
                .map(Self::Group)
                .map_err(|error| error.to_string()),
            None => input.parse().map(Self::One).map_err(|_| {
                format!("`{input}` is not `user:<name>`, `node:<name>` or `group:<name>`")
            }),
        }
    }
}

impl fmt::Display for Principals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::One(principal) => principal.fmt(f),
            Self::Group(group) => write!(f, "group:{group}"),
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

impl FromStr for Access {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "none" => Ok(Self::None),
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            _ => Err(format!("`{input}` is not `none`, `read` or `write`")),
        }
    }
}

impl fmt::Display for Requirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} from {}", self.kind, self.verdict, self.issuer)
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
}

/// How long the payloads of each kind of statement are kept, measured from
/// when this node received the statement: `"ci.*" = "90d"` keeps those of
/// every `ci.` kind for 90 days. A rule names a kind, a prefix ending in
/// `.*`, or `*` for every kind, and the most specific rule that matches
/// wins. Kinds no rule matches keep their payloads.
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

    /// Parses a period: a whole number of seconds (`s`), minutes (`m`),
    /// hours (`h`) or days (`d`), such as `90d`.
    fn parse_period(period: &str) -> Option<Duration> {
        let unit = match period.chars().last()? {
            's' => 1,
            'm' => 60,
            'h' => 60 * 60,
            'd' => 24 * 60 * 60,
            _ => return None,
        };
        let count: u64 = period[..period.len() - 1].parse().ok()?;
        Some(Duration::from_secs(count.checked_mul(unit)?))
    }
}

impl TryFrom<BTreeMap<String, String>> for Retention {
    type Error = RetentionError;

    fn try_from(table: BTreeMap<String, String>) -> Result<Self, Self::Error> {
        let rules = table
            .into_iter()
            .map(|(pattern, period)| {
                let duration = Self::parse_period(&period)
                    .ok_or_else(|| RetentionError::Period(period.clone()))?;
                Ok((pattern.parse()?, duration))
            })
            .collect::<Result<_, RetentionError>>()?;
        Ok(Self { rules })
    }
}

/// Statement kinds, as retention rules and `origin watch --kind` name
/// them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KindPattern {
    /// `*`: every kind.
    Any,
    /// `ci.*`: the kinds under a prefix, not the prefix itself.
    Prefix(Kind),
    /// `ci.test`: one kind.
    Exact(Kind),
}

impl KindPattern {
    /// Whether the pattern matches `kind`.
    #[must_use]
    pub fn matches(&self, kind: &Kind) -> bool {
        self.specificity(kind).is_some()
    }

    /// How specifically the pattern matches `kind`, if it does: longer
    /// prefixes beat shorter ones, and the kind itself beats them all.
    fn specificity(&self, kind: &Kind) -> Option<(usize, bool)> {
        match self {
            Self::Any => Some((0, false)),
            Self::Prefix(prefix) => kind
                .as_str()
                .strip_prefix(prefix.as_str())
                .is_some_and(|rest| rest.starts_with('.'))
                .then_some((prefix.as_str().len(), false)),
            Self::Exact(exact) => (exact == kind).then_some((exact.as_str().len(), true)),
        }
    }
}

impl FromStr for KindPattern {
    type Err = RetentionError;

    fn from_str(pattern: &str) -> Result<Self, Self::Err> {
        let invalid = || RetentionError::Pattern(pattern.to_owned());
        match pattern.strip_suffix(".*") {
            _ if pattern == "*" => Ok(Self::Any),
            Some(prefix) => Kind::new(prefix).map(Self::Prefix).map_err(|_| invalid()),
            None => Kind::new(pattern).map(Self::Exact).map_err(|_| invalid()),
        }
    }
}

/// An invalid `[retention]` rule.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RetentionError {
    /// The key is not a kind, a prefix ending in `.*`, or `*`.
    #[error("`{0}` is not a statement kind, a prefix like `ci.*`, or `*`")]
    Pattern(String),
    /// The value is not a period.
    #[error("`{0}` is not a period like `90d`, `12h`, `30m` or `45s`")]
    Period(String),
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
             [[paths]]\npath = \"secrets\"\n\
             grants = {{ all = \"none\", \"group:maintainers\" = \"write\" }}\n"
        )
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

        let [rule] = policy.paths() else {
            panic!("one path rule")
        };
        assert_eq!(rule.path, RepoPath::new("secrets").unwrap());
        assert_eq!(rule.grants.get(&Grantee::All), Some(&Access::None));
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
