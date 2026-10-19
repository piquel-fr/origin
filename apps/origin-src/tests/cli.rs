//! End-to-end tests of the change workflow, driving the real `origin` binary.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

/// A temporary working copy, and a user configuration of its own.
struct Repo {
    dir: tempfile::TempDir,
    config: tempfile::TempDir,
}

impl Repo {
    fn new() -> Self {
        let config = tempfile::tempdir().unwrap();
        fs::write(config.path().join("config.toml"), "user = \"ada\"\n").unwrap();
        let repo = Self {
            dir: tempfile::tempdir().unwrap(),
            config,
        };
        repo.ok(&["init"]);
        repo
    }

    /// Runs `origin` in `cwd` as the configured user.
    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_origin"))
            .args(args)
            .current_dir(cwd)
            .env("ORIGIN_CONFIG_DIR", self.config.path())
            .output()
            .unwrap()
    }

    /// Runs a command that must succeed, returning its output.
    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(self.dir.path(), args);
        assert!(
            output.status.success(),
            "`origin {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// Runs a command that must fail, returning its error.
    fn fails(&self, args: &[&str]) -> String {
        let output = self.run(self.dir.path(), args);
        assert!(
            !output.status.success(),
            "`origin {}` succeeded",
            args.join(" ")
        );
        String::from_utf8(output.stderr).unwrap()
    }

    fn path(&self, path: &str) -> std::path::PathBuf {
        self.dir.path().join(path)
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.path(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.path(path)).unwrap()
    }

    /// The short id of the working copy's change.
    fn change(&self) -> String {
        let status = self.ok(&["status"]);
        let line = status.lines().next().unwrap();
        let headline = line.strip_prefix("Working copy: ").unwrap();
        headline.split(' ').next().unwrap().to_owned()
    }
}

#[test]
fn the_working_copy_is_a_revision_of_the_current_change() {
    let repo = Repo::new();
    let first = repo.change();
    assert_eq!(
        repo.ok(&["status"]),
        format!("Working copy: {first} (no description)\nOn top of: main #0\nNo changes.\n")
    );

    repo.write("README.md", "hello\n");
    repo.write("src/lib.rs", "fn a() {}\n");
    repo.write("src/old.rs", "old\n");
    repo.ok(&["describe", "Add the library"]);
    assert_eq!(
        repo.ok(&["status"]),
        format!(
            "Working copy: {first} Add the library\nOn top of: main #0\n\
             A README.md\nA src/lib.rs\nA src/old.rs\n"
        )
    );

    let new = repo.ok(&["new", "Rework it"]);
    let second = repo.change();
    assert_ne!(second, first);
    assert_eq!(
        new,
        format!("Working copy: {second} Rework it\nOn top of: {first} Add the library\n")
    );
    repo.write("README.md", "hello, world\n");
    fs::remove_file(repo.path("src/old.rs")).unwrap();
    repo.write("src/new.rs", "new\n");
    assert!(
        repo.ok(&["status"])
            .ends_with("M README.md\nA src/new.rs\nD src/old.rs\n")
    );

    let log = repo.ok(&["log"]);
    assert!(
        log.starts_with(&format!(
            "@ {second} Rework it\n○ {first} Add the library\n◆ main #0 "
        )),
        "{log}"
    );
    // Created empty, snapshotted, described: the first change has three
    // revisions, of which the newest is its head.
    let evolog = repo.ok(&["evolog", &first]);
    let revisions: Vec<_> = evolog.lines().skip(1).collect();
    assert_eq!(revisions.len(), 3, "{evolog}");
    assert!(
        revisions[0].ends_with(" Add the library (head)"),
        "{evolog}"
    );
    assert!(revisions[2].ends_with(" (no description)"), "{evolog}");
}

#[test]
fn diff_shows_line_changes() {
    let repo = Repo::new();
    repo.write("file.txt", "one\ntwo\nthree\n");
    repo.ok(&["new"]);
    repo.write("file.txt", "one\n2\nthree\n");
    repo.write("binary", "\0\u{1}");

    let diff = repo.ok(&["diff"]);
    assert!(diff.contains("Binary file binary differs\n"), "{diff}");
    assert!(diff.contains("--- a/file.txt\n+++ b/file.txt\n"), "{diff}");
    assert!(diff.contains("-two\n+2\n"), "{diff}");
    assert!(repo.ok(&["show", "@"]).ends_with(&diff));
}

#[test]
fn edit_switches_the_files_to_another_change() {
    let repo = Repo::new();
    repo.write("shared", "first\n");
    repo.write("run.sh", "#!/bin/sh\n");
    fs::set_permissions(repo.path("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    let first = repo.change();
    repo.ok(&["new"]);
    let second = repo.change();
    repo.write("shared", "second\n");
    repo.write("dir/added", "added\n");
    fs::remove_file(repo.path("run.sh")).unwrap();

    // A unique prefix is enough.
    repo.ok(&["edit", &first[..6]]);
    assert_eq!(repo.read("shared"), "first\n");
    assert!(!repo.path("dir").exists());
    let mode = fs::metadata(repo.path("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111, "still executable");
    assert!(repo.ok(&["status"]).ends_with("A run.sh\nA shared\n"));

    repo.ok(&["edit", &second]);
    assert_eq!(repo.read("dir/added"), "added\n");
    assert!(!repo.path("run.sh").exists());
    assert!(
        repo.ok(&["status"])
            .ends_with("A dir/added\nD run.sh\nM shared\n")
    );
    assert!(repo.fails(&["edit", "zz"]).contains("is not a change id"));
}

#[test]
fn divergence_shows_until_converged() {
    let repo = Repo::new();
    repo.write("file", "base\n");
    repo.ok(&["describe", "Shared work"]);
    let change = repo.change();
    // Two agents revise the same revision: one, then the other from the
    // revision the first started from.
    let pointer = repo.path(".origin/workspace");
    let started = fs::read_to_string(&pointer).unwrap();
    repo.write("file", "left\n");
    repo.ok(&["status"]);
    fs::write(&pointer, started).unwrap();
    repo.write("file", "right\n");

    let status = repo.ok(&["status"]);
    assert!(
        status.starts_with(&format!("Working copy: {change} Shared work (divergent)\n")),
        "{status}"
    );
    assert!(
        status.contains(&format!("Run `origin converge {change}`")),
        "{status}"
    );
    assert!(
        repo.ok(&["log"])
            .starts_with(&format!("@ {change} Shared work (divergent)\n"))
    );
    assert!(repo.fails(&["edit", &change]).contains("is divergent"));
    assert_eq!(repo.ok(&["show", &change]).matches("Revision ").count(), 2);
    // The divergence is flagged once, about one of the heads.
    let evolog = repo.ok(&["evolog", &change]);
    let flagged: usize = evolog
        .lines()
        .filter(|line| line.ends_with(" (head)"))
        .map(|line| {
            let revision = line.split(' ').nth(1).unwrap();
            repo.ok(&["statements", revision])
                .matches(" core.divergent by node:ada")
                .count()
        })
        .sum();
    assert_eq!(flagged, 1, "{evolog}");

    let converged = repo.ok(&["converge", &change]);
    assert!(
        converged.starts_with(&format!("Converged change {change}")),
        "{converged}"
    );
    assert_eq!(
        repo.read("file"),
        "right\n",
        "the working copy's files are kept"
    );
    assert!(!repo.ok(&["status"]).contains("divergent"));
    assert!(repo.ok(&["evolog"]).contains("    converges "));
    assert!(
        repo.fails(&["converge", &change])
            .contains("is not divergent")
    );
}

#[test]
fn abandoning_the_current_change_restores_its_base() {
    let repo = Repo::new();
    repo.write("kept", "kept\n");
    repo.ok(&["new", "Try a cache"]);
    let abandoned = repo.change();
    repo.write("kept", "changed\n");
    repo.write("cache.rs", "cache\n");

    let output = repo.ok(&["abandon", "--reason", "Too slow"]);
    assert!(
        output.starts_with(&format!("Abandoned change {abandoned}\n")),
        "{output}"
    );
    assert_eq!(repo.read("kept"), "kept\n");
    assert!(!repo.path("cache.rs").exists());
    assert_ne!(repo.change(), abandoned);
    assert!(repo.ok(&["status"]).ends_with("No changes.\n"));

    let show = repo.ok(&["show", &abandoned]);
    assert!(show.contains(" (abandoned)\n"), "{show}");
    assert!(show.contains("Abandoned by user:ada: Too slow\n"), "{show}");
    assert!(show.contains("+cache\n"), "the attempt is kept: {show}");
}

#[test]
fn only_those_who_worked_on_a_change_abandon_it() {
    let repo = Repo::new();
    repo.ok(&["new", "Ada's change"]);
    let change = repo.change();
    repo.ok(&["new", "Next"]);
    let config = repo.config.path().join("config.toml");
    fs::write(&config, "user = \"eve\"\n").unwrap();
    repo.ok(&["abandon", &change, "--reason", "Not mine"]);
    fs::write(&config, "user = \"ada\"\n").unwrap();

    let show = repo.ok(&["show", &change]);
    assert!(!show.contains("abandoned"), "{show}");
    assert!(!show.contains("Abandoned by"), "{show}");
}

#[test]
fn init_creates_policy_and_main_with_one_node_key() {
    let repo = Repo::new();
    let policy = repo.ok(&["log", "policy"]);
    assert!(policy.starts_with("◆ policy #0 "), "{policy}");
    assert_eq!(policy.lines().count(), 1, "the stack is on main: {policy}");
    assert!(
        repo.fails(&["log", "nope"])
            .contains("no target is named `nope`")
    );
    assert!(repo.fails(&["init"]).contains("already has a repository"));

    let key = repo.config.path().join("node.key");
    let mode = fs::metadata(&key).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let before = fs::read(&key).unwrap();
    let other = tempfile::tempdir().unwrap();
    assert!(repo.run(other.path(), &["init"]).status.success());
    assert_eq!(
        fs::read(&key).unwrap(),
        before,
        "every repository shares it"
    );
}

#[test]
fn a_lost_index_is_rebuilt() {
    let repo = Repo::new();
    repo.write("file", "content\n");
    let change = repo.change();
    fs::remove_file(repo.path(".origin/index/log")).unwrap();
    assert_eq!(repo.ok(&["evolog", &change]).lines().count(), 3);
    assert!(
        repo.ok(&["index", "rebuild"])
            .starts_with("Rebuilt the index of ")
    );
}

#[test]
fn commands_find_the_repository_from_subdirectories() {
    let repo = Repo::new();
    repo.write("deep/nested/file", "content");
    let output = repo.run(&repo.path("deep/nested"), &["status"]);
    let status = String::from_utf8(output.stdout).unwrap();
    assert!(status.ends_with("A deep/nested/file\n"), "{status}");
}

#[test]
fn commands_outside_a_repository_fail() {
    let repo = Repo::new();
    let outside = tempfile::tempdir().unwrap();
    let output = repo.run(outside.path(), &["status"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not in an origin repository"));
}

#[test]
fn a_closed_output_pipe_ends_quietly() {
    let repo = Repo::new();
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let output = Command::new(env!("CARGO_BIN_EXE_origin"))
        .arg("status")
        .current_dir(repo.dir.path())
        .env("ORIGIN_CONFIG_DIR", repo.config.path())
        .stdout(writer)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

#[test]
fn statements_are_promoted_where_their_inputs_are_unchanged() {
    let repo = Repo::new();
    repo.write("src/lib.rs", "fn a() {}\n");
    repo.write("docs/guide.md", "# Guide\n");
    repo.write("lint.log", "all clean\n");
    let lint = repo.ok(&[
        "annotate",
        "@",
        "ci.lint",
        "--pass",
        "--inputs",
        "src",
        "--definition",
        "cargo clippy",
        "--payload",
        "lint.log",
    ]);
    assert!(lint.contains(": ci.lint pass about revision "), "{lint}");
    assert!(lint.contains("\nInput digest: "), "{lint}");
    repo.ok(&[
        "annotate",
        "@",
        "docs.test",
        "--fail",
        "--inputs",
        "src",
        "docs",
    ]);
    let rerun = repo.ok(&[
        "annotate",
        "@",
        "docs.test",
        "--pass",
        "--inputs",
        "src",
        "docs",
    ]);
    assert!(rerun.contains("\nSupersedes statement "), "{rerun}");
    repo.ok(&[
        "annotate",
        "@",
        "links",
        "--pass",
        "--inputs",
        "docs",
        "--external",
    ]);
    repo.ok(&["annotate", "@", "review", "--neutral"]);

    let statements = repo.ok(&["statements"]);
    assert_eq!(
        statements.matches(" by user:ada").count(),
        5,
        "{statements}"
    );
    assert!(
        statements.contains("docs.test fail by user:ada (superseded by "),
        "{statements}"
    );
    assert!(
        statements.contains("    inputs: docs, and external\n"),
        "{statements}"
    );
    assert!(
        statements.contains("    payload: all clean\n"),
        "{statements}"
    );

    // A docs-only change keeps the lint result.
    repo.ok(&["new", "Document more"]);
    repo.write("docs/guide.md", "# Guide\nMore.\n");
    let promoted = repo.ok(&["promote"]);
    assert!(
        promoted.starts_with("Promoted ci.lint pass by user:ada as statement "),
        "{promoted}"
    );
    assert_eq!(promoted.lines().count(), 1, "{promoted}");
    assert!(repo.ok(&["promote"]).starts_with("Nothing to promote"));
    let statements = repo.ok(&["statements", "@"]);
    assert!(
        statements.contains(" ci.lint pass by node:ada\n"),
        "{statements}"
    );
    assert!(
        statements.contains("    promoted from statement "),
        "{statements}"
    );
    assert!(
        statements.contains("    payload: all clean\n"),
        "{statements}"
    );

    assert!(
        repo.fails(&["annotate", "@", "core.conflict", "--fail"])
            .contains("is a reserved kind")
    );
    assert!(repo.fails(&["annotate", "@", "ci.lint"]).contains("--pass"));
    assert!(
        repo.ok(&["gc"])
            .starts_with("Dropped 0 payloads past their retention")
    );
}
