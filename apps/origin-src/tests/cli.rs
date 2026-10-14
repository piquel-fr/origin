//! End-to-end tests of the local workflow, driving the real `origin` binary.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// Runs `origin` in `cwd` as a configured user.
fn origin(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_origin"))
        .args(args)
        .current_dir(cwd)
        .env("ORIGIN_USER_NAME", "Ada Lovelace")
        .env("ORIGIN_USER_EMAIL", "ada@example.com")
        .output()
        .unwrap()
}

/// A temporary working copy.
struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Self {
        let repo = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        repo.ok(&["init"]);
        repo
    }

    /// Runs a command that must succeed, returning its output.
    fn ok(&self, args: &[&str]) -> String {
        let output = origin(self.dir.path(), args);
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
        let output = origin(self.dir.path(), args);
        assert!(
            !output.status.success(),
            "`origin {}` succeeded",
            args.join(" ")
        );
        String::from_utf8(output.stderr).unwrap()
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn remove(&self, path: &str) {
        fs::remove_file(self.dir.path().join(path)).unwrap();
    }
}

#[test]
fn commits_track_every_change() {
    let repo = Repo::new();
    assert_eq!(repo.ok(&["status"]), "On branch main\nNo changes.\n");

    repo.write("README.md", "hello\n");
    repo.write("src/lib.rs", "fn a() {}\n");
    repo.write("src/old.rs", "old\n");
    assert_eq!(
        repo.ok(&["status"]),
        "On branch main\nA README.md\nA src/lib.rs\nA src/old.rs\n"
    );
    assert!(repo.ok(&["commit", "-m", "First"]).contains("] First"));
    assert_eq!(repo.ok(&["status"]), "On branch main\nNo changes.\n");

    repo.write("README.md", "hello, world\n");
    repo.remove("src/old.rs");
    repo.write("src/new.rs", "new\n");
    assert_eq!(
        repo.ok(&["status"]),
        "On branch main\nM README.md\nA src/new.rs\nD src/old.rs\n"
    );
    repo.ok(&["commit", "-m", "Second\n\nWith details."]);

    let log = repo.ok(&["log"]);
    let messages: Vec<&str> = log
        .lines()
        .filter_map(|line| line.strip_prefix("    "))
        .collect();
    assert_eq!(messages, ["Second", "", "With details.", "First"]);
    assert!(log.contains("Ada Lovelace"));
    assert_eq!(repo.ok(&["log", "-n", "1"]).lines().count(), 4);
}

#[test]
fn diff_shows_line_changes() {
    let repo = Repo::new();
    repo.write("file.txt", "one\ntwo\nthree\n");
    repo.ok(&["commit", "-m", "Add file"]);
    repo.write("file.txt", "one\n2\nthree\n");
    repo.write("binary", "\0\u{1}");

    let diff = repo.ok(&["diff"]);
    assert!(diff.contains("Binary file binary differs\n"), "{diff}");
    assert!(diff.contains("--- a/file.txt\n+++ b/file.txt\n"), "{diff}");
    assert!(diff.contains("-two\n+2\n"), "{diff}");
}

#[test]
fn committing_nothing_fails() {
    let repo = Repo::new();
    assert!(
        repo.fails(&["commit", "-m", "Empty"])
            .contains("nothing to commit")
    );
}

#[test]
fn commands_find_the_repository_from_subdirectories() {
    let repo = Repo::new();
    repo.write("deep/nested/file", "content");
    let output = origin(&repo.dir.path().join("deep/nested"), &["status"]);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "On branch main\nA deep/nested/file\n"
    );
}

#[test]
fn commands_outside_a_repository_fail() {
    let dir = tempfile::tempdir().unwrap();
    let output = origin(dir.path(), &["status"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not in an origin repository"));
}
