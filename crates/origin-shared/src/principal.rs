//! Who wrote a record.

use std::fmt;
use std::str::FromStr;

use crate::InvalidName;
use crate::name::names;
use crate::object::tagged;

names! {
    /// The name of a user or a node: ASCII letters, digits, `.`, `_` and
    /// `-`, not starting with `.`.
    PrincipalName("principal name");
}

impl PrincipalName {
    fn is_valid(name: &str) -> bool {
        !name.is_empty()
            && !name.starts_with('.')
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    }
}

/// The author of a revision or the issuer of a statement, written
/// `user:<name>` or `node:<name>`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Principal {
    /// A person, or an agent acting under a user's name.
    User(PrincipalName),
    /// A running copy of a repository: a laptop, a CI runner or a server.
    Node(PrincipalName),
}

tagged!(Principal { User = 1, Node = 2 });

impl Principal {
    /// The prefix written before the name.
    fn prefix(&self) -> &'static str {
        match self {
            Self::User(_) => "user",
            Self::Node(_) => "node",
        }
    }

    /// The user's or node's name.
    #[must_use]
    pub fn name(&self) -> &PrincipalName {
        match self {
            Self::User(name) | Self::Node(name) => name,
        }
    }
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.prefix(), self.name())
    }
}

impl fmt::Debug for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.to_string(), f)
    }
}

impl FromStr for Principal {
    type Err = InvalidName;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidName {
            what: "principal",
            name: input.to_owned(),
        };
        let (prefix, name) = input.split_once(':').ok_or_else(invalid)?;
        let name = PrincipalName::new(name)?;
        match prefix {
            "user" => Ok(Self::User(name)),
            "node" => Ok(Self::Node(name)),
            _ => Err(invalid()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_displays() {
        for valid in ["user:alice", "node:ci-1", "user:a.b_c"] {
            assert_eq!(valid.parse::<Principal>().unwrap().to_string(), valid);
        }
        for invalid in [
            "alice", "group:x", "user:", "user:.x", "node:a b", "user:a:b",
        ] {
            assert!(invalid.parse::<Principal>().is_err(), "{invalid:?}");
        }
    }
}
