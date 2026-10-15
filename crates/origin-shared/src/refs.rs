//! Branch names and the updates that move them.

use std::fmt;
use std::str::FromStr;

use crate::ObjectId;

/// The name of a branch, such as `main` or `feature/login`.
///
/// Names are `/`-separated segments of ASCII letters, digits, `.`, `_` and
/// `-`. No segment is empty or starts with `.`, which keeps names usable as
/// file paths and git ref names.
#[derive(
    Clone,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[serde(try_from = "String", into = "String")]
#[schema(value_type = String, example = "main")]
pub struct BranchName(String);

impl BranchName {
    /// Validates a branch name.
    ///
    /// # Errors
    ///
    /// Fails if the name breaks the rules above.
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidBranchName> {
        let name = name.into();
        let valid_segment = |segment: &str| {
            !segment.is_empty()
                && !segment.starts_with('.')
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        };
        if name.split('/').all(valid_segment) {
            Ok(Self(name))
        } else {
            Err(InvalidBranchName(name))
        }
    }

    /// The name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The branch new repositories start on: `main`.
impl Default for BranchName {
    fn default() -> Self {
        Self("main".to_owned())
    }
}

impl FromStr for BranchName {
    type Err = InvalidBranchName;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::new(name)
    }
}

impl TryFrom<String> for BranchName {
    type Error = InvalidBranchName;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        Self::new(name)
    }
}

impl From<BranchName> for String {
    fn from(name: BranchName) -> Self {
        name.0
    }
}

impl fmt::Display for BranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for BranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

/// A branch name that [`BranchName::new`] rejected.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid branch name {0:?}")]
pub struct InvalidBranchName(pub String);

/// A compare-and-swap of one branch. Updates are applied in transactions:
/// all of them succeed, or none do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefUpdate {
    /// The branch to move.
    pub branch: BranchName,
    /// Where the branch must point for the update to apply. `None` requires
    /// the branch not to exist.
    pub expected: Option<ObjectId>,
    /// Where the branch will point. `None` deletes it.
    pub new: Option<ObjectId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_are_validated() {
        for valid in ["main", "feature/login", "v1.2", "a_b-c"] {
            assert!(BranchName::new(valid).is_ok(), "{valid:?}");
        }
        for invalid in [
            "", "/main", "main/", "a//b", ".hidden", "a/../b", "sp ace", "é",
        ] {
            assert!(BranchName::new(invalid).is_err(), "{invalid:?}");
        }
    }
}
