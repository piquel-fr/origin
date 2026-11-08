//! End-to-end tests of the change workflow, driving the real `origin` binary.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::slice;

use origin_platform::Native;
use origin_shared::{
    ChangeId, EncodedObject, Kind, Record, Revision, RevisionId, Statement, StatementId, Verdict,
};
use origin_src::store::Store;

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

/// The working copy's revision, in full.
fn revision_of(repo: &Repo) -> RevisionId {
    repo.read(".origin/workspace").trim().parse().unwrap()
}

#[test]
fn at_names_the_working_copy_s_own_revision() {
    let repo = Repo::new();
    repo.write("file", "base\n");
    repo.ok(&["describe", "Shared work"]);
    // Another revision of the change, then the working copy back on the
    // one before: the change diverges, and `@` is still the working copy.
    let pointer = repo.path(".origin/workspace");
    let started = fs::read_to_string(&pointer).unwrap();
    repo.ok(&["describe", "Elsewhere"]);
    fs::write(&pointer, &started).unwrap();
    repo.write("file", "here\n");
    repo.ok(&["describe", "Shared work"]);
    let ours = revision_of(&repo);
    let issued = repo.ok(&["annotate", "@", "review", "--pass"]);
    assert!(
        issued.contains(&format!("about revision {}", ours.short())),
        "{issued}"
    );
}

#[test]
fn a_prefix_naming_a_change_and_another_revision_is_ambiguous() {
    let repo = Repo::new();
    repo.write("file", "file\n");
    repo.ok(&["describe", "Ours"]);
    let ours = revision_of(&repo);
    // Another change, whose id starts as that revision's does.
    let store = Store::new(repo.path(".origin"), Native::default());
    let mut change = [0; 16];
    change[..4].copy_from_slice(&ours.as_bytes()[..4]);
    let revision: Revision = store.get(ours).unwrap();
    let other = Revision {
        change: ChangeId::from_bytes(change),
        predecessors: Vec::new(),
        ..revision
    }
    .encode();
    store.write_all(slice::from_ref(&other)).unwrap();

    let hex = ours.to_string();
    let refused = repo.fails(&["statements", &hex[..8]]);
    assert!(refused.contains("names both change"), "{refused}");
    assert!(repo.ok(&["statements", &hex[..12]]).contains(&ours.short()));
}

#[test]
fn a_promotion_lists_without_its_original() {
    let repo = Repo::new();
    repo.ok(&["describe", "Ours"]);
    let missing = StatementId::from(EncodedObject::blob(b"elsewhere").id());
    let promotion = Statement {
        subject: origin_shared::RecordId::Revision(revision_of(&repo)),
        kind: Kind::new("ci.test").unwrap(),
        verdict: Some(Verdict::Pass),
        issuer: "node:ada".parse().unwrap(),
        inputs: None,
        payload: None,
        supersedes: None,
        promoted_from: Some(missing),
    }
    .encode();
    let store = Store::new(repo.path(".origin"), Native::default());
    store.write_all(slice::from_ref(&promotion)).unwrap();
    let listed = repo.ok(&["statements"]);
    assert!(
        listed.contains(&format!(
            "promoted from statement {} by an issuer not in this repository",
            missing.short()
        )),
        "{listed}"
    );
}

#[test]
fn edits_recorded_on_a_stale_revision_flag_divergence() {
    let repo = Repo::new();
    repo.write("file", "base\n");
    repo.ok(&["describe", "Shared work"]);
    let change = repo.change();
    let pointer = repo.path(".origin/workspace");
    let started = fs::read_to_string(&pointer).unwrap();
    repo.ok(&["describe", "Moved on"]);
    // Edits on the revision before, recorded by `new`, make a second head.
    fs::write(&pointer, &started).unwrap();
    repo.write("file", "stale\n");
    repo.ok(&["new", "Next"]);
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
}

#[test]
fn a_statement_takes_its_details_as_text_or_from_stdin() {
    let repo = Repo::new();
    repo.ok(&["annotate", "@", "review", "--pass", "-m", "Looks good"]);
    let statements = repo.ok(&["statements", "@"]);
    assert!(
        statements.contains("    payload: Looks good\n"),
        "{statements}"
    );

    let mut piped = Command::new(env!("CARGO_BIN_EXE_origin"))
        .args(["annotate", "@", "ci.test", "--fail", "--payload", "-"])
        .current_dir(repo.dir.path())
        .env("ORIGIN_CONFIG_DIR", repo.config.path())
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    piped
        .stdin
        .take()
        .unwrap()
        .write_all(b"1 test failed\n")
        .unwrap();
    assert!(piped.wait().unwrap().success());
    let statements = repo.ok(&["statements", "@"]);
    assert!(
        statements.contains("    payload: 1 test failed\n"),
        "{statements}"
    );

    let both = repo.fails(&[
        "annotate",
        "@",
        "review",
        "--pass",
        "-m",
        "x",
        "--payload",
        "-",
    ]);
    assert!(both.contains("cannot be used with"), "{both}");
}

#[test]
fn init_with_a_malformed_key_leaves_no_repository() {
    let repo = Repo::new();
    fs::write(repo.config.path().join("node.key"), b"short").unwrap();
    let other = tempfile::tempdir().unwrap();
    assert!(!repo.run(other.path(), &["init"]).status.success());
    assert!(!other.path().join(".origin").exists());
}

#[test]
fn only_the_creator_of_a_change_abandons_it() {
    let repo = Repo::new();
    repo.write("file", "ada's\n");
    repo.ok(&["describe", "Ada's change"]);
    let change = repo.change();
    // Eve takes the change over, then tries to give it up.
    let config = repo.config.path().join("config.toml");
    fs::write(&config, "user = \"eve\"\n").unwrap();
    repo.write("file", "eve's\n");
    repo.ok(&["describe", "Eve's now"]);
    let pointer = repo.read(".origin/workspace");
    let statements = repo.ok(&["statements"]);

    let refused = repo.fails(&["abandon", "--reason", "Not mine"]);
    assert!(
        refused.contains("may not issue `core.abandon`"),
        "{refused}"
    );
    assert_eq!(repo.read(".origin/workspace"), pointer);
    assert_eq!(repo.read("file"), "eve's\n");
    assert_eq!(repo.ok(&["statements"]), statements);
    fs::write(&config, "user = \"ada\"\n").unwrap();
    assert!(!repo.ok(&["show", &change]).contains("bandoned"));
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

/// The id of the statement an `annotate` issued, as it prints it.
fn issued(output: &str) -> String {
    let rest = output.split("statement ").nth(1).unwrap();
    rest.split(':').next().unwrap().to_owned()
}

#[test]
fn withdrawing_takes_a_statement_back() {
    let repo = Repo::new();
    repo.ok(&["describe", "Ours"]);
    let passed = issued(&repo.ok(&["annotate", "@", "review", "--pass"]));
    let withdrew = repo.ok(&["annotate", "@", "review", "--withdraw", "-m", "Too hasty"]);
    assert!(
        withdrew.starts_with(&format!("Withdrew statement {passed} with statement ")),
        "{withdrew}"
    );
    let revision = revision_of(&repo).short();
    assert!(
        withdrew.ends_with(&format!(": review about revision {revision}\n")),
        "{withdrew}"
    );
    let listed = repo.ok(&["statements"]);
    assert!(
        listed.contains(&format!("{passed} review pass by user:ada (superseded by ")),
        "{listed}"
    );
    assert!(
        listed.contains(" review withdrawn by user:ada\n    payload: Too hasty\n"),
        "{listed}"
    );
}

#[test]
fn withdrawing_needs_a_statement_to_take_back() {
    let repo = Repo::new();
    repo.ok(&["describe", "Ours"]);
    let nothing = "no `review` statement about revision";
    assert!(
        repo.fails(&["annotate", "@", "review", "--withdraw"])
            .contains(nothing)
    );
    repo.ok(&["annotate", "@", "review", "--fail"]);
    repo.ok(&["annotate", "@", "review", "--withdraw"]);
    // Withdrawn already.
    assert!(
        repo.fails(&["annotate", "@", "review", "--withdraw"])
            .contains(nothing)
    );
    let inputs = repo.fails(&["annotate", "@", "ci.test", "--withdraw", "--inputs", "src"]);
    assert!(inputs.contains("cannot be used with"), "{inputs}");
}

#[test]
fn a_promoted_result_is_withdrawn_where_it_ran() {
    let repo = Repo::new();
    repo.write("src/lib.rs", "fn a() {}\n");
    repo.ok(&["annotate", "@", "ci.test", "--pass", "--inputs", "src"]);
    let ran = revision_of(&repo).short();
    repo.ok(&["new"]);
    repo.write("docs.md", "docs\n");
    assert!(repo.ok(&["promote"]).contains("Promoted ci.test pass"));
    let refused = repo.fails(&["annotate", "@", "ci.test", "--withdraw"]);
    assert!(
        refused.contains(&format!(
            "withdraw it there, with `origin annotate {ran} ci.test --withdraw`"
        )),
        "{refused}"
    );
}

#[test]
fn statements_about_statements_show_under_them() {
    let repo = Repo::new();
    let failed = issued(&repo.ok(&["annotate", "@", "ci.test", "--fail"]));
    let on = |args: &[&str]| repo.ok(&[&["annotate", "--on", &failed[..8]], args].concat());
    let marked = on(&["flaky", "--neutral", "-m", "Timeout on CI"]);
    assert!(
        marked.contains(&format!(": flaky neutral about statement {failed}\n")),
        "{marked}"
    );
    let flaky = issued(&marked);
    let listed = repo.ok(&["statements"]);
    assert!(
        listed.ends_with(&format!(
            "{failed} ci.test fail by user:ada\n    {flaky} flaky neutral by user:ada\n        \
             payload: Timeout on CI\n"
        )),
        "{listed}"
    );

    // Taken back the same way.
    let withdrew = on(&["flaky", "--withdraw"]);
    assert!(
        withdrew.starts_with(&format!("Withdrew statement {flaky} with statement ")),
        "{withdrew}"
    );
    assert!(withdrew.ends_with(&format!(": flaky about statement {failed}\n")));
    let listed = repo.ok(&["statements"]);
    assert!(listed.contains("\n    "), "{listed}");
    assert!(
        listed.contains(" flaky withdrawn by user:ada\n"),
        "{listed}"
    );
}

#[test]
fn annotating_a_statement_needs_one_and_no_inputs() {
    let repo = Repo::new();
    let mut ids = Vec::new();
    // Seventeen statements: two of them start with the same hex digit.
    for kind in 0..17 {
        let kind = format!("check.k{kind}");
        ids.push(issued(&repo.ok(&["annotate", "@", &kind, "--pass"])));
    }
    let digit = (0..16)
        .map(|digit| format!("{digit:x}"))
        .find(|digit| {
            ids.iter()
                .filter(|id| id.starts_with(digit.as_str()))
                .count()
                > 1
        })
        .unwrap();
    let ambiguous = repo.fails(&["annotate", "--on", &digit, "flaky", "--neutral"]);
    assert!(
        ambiguous.contains("could be any of the statements"),
        "{ambiguous}"
    );
    let unknown = repo.fails(&["annotate", "--on", "0000000000", "flaky", "--neutral"]);
    assert!(
        unknown.contains("no statement has an id starting with"),
        "{unknown}"
    );
    let invalid = repo.fails(&["annotate", "--on", "", "flaky", "--neutral"]);
    assert!(invalid.contains("is not a statement id"), "{invalid}");
    // Neither a statement nor a withdrawal is promoted: no inputs, nor
    // what describes them.
    for declared in [
        &["--inputs", "src"][..],
        &["--definition", "x"],
        &["--toolchain", "x"],
        &["--external"],
    ] {
        for verdict in [
            &["--on", &ids[0], "flaky", "--pass"][..],
            &["@", "flaky", "--withdraw"],
        ] {
            let args = [&["annotate"][..], verdict, declared].concat();
            let refused = repo.fails(&args);
            assert!(
                refused.contains("cannot be used with"),
                "{args:?}: {refused}"
            );
        }
    }
    let both = repo.fails(&["annotate", "--on", &ids[0], "@", "flaky", "--pass"]);
    assert!(both.contains("with --on, give the kind alone"), "{both}");
    // A bad kind is an error of the command line, found before anything.
    let outside = tempfile::tempdir().unwrap();
    let bad = repo.run(outside.path(), &["annotate", "@", "Bad Kind!", "--pass"]);
    assert_eq!(bad.status.code(), Some(2));
    let error = String::from_utf8_lossy(&bad.stderr);
    assert!(
        error.starts_with("error: invalid kind `Bad Kind!`"),
        "{error}"
    );
    assert!(error.contains("\nUsage: origin annotate "), "{error}");
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
}
