//! `checks.toml`, at the root of the snapshot under test: the checks to
//! run on it.
//!
//! ```toml
//! [checks.lint]
//! command   = "cargo clippy --workspace -- -D warnings"
//! inputs    = ["src", "Cargo.toml", "Cargo.lock"]
//! toolchain = "rustc -V"
//!
//! [checks.links]
//! command  = "lychee docs"
//! inputs   = ["docs"]
//! external = true          # reads the network: never promoted
//! ```
//!
//! Each check issues statements of the kind `ci.<name>`. Its `inputs` are
//! the files and directories it reads, the whole snapshot by default; its
//! `toolchain`, a command whose output identifies the tools it runs with.
//! A result is promoted to every revision where the inputs, the command and
//! the toolchain's output are the same, unless the check is `external`.

use std::collections::BTreeMap;

use origin_shared::{Digest, Inputs, InvalidName, Kind, RepoPath};
use serde::Deserialize;

/// The checks of a snapshot, by kind.
#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "ChecksFile")]
pub struct Checks(BTreeMap<Kind, Check>);

/// One check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// The kind of the statements it issues: `ci.<name>`.
    pub kind: Kind,
    /// The shell command that runs it, in a checkout of the snapshot: it
    /// passes if it exits with 0.
    pub command: String,
    /// The files and directories it reads.
    pub inputs: Vec<RepoPath>,
    /// The shell command whose output identifies its tools, if any.
    pub toolchain: Option<String>,
    /// Whether it also reads what isn't in the snapshot, such as the
    /// network, so its results are never promoted.
    pub external: bool,
}

impl Checks {
    /// The file holding the checks, at the root of a snapshot.
    pub const FILE: &str = "checks.toml";

    /// The namespace of the kinds checks issue.
    pub const NAMESPACE: &str = "ci";

    /// Parses `checks.toml`.
    ///
    /// # Errors
    ///
    /// Fails if it isn't valid, or a check's name doesn't make a kind.
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// The check issuing `kind`, if there's one.
    #[must_use]
    pub fn get(&self, kind: &Kind) -> Option<&Check> {
        self.0.get(kind)
    }

    /// Every check, by kind.
    pub fn iter(&self) -> impl Iterator<Item = &Check> {
        self.0.values()
    }
}

impl Check {
    /// What a result of the check declares it depended on, with the output
    /// of its toolchain command, if it has one.
    #[must_use]
    pub fn inputs(&self, toolchain: &[u8]) -> Inputs {
        Inputs {
            paths: self.inputs.clone(),
            definition: Digest::of(self.command.as_bytes()),
            toolchain: Digest::of(toolchain),
            external: self.external,
        }
    }
}

/// `checks.toml` as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChecksFile {
    #[serde(default)]
    checks: BTreeMap<String, CheckFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckFile {
    command: String,
    inputs: Option<Vec<RepoPath>>,
    toolchain: Option<String>,
    #[serde(default)]
    external: bool,
}

impl TryFrom<ChecksFile> for Checks {
    type Error = InvalidName;

    fn try_from(file: ChecksFile) -> Result<Self, Self::Error> {
        file.checks
            .into_iter()
            .map(|(name, check)| {
                let kind = Kind::new(format!("{}.{name}", Self::NAMESPACE))?;
                let check = Check {
                    kind: kind.clone(),
                    command: check.command,
                    inputs: check.inputs.unwrap_or_else(|| vec![RepoPath::root()]),
                    toolchain: check.toolchain,
                    external: check.external,
                };
                Ok((kind, check))
            })
            .collect::<Result<_, _>>()
            .map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_checks_by_kind() {
        let checks = Checks::parse(
            "[checks.lint]\ncommand = \"cargo clippy\"\ninputs = [\"src\", \"Cargo.toml\"]\n\
             toolchain = \"rustc -V\"\n\
             [checks.links]\ncommand = \"lychee docs\"\nexternal = true\n",
        )
        .unwrap();
        let lint = checks.get(&Kind::new("ci.lint").unwrap()).unwrap();
        assert_eq!(
            lint.inputs,
            [
                RepoPath::new("src").unwrap(),
                RepoPath::new("Cargo.toml").unwrap()
            ]
        );
        assert_eq!(lint.toolchain.as_deref(), Some("rustc -V"));
        let links = checks.get(&Kind::new("ci.links").unwrap()).unwrap();
        assert_eq!(links.inputs, [RepoPath::root()], "the whole snapshot");
        assert!(links.external && links.inputs(b"").external);
        assert_eq!(checks.iter().count(), 2);

        assert!(
            Checks::parse("[checks.Lint]\ncommand = \"x\"\n").is_err(),
            "not a kind"
        );
        assert!(
            Checks::parse("[checks.lint]\ncmd = \"x\"\n").is_err(),
            "unknown key"
        );
        assert_eq!(Checks::parse("").unwrap(), Checks::default());
    }

    #[test]
    fn inputs_follow_the_command_and_toolchain() {
        let check = |command: &str| Check {
            kind: Kind::new("ci.test").unwrap(),
            command: command.to_owned(),
            inputs: vec![RepoPath::new("src").unwrap()],
            toolchain: None,
            external: false,
        };
        let inputs = check("cargo test").inputs(b"rustc 1.99.0");
        assert_eq!(inputs, check("cargo test").inputs(b"rustc 1.99.0"));
        assert_ne!(
            inputs,
            check("cargo test --release").inputs(b"rustc 1.99.0")
        );
        assert_ne!(inputs, check("cargo test").inputs(b"rustc 1.100.0"));
    }
}
