//! Who can read and write which paths of a repository.
//!
//! Rules map paths to grants, for everyone (`all`), for users, and for
//! groups:
//!
//! ```toml
//! [permissions."/"]
//! all = "read"
//! groups.dev = "write"
//!
//! [permissions."secrets"]
//! all = "none"
//! users.alice = "write"
//! ```
//!
//! The most specific rule that mentions someone decides: here everyone reads
//! the repository, `dev` writes it, and only alice sees `secrets`. Paths no
//! rule mentions someone for are writable: repositories are open until
//! permissions are set. Write implies read.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::RepoPath;

/// What someone may do with a path.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Nothing: the content is withheld.
    None,
    /// Read the content.
    Read,
    /// Read and change the content.
    Write,
}

impl std::str::FromStr for Access {
    type Err = String;

    fn from_str(access: &str) -> Result<Self, Self::Err> {
        match access {
            "none" => Ok(Self::None),
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            _ => Err(format!(
                "invalid access `{access}`: expected none, read, or write"
            )),
        }
    }
}

impl std::fmt::Display for Access {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Read => "read",
            Self::Write => "write",
        })
    }
}

/// An authenticated user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Identity {
    /// The user's name.
    pub user: String,
    /// The groups the user belongs to.
    #[serde(default)]
    pub groups: Vec<String>,
}

/// Who a server thinks the caller is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Whoami {
    /// The caller, or `None` when anonymous.
    pub identity: Option<Identity>,
}

/// The grants of one path.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Rule {
    /// Everyone's access, including anonymous callers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all: Option<Access>,
    /// Access of users, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub users: BTreeMap<String, Access>,
    /// Access of groups' members, by group.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub groups: BTreeMap<String, Access>,
}

impl Rule {
    /// The highest access this rule grants `identity`, if it mentions them.
    #[must_use]
    pub fn grant(&self, identity: Option<&Identity>) -> Option<Access> {
        let user = identity.and_then(|identity| self.users.get(&identity.user));
        let groups = identity
            .into_iter()
            .flat_map(|identity| &identity.groups)
            .filter_map(|group| self.groups.get(group));
        self.all.iter().chain(user).chain(groups).max().copied()
    }

    /// Sets `principal`'s access.
    pub fn set(&mut self, principal: Principal, access: Access) {
        match principal {
            Principal::All => self.all = Some(access),
            Principal::User(user) => {
                self.users.insert(user, access);
            }
            Principal::Group(group) => {
                self.groups.insert(group, access);
            }
        }
    }
}

/// Who a grant is for: `all`, `user:<name>`, or `group:<name>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Principal {
    /// Everyone, including anonymous callers.
    All,
    /// One user.
    User(String),
    /// The members of a group.
    Group(String),
}

impl std::str::FromStr for Principal {
    type Err = String;

    fn from_str(principal: &str) -> Result<Self, Self::Err> {
        match principal.split_once(':') {
            None if principal == "all" => Ok(Self::All),
            Some(("user", name)) if !name.is_empty() => Ok(Self::User(name.to_owned())),
            Some(("group", name)) if !name.is_empty() => Ok(Self::Group(name.to_owned())),
            _ => Err(format!(
                "invalid principal `{principal}`: expected all, user:<name>, or group:<name>"
            )),
        }
    }
}

/// A repository's rules, by path.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(try_from = "BTreeMap<String, Rule>", into = "BTreeMap<String, Rule>")]
#[schema(value_type = BTreeMap<String, Rule>)]
pub struct Permissions(BTreeMap<RepoPath, Rule>);

impl Permissions {
    /// What `identity`, or an anonymous caller, may do with `path`.
    #[must_use]
    pub fn access(&self, identity: Option<&Identity>, path: &RepoPath) -> Access {
        path.ancestors()
            .filter_map(|ancestor| self.0.get(&ancestor))
            .find_map(|rule| rule.grant(identity))
            .unwrap_or(Access::Write)
    }

    /// Whether `identity` may see the repository at all: read its root, or
    /// read some path inside it.
    #[must_use]
    pub fn can_see(&self, identity: Option<&Identity>) -> bool {
        self.access(identity, &RepoPath::root()) >= Access::Read
            || self
                .0
                .values()
                .any(|rule| rule.grant(identity) >= Some(Access::Read))
    }

    /// Whether there are no rules: everyone may write everything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The rule of a path, created empty if missing.
    pub fn rule(&mut self, path: RepoPath) -> &mut Rule {
        self.0.entry(path).or_default()
    }

    /// Every rule, by path.
    pub fn rules(&self) -> impl Iterator<Item = (&RepoPath, &Rule)> {
        self.0.iter()
    }
}

impl TryFrom<BTreeMap<String, Rule>> for Permissions {
    type Error = crate::object::InvalidEntryName;

    fn try_from(rules: BTreeMap<String, Rule>) -> Result<Self, Self::Error> {
        let rules = rules
            .into_iter()
            .map(|(path, rule)| Ok((RepoPath::new(&path)?, rule)))
            .collect::<Result<_, Self::Error>>()?;
        Ok(Self(rules))
    }
}

impl From<Permissions> for BTreeMap<String, Rule> {
    fn from(permissions: Permissions) -> Self {
        permissions
            .0
            .into_iter()
            .map(|(path, rule)| (path.to_string(), rule))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(user: &str, groups: &[&str]) -> Identity {
        Identity {
            user: user.into(),
            groups: groups.iter().map(|&group| group.into()).collect(),
        }
    }

    fn path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn permissions() -> Permissions {
        toml_like(&[
            (
                "/",
                Rule {
                    all: Some(Access::Read),
                    groups: [("dev".into(), Access::Write)].into(),
                    ..Rule::default()
                },
            ),
            (
                "secrets",
                Rule {
                    all: Some(Access::None),
                    users: [("alice".into(), Access::Write)].into(),
                    ..Rule::default()
                },
            ),
            (
                "docs",
                Rule {
                    users: [("bob".into(), Access::None)].into(),
                    ..Rule::default()
                },
            ),
        ])
    }

    fn toml_like(rules: &[(&str, Rule)]) -> Permissions {
        rules
            .iter()
            .map(|(path, rule)| ((*path).to_owned(), rule.clone()))
            .collect::<BTreeMap<_, _>>()
            .try_into()
            .unwrap()
    }

    #[test]
    fn the_most_specific_mention_decides() {
        let permissions = permissions();
        let (alice, bob, dev) = (
            identity("alice", &[]),
            identity("bob", &["dev"]),
            identity("carol", &["dev"]),
        );
        assert_eq!(permissions.access(None, &path("src/lib.rs")), Access::Read);
        assert_eq!(
            permissions.access(Some(&dev), &path("src/lib.rs")),
            Access::Write
        );
        assert_eq!(
            permissions.access(Some(&dev), &path("secrets/key")),
            Access::None
        );
        assert_eq!(
            permissions.access(Some(&alice), &path("secrets/key")),
            Access::Write
        );
        // `docs` mentions bob only: everyone else falls through to `/`.
        assert_eq!(
            permissions.access(Some(&bob), &path("docs/guide.md")),
            Access::None
        );
        assert_eq!(
            permissions.access(Some(&dev), &path("docs/guide.md")),
            Access::Write
        );
    }

    #[test]
    fn repositories_are_open_until_rules_are_set() {
        let permissions = Permissions::default();
        assert_eq!(permissions.access(None, &path("anything")), Access::Write);
        assert!(permissions.can_see(None));
    }

    #[test]
    fn seeing_a_repository_takes_reading_part_of_it() {
        let permissions = toml_like(&[
            (
                "/",
                Rule {
                    all: Some(Access::None),
                    ..Rule::default()
                },
            ),
            (
                "public",
                Rule {
                    users: [("bob".into(), Access::Read)].into(),
                    ..Rule::default()
                },
            ),
        ]);
        assert!(!permissions.can_see(None));
        assert!(permissions.can_see(Some(&identity("bob", &[]))));
    }

    #[test]
    fn rules_round_trip_with_readable_paths() {
        let json = serde_json::to_value(permissions()).unwrap();
        assert!(json.get("/").is_some(), "the root is written `/`: {json}");
        let back: Permissions = serde_json::from_value(json).unwrap();
        assert_eq!(back, permissions());
    }
}
