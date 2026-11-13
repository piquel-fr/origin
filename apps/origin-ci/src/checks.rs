//! `checks.toml`, at the root of a snapshot: the checks to run on it, or
//! for a worker, at the tip of a target, on what lands on it.
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
//!
//! `checks.toml` is always one of the inputs, read from where the checks
//! are in force: see [`Defined`]. The landing queue promotes results by
//! input digest alone, without reading the checks, so a result must never
//! share the digest of one that ran another definition. A worker runs the
//! checks a target defines, so its results declare the file as the state
//! of the target the record is on holds it, never the record's own copy: a
//! candidate that restores an older `checks.toml` doesn't match the results
//! of the older definition, and one that only edits it still matches those
//! of the definition in force. Nor does the queue promote a result from
//! one target to another, whose checks may differ.
//!
//! **Known limitation.** The toolchain's output a result declares is the
//! one it ran with: the queue never runs the toolchain command, so tools
//! upgraded where checks run, with the inputs unchanged, don't stop the
//! old results from being promoted. Pin the tools in a file the check
//! reads instead, such as `rust-toolchain.toml` among its `inputs`: then
//! a new toolchain is a change of the inputs, and the check runs again.

use std::borrow::Cow;
use std::collections::BTreeMap;

use origin_shared::{Digest, Inputs, InvalidName, Kind, RepoPath};
use serde::Deserialize;

/// Where the `checks.toml` a check is defined by is in force, which its
/// results declare it read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Defined {
    /// In the snapshot checked, as `origin ci run` reads it: one of the
    /// paths read from the snapshot.
    InSnapshot,
    /// On the target the record checked is on, as a worker reads it: one
    /// of the paths read from the target's state.
    OnTarget,
}

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
    /// The files and directories it reads: for a check
    /// [defined](Defined::InSnapshot) in the snapshot, `checks.toml`
    /// included.
    pub inputs: Vec<RepoPath>,
    /// Those it reads from the state of the target the record is on: for a
    /// check [defined](Defined::OnTarget) there, `checks.toml`.
    pub target_inputs: Vec<RepoPath>,
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

    /// Parses `checks.toml`, in force where `defined` says, which each
    /// check declares among its inputs.
    ///
    /// # Errors
    ///
    /// Fails if it isn't valid, or a check's name doesn't make a kind.
    pub fn parse(text: &str, defined: Defined) -> Result<Self, toml::de::Error> {
        let mut checks: Self = toml::from_str(text)?;
        let file = Self::path();
        for check in checks.0.values_mut() {
            match defined {
                Defined::InSnapshot
                    if !check.inputs.iter().any(|input| file.starts_with(input)) =>
                {
                    check.inputs.push(file.clone());
                }
                Defined::InSnapshot => {}
                Defined::OnTarget => check.target_inputs.push(file.clone()),
            }
        }
        Ok(checks)
    }

    /// [`Checks::FILE`], as a path.
    fn path() -> RepoPath {
        RepoPath::new(Self::FILE).expect("a valid path")
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

    /// The kind of the statements the check `name` issues: `ci.<name>`.
    ///
    /// # Errors
    ///
    /// Fails if the name doesn't make a kind.
    pub fn kind(name: &str) -> Result<Kind, InvalidName> {
        Kind::new(format!("{}.{name}", Self::NAMESPACE))
    }
}

impl Check {
    /// A check given rather than read from `checks.toml`, named `name`:
    /// it runs `words`, each quoted for the shell, and reads `inputs`, the
    /// whole snapshot if none.
    ///
    /// # Errors
    ///
    /// Fails if the name doesn't make a kind.
    pub fn adhoc(
        name: &str,
        words: &[String],
        inputs: Vec<RepoPath>,
        toolchain: Option<String>,
        external: bool,
    ) -> Result<Self, InvalidName> {
        let words: Vec<_> = words.iter().map(|word| Self::quote(word)).collect();
        Ok(Self {
            kind: Checks::kind(name)?,
            command: words.join(" "),
            inputs: if inputs.is_empty() {
                vec![RepoPath::root()]
            } else {
                inputs
            },
            target_inputs: Vec::new(),
            toolchain,
            external,
        })
    }

    /// `word`, quoted for the shell unless it needs no quotes.
    fn quote(word: &str) -> Cow<'_, str> {
        // Without `=`: a first word `A=1` would assign a variable.
        let plain = |byte: u8| byte.is_ascii_alphanumeric() || b"_-./:,+@%".contains(&byte);
        if !word.is_empty() && word.bytes().all(plain) {
            return Cow::Borrowed(word);
        }
        Cow::Owned(format!("'{}'", word.replace('\'', "'\\''")))
    }

    /// What a result of the check declares it depended on, with the output
    /// of its toolchain command, if it has one.
    #[must_use]
    pub fn inputs(&self, toolchain: &[u8]) -> Inputs {
        Inputs {
            paths: self.inputs.clone(),
            target_paths: self.target_inputs.clone(),
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
                let kind = Self::kind(&name)?;
                let check = Check {
                    kind: kind.clone(),
                    command: check.command,
                    inputs: check.inputs.unwrap_or_else(|| vec![RepoPath::root()]),
                    target_inputs: Vec::new(),
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
    use std::slice;

    use super::*;

    const LINT_AND_LINKS: &str = "[checks.lint]\ncommand = \"cargo clippy\"\n\
        inputs = [\"src\", \"Cargo.toml\"]\ntoolchain = \"rustc -V\"\n\
        [checks.links]\ncommand = \"lychee docs\"\nexternal = true\n";

    #[test]
    fn parses_checks_by_kind() {
        let checks = Checks::parse(LINT_AND_LINKS, Defined::InSnapshot).unwrap();
        let lint = checks.get(&Kind::new("ci.lint").unwrap()).unwrap();
        assert_eq!(
            lint.inputs,
            [
                RepoPath::new("src").unwrap(),
                RepoPath::new("Cargo.toml").unwrap(),
                RepoPath::new("checks.toml").unwrap(),
            ],
            "the checks are an input of each"
        );
        assert_eq!(lint.target_inputs, []);
        assert_eq!(lint.toolchain.as_deref(), Some("rustc -V"));
        let links = checks.get(&Kind::new("ci.links").unwrap()).unwrap();
        assert_eq!(Checks::kind("links").unwrap(), links.kind);
        assert_eq!(links.inputs, [RepoPath::root()], "the whole snapshot");
        assert!(links.external && links.inputs(b"").external);
        assert_eq!(checks.iter().count(), 2);

        let parse = |text| Checks::parse(text, Defined::InSnapshot);
        assert!(
            parse("[checks.Lint]\ncommand = \"x\"\n").is_err(),
            "not a kind"
        );
        assert!(
            parse("[checks.lint]\ncmd = \"x\"\n").is_err(),
            "unknown key"
        );
        assert_eq!(parse("").unwrap(), Checks::default());
    }

    #[test]
    fn checks_in_force_on_a_target_read_it_there() {
        let checks = Checks::parse(LINT_AND_LINKS, Defined::OnTarget).unwrap();
        let file = RepoPath::new(Checks::FILE).unwrap();
        for check in checks.iter() {
            assert_eq!(check.target_inputs, slice::from_ref(&file), "{check:?}");
            assert!(!check.inputs.contains(&file), "{check:?}");
        }
        let lint = checks.get(&Kind::new("ci.lint").unwrap()).unwrap();
        let in_snapshot = Checks::parse(LINT_AND_LINKS, Defined::InSnapshot).unwrap();
        let ran = lint.inputs(b"rustc 1.99.0");
        assert_eq!(ran.target_paths, [file]);
        assert_ne!(
            ran,
            in_snapshot.get(&lint.kind).unwrap().inputs(b"rustc 1.99.0")
        );
    }

    #[test]
    fn inputs_follow_the_command_and_toolchain() {
        let check = |command: &str| Check {
            kind: Kind::new("ci.test").unwrap(),
            command: command.to_owned(),
            inputs: vec![RepoPath::new("src").unwrap()],
            target_inputs: Vec::new(),
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

    #[test]
    fn adhoc_checks_quote_their_words() {
        let words = ["A=1", "printf", "%s\\n", "a b", "it's", ""].map(String::from);
        let check = Check::adhoc("say", &words, Vec::new(), None, false).unwrap();
        assert_eq!(check.kind, Checks::kind("say").unwrap());
        assert_eq!(check.command, r"'A=1' printf '%s\n' 'a b' 'it'\''s' ''");
        assert_eq!(check.inputs, [RepoPath::root()], "the whole snapshot");
        assert_eq!(check.target_inputs, [], "no checks.toml");
    }
}
