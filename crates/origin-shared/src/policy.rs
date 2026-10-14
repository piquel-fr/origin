//! The parts of a repository's policy that its API shows: what landing on a
//! target requires, and patterns of statement kinds.

use std::fmt;
use std::str::FromStr;

use crate::{InvalidName, Kind, Principal, PrincipalName, Verdict};

/// A statement that landing needs: of `kind`, with `verdict`, from one of
/// `issuer`, about what lands.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Requirement {
    /// The statement's kind.
    pub kind: Kind,
    /// Its verdict.
    pub verdict: Verdict,
    /// Who may issue it.
    pub issuer: Principals,
}

impl fmt::Display for Requirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} from {}", self.kind, self.verdict, self.issuer)
    }
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

impl FromStr for Principals {
    type Err = InvalidName;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input.strip_prefix("group:") {
            Some(group) => PrincipalName::new(group).map(Self::Group),
            None => input.parse().map(Self::One).map_err(|_| InvalidName {
                what: "principal, `user:<name>`, `node:<name>` or `group:<name>`,",
                name: input.to_owned(),
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

/// Statement kinds, as retention rules and event filters name them.
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
    #[must_use]
    pub fn specificity(&self, kind: &Kind) -> Option<(usize, bool)> {
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
    type Err = InvalidName;

    fn from_str(pattern: &str) -> Result<Self, Self::Err> {
        let invalid = |_| InvalidName {
            what: "statement kind, prefix like `ci.*`, or `*`",
            name: pattern.to_owned(),
        };
        match pattern.strip_suffix(".*") {
            _ if pattern == "*" => Ok(Self::Any),
            Some(prefix) => Kind::new(prefix).map(Self::Prefix).map_err(invalid),
            None => Kind::new(pattern).map(Self::Exact).map_err(invalid),
        }
    }
}

impl fmt::Display for KindPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => f.write_str("*"),
            Self::Prefix(prefix) => write!(f, "{prefix}.*"),
            Self::Exact(kind) => kind.fmt(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_patterns_match_by_prefix() {
        let kind = |kind: &str| Kind::new(kind).unwrap();
        let ci: KindPattern = "ci.*".parse().unwrap();
        assert!(ci.matches(&kind("ci.test")));
        assert!(!ci.matches(&kind("ci")), "the prefix itself isn't under it");
        assert!(!ci.matches(&kind("cix.test")));
        assert!("*".parse::<KindPattern>().unwrap().matches(&kind("review")));
        for pattern in ["*", "ci.*", "ci.test"] {
            assert_eq!(pattern.parse::<KindPattern>().unwrap().to_string(), pattern);
        }
        assert!("ci.**".parse::<KindPattern>().is_err());
    }

    #[test]
    fn principals_parse_as_they_display() {
        for valid in ["user:alice", "node:ci-1", "group:runners"] {
            assert_eq!(valid.parse::<Principals>().unwrap().to_string(), valid);
        }
        assert!("team:x".parse::<Principals>().is_err());
    }
}
