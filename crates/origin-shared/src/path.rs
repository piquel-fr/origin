//! Paths of files and directories within a repository.

use std::fmt;
use std::str::FromStr;

use crate::InvalidName;
use crate::object::{DecodeError, EntryName, Field, Reader};

/// A path relative to the repository root, such as `src/main.rs`.
///
/// Components are valid [`EntryName`]s joined by `/`. The empty path is the
/// root directory.
#[derive(Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoPath(String);

impl RepoPath {
    /// The repository root.
    #[must_use]
    pub const fn root() -> Self {
        Self(String::new())
    }

    /// Validates a `/`-separated path. Leading and trailing slashes are
    /// ignored, so `/src/` is `src`.
    ///
    /// # Errors
    ///
    /// Fails if any component is not a valid [`EntryName`].
    pub fn new(path: &str) -> Result<Self, InvalidName> {
        let trimmed = path.trim_matches('/');
        if trimmed.is_empty() {
            return Ok(Self::root());
        }
        for component in trimmed.split('/') {
            EntryName::new(component)?;
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// Whether this is the repository root.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The path of a child entry.
    #[must_use]
    pub fn join(&self, name: &EntryName) -> Self {
        if self.is_root() {
            Self(name.as_str().to_owned())
        } else {
            Self(format!("{}/{name}", self.0))
        }
    }

    /// The components, outermost first.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|component| !component.is_empty())
    }

    /// Whether `self` is `ancestor` or inside it, comparing whole components:
    /// `src/lib` is inside `src` but not inside `sr`.
    #[must_use]
    pub fn starts_with(&self, ancestor: &RepoPath) -> bool {
        ancestor.is_root()
            || self.0 == ancestor.0
            || self
                .0
                .strip_prefix(&ancestor.0)
                .is_some_and(|rest| rest.starts_with('/'))
    }

    /// The path as a string, empty for the root.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for RepoPath {
    type Err = InvalidName;

    fn from_str(path: &str) -> Result<Self, Self::Err> {
        Self::new(path)
    }
}

/// Encoded as a name. Only the normalized form decodes, so each path has one
/// encoding.
impl Field for RepoPath {
    fn write(&self, out: &mut Vec<u8>) {
        Reader::write_str(&self.0, out);
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let encoded = reader.prefixed_str()?;
        let path = Self::new(encoded)?;
        if path.as_str() == encoded {
            Ok(path)
        } else {
            Err(DecodeError::NonCanonical("path"))
        }
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() {
            f.write_str("/")
        } else {
            f.write_str(&self.0)
        }
    }
}

impl fmt::Debug for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    #[test]
    fn parses_and_normalizes() {
        assert!(path("").is_root());
        assert!(path("/").is_root());
        assert_eq!(path("/src/main.rs/").as_str(), "src/main.rs");
        assert_eq!(
            path("src/main.rs").components().collect::<Vec<_>>(),
            ["src", "main.rs"]
        );
        for invalid in ["a//b", "a/../b", "a/./b", ".origin/objects"] {
            assert!(RepoPath::new(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn joins_children() {
        let name = EntryName::new("lib.rs").unwrap();
        assert_eq!(RepoPath::root().join(&name), path("lib.rs"));
        assert_eq!(path("src").join(&name), path("src/lib.rs"));
    }

    #[test]
    fn ancestry_compares_whole_components() {
        assert!(path("src/lib").starts_with(&path("src")));
        assert!(path("src").starts_with(&path("src")));
        assert!(path("src").starts_with(&RepoPath::root()));
        assert!(!path("src/lib").starts_with(&path("sr")));
        assert!(!path("src").starts_with(&path("src/lib")));
    }
}
