//! Target names.

use std::borrow::Cow;

use crate::TargetId;
use crate::name::names;

names! {
    /// The name of a target, such as `main` or `release/v3`: `/`-separated
    /// segments of ASCII letters, digits, `.`, `_` and `-`. No segment is
    /// empty or starts with `.`, which keeps names unambiguous in URLs and
    /// on the command line.
    ///
    /// Names never become file names: a target is stored under its
    /// [`TargetId`].
    TargetName("target name");
}

impl TargetName {
    /// The target holding `policy.toml`.
    pub const POLICY: Self = Self(Cow::Borrowed("policy"));
    /// The target every repository starts with.
    pub const MAIN: Self = Self(Cow::Borrowed("main"));

    fn is_valid(name: &str) -> bool {
        name.split('/').all(|segment| {
            !segment.is_empty()
                && !segment.starts_with('.')
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        })
    }

    /// The target's id: the hash of its name.
    #[must_use]
    pub fn id(&self) -> TargetId {
        TargetId::of(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_names_are_validated() {
        for valid in ["main", "release/v3", "v1.2", "a_b-c"] {
            assert!(TargetName::new(valid).is_ok(), "{valid:?}");
        }
        for invalid in [
            "", "/main", "main/", "a//b", ".hidden", "a/../b", "sp ace", "é",
        ] {
            assert!(TargetName::new(invalid).is_err(), "{invalid:?}");
        }
        for constant in [TargetName::POLICY, TargetName::MAIN] {
            assert!(TargetName::is_valid(constant.as_str()), "{constant:?}");
        }
    }

    #[test]
    fn ids_differ_by_name() {
        assert_ne!(TargetName::MAIN.id(), TargetName::POLICY.id());
        assert_eq!(TargetName::MAIN.id(), TargetName::new("main").unwrap().id());
    }
}
