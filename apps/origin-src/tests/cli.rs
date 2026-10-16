//! End-to-end tests of the change workflow and workspaces, driving the real
//! `origin` binary.

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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

    /// Starts a long-running command, such as `watch`, returning it and its
    /// output lines as they come.
    fn spawn(&self, args: &[&str]) -> (Child, mpsc::Receiver<String>) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_origin"))
            .args(args)
            .current_dir(self.dir.path())
            .env("ORIGIN_CONFIG_DIR", self.config.path())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            for line in stdout.lines() {
                if send.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        (child, receive)
    }

    /// The revision the working copy is, in full.
    fn revision(&self) -> String {
        self.read(".origin/workspace").trim_end().to_owned()
    }

    /// Runs a command that must succeed, returning its output.
    fn ok(&self, args: &[&str]) -> String {
        self.ok_in(self.dir.path(), args)
    }

    /// Runs a command in `cwd` that must succeed, returning its output.
    fn ok_in(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.run(cwd, args);
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

    /// Creates a workspace at `path`, returning its change.
    fn workspace(&self, path: &Path, args: &[&str]) -> String {
        let path = path.to_str().unwrap();
        let output = self.ok(&[&["workspace", "new", path], args].concat());
        let line = output.lines().nth(1).unwrap();
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
    assert!(
        converged.contains("Conflicts: file hold both versions"),
        "{converged}"
    );
    // Both versions, the working copy's first, between markers.
    let file = repo.read("file");
    assert!(file.starts_with("<<<<<<< "), "{file}");
    assert!(file.contains("\nright\n=======\nleft\n>>>>>>> "), "{file}");
    let status = repo.ok(&["status"]);
    assert!(!status.contains("divergent"), "{status}");
    assert!(status.contains("Conflicts: file hold"), "{status}");
    assert!(repo.ok(&["evolog"]).contains("    converges "));

    // It doesn't land until a snapshot has no markers left.
    repo.write("file", "right\nstill <<<<<<< here\n=======\n");
    let refused = repo.fails(&["land"]);
    assert!(refused.contains("unresolved conflicts"), "{refused}");
    assert!(refused.contains("markers out of file"), "{refused}");
    repo.write("file", "both\n");
    assert!(!repo.ok(&["status"]).contains("Conflicts"));
    assert!(repo.ok(&["land"]).starts_with("Landed change"));
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

#[test]
fn workspaces_share_the_repository_with_changes_of_their_own() {
    let repo = Repo::new();
    repo.write("shared", "base\n");
    repo.write("dir/run.sh", "#!/bin/sh\n");
    fs::set_permissions(repo.path("dir/run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    repo.ok(&["describe", "Base"]);
    repo.ok(&["new", "Main work"]);
    let others = tempfile::tempdir().unwrap();
    let ws = others.path().join("ws");

    // A new change beside the working copy's, on the same base.
    let change = repo.workspace(&ws, &[]);
    assert_eq!(fs::read_to_string(ws.join("shared")).unwrap(), "base\n");
    let mode = fs::metadata(ws.join("dir/run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111, "still executable");
    fs::write(ws.join("shared"), "agent\n").unwrap();
    repo.ok_in(&ws.join("dir"), &["describe", "Agent work"]);
    let status = repo.ok_in(&ws, &["status"]);
    assert!(
        status.starts_with(&format!("Working copy: {change} Agent work\nOn top of: ")),
        "{status}"
    );
    assert!(status.ends_with("M shared\n"), "{status}");

    // Main sees the workspace's records, and keeps its own files.
    assert_eq!(repo.read("shared"), "base\n");
    assert!(repo.ok(&["show", &change]).contains("+agent\n"));
    let list = repo.ok(&["workspace", "list"]);
    let lines: Vec<_> = list.lines().collect();
    assert_eq!(lines.len(), 2, "{list}");
    assert!(lines[0].starts_with("@ ") && lines[0].ends_with(" Main work"));
    assert!(lines[1].starts_with("  ") && lines[1].ends_with(" Agent work"));

    // Another workspace on the same change.
    let same = others.path().join("same");
    assert_eq!(repo.workspace(&same, &["--change", &change]), change);
    assert_eq!(fs::read_to_string(same.join("shared")).unwrap(), "agent\n");
}

#[test]
fn removing_a_workspace_keeps_its_work() {
    let repo = Repo::new();
    let others = tempfile::tempdir().unwrap();
    let ws = others.path().join("ws");
    let change = repo.workspace(&ws, &[]);
    fs::write(ws.join("unsaved"), "not snapshotted yet\n").unwrap();

    let removed = repo.ok(&["workspace", "rm", ws.to_str().unwrap()]);
    assert!(removed.starts_with(&format!("Kept its working copy: {change} ")));
    assert!(!ws.exists());
    assert_eq!(repo.ok(&["workspace", "list"]).lines().count(), 1);
    assert!(
        repo.ok(&["show", &change])
            .contains("+not snapshotted yet\n")
    );

    assert!(
        repo.fails(&["workspace", "rm", "."])
            .contains("is the main working copy")
    );
    assert!(
        repo.fails(&["workspace", "rm", ws.to_str().unwrap()])
            .contains("is not a workspace of this repository")
    );
    assert!(
        repo.fails(&["workspace", "new", "nested"])
            .contains("is inside a working copy")
    );
    fs::write(others.path().join("file"), "").unwrap();
    assert!(
        repo.fails(&["workspace", "new", others.path().to_str().unwrap()])
            .contains("is not empty")
    );
}

#[test]
fn landing_moves_the_working_copy_onto_the_target() {
    let repo = Repo::new();
    repo.write("a", "a\n");
    repo.ok(&["describe", "Add a"]);
    let first = repo.change();
    repo.ok(&["new", "Add b"]);
    let second = repo.change();
    repo.write("b", "b\n");
    repo.ok(&["new"]);
    let empty = repo.change();

    // The stack lands from the bottom; the empty working copy has nothing
    // to land, and moves onto the new tip.
    let land = repo.ok(&["land"]);
    let lines: Vec<_> = land.lines().collect();
    assert!(
        lines[0].starts_with(&format!("Landed change {first}, revision ")),
        "{land}"
    );
    assert!(lines[0].ends_with(", on main #1"), "{land}");
    assert!(
        lines[1].starts_with(&format!("Landed change {second}, ")),
        "{land}"
    );
    assert_eq!(
        lines[2],
        format!("Landing change {empty} would change nothing on `main`")
    );
    assert!(
        repo.ok(&["status"])
            .contains("On top of: main #2\nNo changes.\n")
    );

    // Landing the working copy's change starts a new one on the tip.
    repo.write("c", "c\n");
    let land = repo.ok(&["land"]);
    assert!(land.contains(" on main #3\nWorking copy: "), "{land}");
    assert_ne!(repo.change(), empty);
    assert!(
        repo.ok(&["status"])
            .ends_with("On top of: main #3\nNo changes.\n")
    );
    assert_eq!(repo.read("c"), "c\n");

    let targets = repo.ok(&["targets"]);
    assert!(targets.starts_with("main #3 "), "{targets}");
    assert!(targets.contains("(node:ada)\npolicy #0 "), "{targets}");
}

#[test]
fn targets_fork_and_land_on_each_other() {
    let repo = Repo::new();
    repo.ok(&["target", "new", "overhaul", "--from", "main"]);
    assert!(
        repo.fails(&["target", "new", "overhaul", "--from", "main"])
            .contains("already named `overhaul`")
    );
    repo.ok(&["new", "--on", "overhaul", "Rework"]);
    repo.write("rework", "new\n");
    let land = repo.ok(&["land"]);
    assert!(
        land.contains(" on overhaul #1\n"),
        "lands where it's made: {land}"
    );

    let land = repo.ok(&["land", "--from", "overhaul", "--onto", "main"]);
    assert_eq!(land, "Landed `overhaul` on main #1\n");
    assert!(repo.ok(&["log", "main"]).starts_with("◆ main #1 "));
    assert!(repo.ok(&["log", "main"]).contains("\n    overhaul #1\n"));
}

#[test]
fn landings_wait_for_the_evidence_policy_requires() {
    let repo = Repo::new();
    repo.ok(&["new", "--on", "policy", "Require tests"]);
    let policy = repo
        .read("policy.toml")
        .replace("# [groups]", "[groups]")
        .replace(
            "# runners = [\"node:ci-1\"]",
            "runners = [\"node:ci-1\", \"user:ada\"]",
        )
        .replace("# require", "require");
    repo.write("policy.toml", &policy);
    assert!(repo.ok(&["land"]).contains(" on policy #1\n"));
    assert!(
        repo.ok(&["targets"])
            .contains("    requires ci.test pass from group:runners\n")
    );

    repo.ok(&["new", "--on", "main", "Feature"]);
    let change = repo.change();
    repo.write("src/lib.rs", "fn a() {}\n");
    let waiting = repo.ok(&["land"]);
    assert!(
        waiting.starts_with(&format!(
            "The landing of change {change} on `main` waits for ci.test pass from group:runners\n"
        )),
        "{waiting}"
    );
    assert_eq!(repo.ok(&["land", "--status"]), waiting);
    repo.ok(&["annotate", "@", "ci.test", "--pass", "--inputs", "src"]);
    assert!(repo.ok(&["land"]).contains(" on main #1\n"));
    assert_eq!(repo.ok(&["land", "--status"]), "No landing waits.\n");

    // Only the authority lands: here, after a policy edit hands `main` to
    // another node.
    repo.ok(&["new", "--on", "policy"]);
    let policy = repo.read("policy.toml").replace(
        "[targets.main]\nauthority = \"node:ada\"",
        "[targets.main]\nauthority = \"node:hosted\"",
    );
    // Another node's key: any valid one will do.
    let key = policy
        .lines()
        .find_map(|line| line.strip_prefix("\"ada\" = "))
        .unwrap();
    repo.write(
        "policy.toml",
        &policy.replace("[nodes]\n", &format!("[nodes]\nhosted = {key}\n")),
    );
    repo.ok(&["land"]);
    repo.ok(&["new", "--on", "main"]);
    repo.write("x", "x\n");
    assert!(
        repo.fails(&["land"])
            .contains("node:hosted is the authority of `main`")
    );
}

#[test]
fn conflicting_landings_are_flagged_and_wait() {
    let repo = Repo::new();
    repo.ok(&["describe", "One"]);
    repo.write("shared", "one\n");
    let one = repo.change();
    repo.ok(&["new", "--on", "main", "Two"]);
    repo.write("shared", "two\n");
    let two = repo.change();
    repo.ok(&["land", &one]);

    let conflict = repo.ok(&["land"]);
    assert!(
        conflict.starts_with(&format!(
            "The landing of change {two} on `main` conflicts in shared\n"
        )),
        "{conflict}"
    );
    assert!(
        repo.ok(&["statements"])
            .contains(" core.conflict by node:ada\n")
    );
    assert_eq!(repo.ok(&["land", "--status"]), conflict);
}

#[test]
fn converge_merges_the_files_of_every_head() {
    let repo = Repo::new();
    for path in ["a", "b", "c"] {
        repo.write(path, "base\n");
    }
    repo.ok(&["describe", "Shared work"]);
    let change = repo.change();
    // Three agents revise the same revision, each changing its own file.
    let pointer = repo.path(".origin/workspace");
    let started = fs::read_to_string(&pointer).unwrap();
    for changed in ["a", "b", "c"] {
        fs::write(&pointer, &started).unwrap();
        for path in ["a", "b", "c"] {
            repo.write(
                path,
                if path == changed {
                    "changed\n"
                } else {
                    "base\n"
                },
            );
        }
        repo.ok(&["status"]);
    }
    // Merging a third head reads the tree merging the first two.
    let converged = repo.ok(&["converge", &change]);
    assert!(
        converged.ends_with(", merging their files\n"),
        "{converged}"
    );
    for path in ["a", "b", "c"] {
        assert_eq!(repo.read(path), "changed\n", "{path}");
    }
}

#[test]
fn commands_in_different_workspaces_run_concurrently() {
    const WORKSPACES: usize = 4;
    const EDITS: usize = 5;
    let repo = Repo::new();
    let others = tempfile::tempdir().unwrap();
    let workspaces: Vec<_> = (0..WORKSPACES)
        .map(|i| {
            let root = others.path().join(format!("ws{i}"));
            let change = repo.workspace(&root, &[]);
            (root, change)
        })
        .collect();

    std::thread::scope(|scope| {
        for (root, _) in &workspaces {
            let repo = &repo;
            scope.spawn(move || {
                for edit in 0..EDITS {
                    fs::write(root.join("file"), edit.to_string()).unwrap();
                    repo.ok_in(root, &["status"]);
                }
            });
        }
    });

    // Each change has its first revision, then one per edit, whichever
    // order the index received them in.
    let revisions = |change: &str| repo.ok(&["evolog", change]).lines().count() - 1;
    for (_, change) in &workspaces {
        assert_eq!(revisions(change), 1 + EDITS);
    }
    repo.ok(&["index", "rebuild"]);
    for (_, change) in &workspaces {
        assert_eq!(revisions(change), 1 + EDITS);
    }
}

#[test]
fn workspaces_land_their_own_changes() {
    let repo = Repo::new();
    repo.write("unlanded", "main's own work\n");
    let others = tempfile::tempdir().unwrap();
    let (one, two) = (others.path().join("one"), others.path().join("two"));
    let first = repo.workspace(&one, &[]);
    let second = repo.workspace(&two, &[]);
    fs::write(one.join("a"), "a\n").unwrap();
    fs::write(two.join("b"), "b\n").unwrap();

    let land = repo.ok_in(&one, &["land"]);
    assert!(
        land.starts_with(&format!("Landed change {first}, revision ")),
        "{land}"
    );
    assert!(land.contains(" on main #1\nWorking copy: "), "{land}");
    // Made on main #0, the second change lands rebased onto main #1: a new
    // revision of it, by the queue.
    let land = repo.ok_in(&two, &["land"]);
    assert!(land.contains(" on main #2\nWorking copy: "), "{land}");
    assert!(repo.ok(&["show", &first]).contains(" by user:ada\n"));
    assert!(repo.ok(&["show", &second]).contains(" by node:ada\n"));

    // Each landing moved its own workspace onto the tip it made.
    let status = repo.ok_in(&two, &["status"]);
    assert!(
        status.ends_with("On top of: main #2\nNo changes.\n"),
        "{status}"
    );
    assert_eq!(fs::read_to_string(two.join("a")).unwrap(), "a\n");
    let status = repo.ok_in(&one, &["status"]);
    assert!(
        status.ends_with("On top of: main #1\nNo changes.\n"),
        "{status}"
    );
    assert!(!one.join("b").exists());
    // The main working copy's change didn't land, so it stays.
    let status = repo.ok(&["status"]);
    assert!(
        status.ends_with("On top of: main #0\nA unlanded\n"),
        "{status}"
    );
    assert!(!repo.path("a").exists());
}

#[test]
fn a_workspace_follows_its_change_once_it_lands_elsewhere() {
    let repo = Repo::new();
    repo.write("shared", "base\n");
    repo.ok(&["describe", "Shared"]);
    let change = repo.change();
    let others = tempfile::tempdir().unwrap();
    let (clean, conflicting) = (
        others.path().join("clean"),
        others.path().join("conflicting"),
    );
    repo.workspace(&clean, &["--change", &change]);
    repo.workspace(&conflicting, &["--change", &change]);
    let pointer = clean.join(".origin/workspace");
    let before = fs::read_to_string(&pointer).unwrap();

    // The change lands from the main working copy, then main moves on.
    assert!(repo.ok(&["land"]).contains(" on main #1\n"));
    repo.write("shared", "main\n");
    assert!(repo.ok(&["land"]).contains(" on main #2\n"));
    assert_eq!(fs::read_to_string(&pointer).unwrap(), before, "left alone");

    // Its next command starts a new change on the tip, with its edits.
    fs::write(clean.join("notes"), "mine\n").unwrap();
    let status = repo.ok_in(&clean, &["status"]);
    assert!(
        status.starts_with(&format!(
            "Change {change} landed on main #1\nWorking copy: "
        )),
        "{status}"
    );
    assert!(
        status.ends_with("On top of: main #2\nA notes\n"),
        "{status}"
    );
    assert_eq!(fs::read_to_string(clean.join("shared")).unwrap(), "main\n");

    // Edits that conflict with the tip stay a revision of the change.
    fs::write(conflicting.join("shared"), "mine\n").unwrap();
    let status = repo.ok_in(&conflicting, &["status"]);
    assert!(
        status.contains(
            "The working copy stays on it: its edits conflict with `main` in shared\n\
             Working copy: "
        ),
        "{status}"
    );
    assert!(
        status.contains(&format!("Working copy: {change} Shared\n")),
        "{status}"
    );
    assert_eq!(
        fs::read_to_string(conflicting.join("shared")).unwrap(),
        "mine\n"
    );
}

#[test]
fn undo_moves_the_working_copy_back_one_operation_at_a_time() {
    let repo = Repo::new();
    let first = repo.change();
    repo.write("a", "a\n");
    repo.ok(&["describe", "Add a"]);
    repo.ok(&["new", "Second"]);
    assert_ne!(repo.change(), first);

    let undone = repo.ok(&["undo"]);
    assert!(undone.starts_with("Undid #"), "{undone}");
    assert!(undone.contains(": origin new Second\n"), "{undone}");
    assert_eq!(repo.change(), first);

    // Describing makes a newer revision of the change: undoing it restores
    // the description in another one, so the change keeps a single head.
    repo.ok(&["describe", "Renamed"]);
    repo.ok(&["undo"]);
    let status = repo.ok(&["status"]);
    assert!(
        status.starts_with(&format!("Working copy: {first} Add a\n")),
        "{status}"
    );
    assert!(!repo.ok(&["evolog", &first]).contains("divergent"));

    // Then the first description, back to the edits it was made on, and
    // then nothing is left but creating the repository.
    repo.ok(&["undo"]);
    let status = repo.ok(&["status"]);
    assert!(status.starts_with(&format!("Working copy: {first} (no description)\n")));
    assert!(status.ends_with("A a\n"), "the edits stay: {status}");
    assert!(
        repo.fails(&["undo"])
            .contains("nothing to undo but the creation")
    );
}

#[test]
fn undo_moves_only_its_own_workspace() {
    let repo = Repo::new();
    let others = tempfile::tempdir().unwrap();
    let ws = others.path().join("ws");
    repo.workspace(&ws, &[]);
    repo.ok_in(&ws, &["new", "Agent work"]);
    repo.ok(&["new", "Main work"]);
    let main = repo.change();

    // The latest operation is main's, but the workspace undoes its own.
    let agent = repo.ok_in(&ws, &["undo"]);
    assert!(agent.contains(": origin new Agent work\n"), "{agent}");
    assert_eq!(repo.change(), main);

    let log = repo.ok(&["op", "log"]);
    let headers: Vec<_> = log
        .lines()
        .filter(|line| !line.starts_with("    "))
        .collect();
    assert!(
        headers[0].starts_with("○ #") && headers[0].ends_with(": origin undo"),
        "{log}"
    );
    assert!(headers[1].starts_with("@ #") && headers[1].ends_with(": origin new Main work"));
    assert!(headers[1].contains(&repo.dir.path().display().to_string()));
    assert!(log.contains("    undid #"), "{log}");
    assert_eq!(
        repo.ok(&["op", "log", "-n", "1"]).lines().count(),
        2,
        "the undo, and its move"
    );
}

#[test]
fn a_move_cut_short_is_finished_by_the_next_command() {
    let repo = Repo::new();
    repo.write("a", "a\n");
    repo.ok(&["describe", "First"]);
    let first = repo.revision();
    repo.ok(&["new", "Second"]);
    repo.write("b", "b\n");
    repo.write("a", "changed\n");
    repo.ok(&["status"]);

    // A crash in the middle of `edit` back to the first: some files checked
    // out, others not.
    fs::write(repo.path(".origin/moving"), format!("{first} command\n")).unwrap();
    fs::remove_file(repo.path("b")).unwrap();
    let status = repo.ok(&["status"]);
    assert!(
        status.starts_with("Finished moving the working copy to "),
        "{status}"
    );
    assert_eq!(repo.revision(), first);
    assert_eq!(repo.read("a"), "a\n");
    assert!(!repo.path("b").exists());
    assert!(!repo.path(".origin/moving").exists());
}

#[test]
fn watch_prints_events_from_a_cursor_or_as_they_happen() {
    let repo = Repo::new();
    let line =
        |receive: &mpsc::Receiver<String>| receive.recv_timeout(Duration::from_secs(20)).unwrap();
    // From the start: the targets `init` created.
    let (mut all, receive) = repo.spawn(&["watch", "--since", "0"]);
    let policy = line(&receive);
    assert!(
        policy.contains(" target.advanced target=policy seq=0 "),
        "{policy}"
    );
    all.kill().unwrap();

    let (mut checks, receive) = repo.spawn(&["watch", "--kind", "ci.*"]);
    // Let it read where the log ends before anything happens.
    thread::sleep(Duration::from_millis(500));
    repo.ok(&["describe", "Work"]);
    repo.ok(&["annotate", "@", "ci.test", "--pass"]);
    let issued = line(&receive);
    checks.kill().unwrap();
    let fields: Vec<_> = issued.split(' ').collect();
    assert!(fields[0].parse::<u64>().is_ok(), "a cursor first: {issued}");
    assert_eq!(fields[1], "statement.issued");
    assert!(
        issued.contains(" kind=ci.test verdict=pass issuer=user:ada"),
        "{issued}"
    );
}
