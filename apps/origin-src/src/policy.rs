//! `policy.toml`, in the snapshot of the `policy` target: who may land what,
//! and how long payloads are kept. The policy in force is the one at the
//! target's head. Only `[retention]` is read so far; the other tables are
//! left to the code that enforces them.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

use origin_platform::Platform;
use origin_shared::{AcceptanceId, Kind, TargetName};
use serde::Deserialize;

use crate::store::{Store, StoreError};

/// The policy of a repository.
#[derive(Debug, Default, Deserialize)]
pub struct Policy {
    /// How long payloads are kept.
    #[serde(default)]
    pub retention: Retention,
}

impl Policy {
    /// The file holding the policy, at the root of the `policy` target.
    pub const FILE: &str = "policy.toml";

    /// The policy at the head of the `policy` target, or the default if the
    /// repository has no such target or file.
    ///
    /// # Errors
    ///
    /// Fails if the target's records cannot be read, or the file is not a
    /// valid policy.
    pub fn load<P: Platform>(store: &Store<P>) -> Result<Self, PolicyError> {
        let Some(head) = store.head(&TargetName::POLICY)? else {
            return Ok(Self::default());
        };
        let state = store.get(head.acceptance)?.body().state;
        let Some(&entry) = store.get(state)?.get(Self::FILE) else {
            return Ok(Self::default());
        };
        let invalid = |source| PolicyError::Invalid {
            acceptance: head.acceptance,
            source,
        };
        let content = store
            .try_blob(entry.id)?
            .ok_or(StoreError::NotFound(entry.id))?;
        let content =
            String::from_utf8(content).map_err(|_| PolicyError::NotText(head.acceptance))?;
        toml::from_str(&content).map_err(invalid)
    }
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

/// The kinds a retention rule applies to.
#[derive(Clone, Debug, PartialEq, Eq)]
enum KindPattern {
    /// `*`: every kind.
    Any,
    /// `ci.*`: the kinds under a prefix, not the prefix itself.
    Prefix(Kind),
    /// `ci.test`: one kind.
    Exact(Kind),
}

impl KindPattern {
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
    /// The policy's records cannot be read.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// `policy.toml` isn't UTF-8.
    #[error("the policy.toml of acceptance {0} is not text")]
    NotText(AcceptanceId),
    /// `policy.toml` isn't a valid policy.
    #[error("invalid policy.toml in acceptance {acceptance}")]
    Invalid {
        /// The acceptance whose snapshot holds it.
        acceptance: AcceptanceId,
        /// What is wrong.
        source: toml::de::Error,
    },
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

    #[test]
    fn without_rules_payloads_stay() {
        let retention = retention("[targets.main]\nauthority = \"node:a\"\n").unwrap();
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
