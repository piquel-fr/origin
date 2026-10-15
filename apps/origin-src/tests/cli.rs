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
    // Created empty, its draft recorded, described: the first change has
    // three revisions, of which the newest is its head.
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
    assert!(repo.ok(&["show"]).ends_with(&diff));
}

#[test]
fn only_commands_that_build_on_the_working_copy_record_it() {
    let repo = Repo::new();
    let change = repo.change();
    let revisions = || repo.ok(&["evolog", &change]).lines().count() - 1;
    repo.write("file", "draft\n");

    // Reading the working copy shows the draft, and records nothing.
    assert!(repo.ok(&["status"]).ends_with("A file\n"));
    assert!(repo.ok(&["diff"]).contains("+draft\n"));
    assert!(repo.ok(&["show"]).contains(", with unrecorded edits\n"));
    assert_eq!(revisions(), 1);

    // Starting the next change records it first.
    repo.ok(&["new"]);
    assert_eq!(revisions(), 2);
    assert!(repo.ok(&["show", &change]).contains("+draft\n"));
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
    repo.ok(&["describe", "Shared work"]);
    fs::write(&pointer, started).unwrap();
    repo.write("file", "right\n");
    repo.ok(&["describe", "Shared work"]);

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

    // It doesn't land until the file changes: describing it keeps the
    // conflict, an edit resolves it, whatever the file then holds.
    repo.ok(&["describe", "Shared work, converged"]);
    let refused = repo.fails(&["land"]);
    assert!(refused.contains("unresolved conflicts"), "{refused}");
    assert!(refused.contains("edit file"), "{refused}");
    repo.write("file", "Heading\n=======\nboth\n");
    assert!(!repo.ok(&["status"]).contains("Conflicts"));
    assert!(repo.ok(&["land"]).starts_with("Landed change"));
    assert!(
        repo.fails(&["converge", &change])
            .contains("is not divergent")
    );
}

#[test]
fn binary_conflicts_resolve_only_when_accepted() {
    let repo = Repo::new();
    repo.write("image", "base\0");
    repo.write("text", "base\n");
    repo.ok(&["describe", "Shared work"]);
    let change = repo.change();
    let pointer = repo.path(".origin/workspace");
    let started = fs::read_to_string(&pointer).unwrap();
    repo.write("image", "left\0");
    repo.write("text", "left\n");
    repo.ok(&["describe", "Shared work"]);
    fs::write(&pointer, started).unwrap();
    repo.write("image", "right\0");
    repo.write("text", "right\n");
    repo.ok(&["converge", &change]);
    assert_eq!(repo.read("image"), "right\0", "the working copy's side");

    // An edit elsewhere leaves it unresolved, even without markers.
    repo.write("unrelated", "docs\n");
    assert!(repo.fails(&["land"]).contains("unresolved conflicts"));
    let resolved = repo.ok(&["resolve", "image", "unrelated"]);
    assert!(resolved.starts_with("Resolved image\n"), "{resolved}");
    assert!(resolved.contains("Conflicts: text hold"), "{resolved}");
    let status = repo.ok(&["status"]);
    assert!(status.contains("\nConflicts: text hold"), "{status}");
    repo.ok(&["resolve", "text"]);
    assert!(repo.ok(&["land"]).starts_with("Landed change"));
}

#[test]
fn a_revision_is_published_only_after_its_conflicts() {
    use origin_shared::{EncodedObject, Record, RecordId, Revision, RevisionId};
    use origin_src::{annotations::Annotations, conflict::Conflicts, store::Store};

    let repo = Repo::new();
    repo.write("file", "base\n");
    repo.ok(&["describe", "Shared work"]);
    let change = repo.change();
    let pointer = repo.path(".origin/workspace");
    let started = fs::read_to_string(&pointer).unwrap();
    repo.write("file", "left\n");
    repo.ok(&["describe", "Shared work"]);
    fs::write(&pointer, started).unwrap();
    repo.write("file", "right\n");
    repo.ok(&["converge", &change]);

    // The `core.conflict` statement `describe` issues about its revision.
    let store = Store::new(repo.path(".origin"), origin_platform::Native::default());
    let converged: RevisionId = fs::read_to_string(&pointer)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let revision: Revision = store.get(converged).unwrap();
    let claims = Annotations::open(&store)
        .unwrap()
        .claims(RecordId::Revision(converged))
        .unwrap();
    let conflicts = Conflicts::of(&store, &claims, &revision).unwrap();
    let issuer = revision.author.clone();
    let described = Revision {
        predecessors: vec![converged],
        description: EncodedObject::blob(b"Described").id().into(),
        ..revision
    }
    .encode();
    let subject = RecordId::Revision(described.id().into());
    let (_, statement) = conflicts.statement(subject, issuer, None);
    let hex = statement.encode().id().to_hex();

    // Writing it fails: its directory is read-only.
    let dir = repo.path(&format!(".origin/objects/loose/{}", &hex[..2]));
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    repo.fails(&["describe", "Described"]);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

    // So the revision, which would hide the conflict, wasn't written.
    assert!(!store.contains(described.id()).unwrap());
    let refused = repo.fails(&["land"]);
    assert!(refused.contains("unresolved conflicts"), "{refused}");
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
    repo.ok(&["new"]);
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
        repo.ok(&["describe", "Shared work"]);
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
