//! End-to-end tests of `origin ci`, through the real `origin` and
//! `origin-ci` binaries: as a tool in a working copy, and as a worker
//! answering a server's check requests.

#[path = "../../origin-src/tests/common/mod.rs"]
mod common;

use std::fs;
use std::path::Path;
use std::process::Child;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use common::{Repo, Server, User};

/// A `checks.toml` whose `test` reads `src` and itself, and appends a line
/// to `log` each time it runs.
fn checks(log: &Path) -> String {
    format!(
        "[checks.test]\ncommand = \"test -f src/lib.rs && echo tested && echo ran >> {}\"\n\
         inputs = [\"src\", \"checks.toml\"]\ntoolchain = \"echo toolchain 1\"\n",
        log.display()
    )
}

/// How many times the check logging to `log` ran.
fn runs(log: &Path) -> usize {
    fs::read_to_string(log).map_or(0, |log| log.lines().count())
}

#[test]
fn tool_mode_runs_checks_and_promotes_unchanged_results() {
    let repo = Repo::new();
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    repo.write("src/lib.rs", "fn a() {}\n");
    repo.write(
        "checks.toml",
        &(checks(&log)
            + "[checks.lint]\ncommand = \"echo lint failed; exit 1\"\ninputs = [\"src\"]\n"),
    );

    let output = repo.run(repo.root(), &["ci", "run"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!output.status.success(), "lint fails: {stdout}");
    assert!(stdout.contains("ci.lint fail in "), "{stdout}");
    assert!(
        stdout.contains("lint failed\n"),
        "a failure shows its output: {stdout}"
    );
    assert!(stdout.contains("ci.test pass in "), "{stdout}");
    assert_eq!(runs(&log), 1);
    let statements = repo.ok(&["statements"]);
    assert!(
        statements.contains("ci.test pass by user:ada\n"),
        "{statements}"
    );
    assert!(statements.contains("    payload: tested\n"), "{statements}");

    // Running again finds the results.
    let again = repo.run(repo.root(), &["ci", "run"]);
    let stdout = String::from_utf8(again.stdout).unwrap();
    assert!(stdout.contains("ci.test pass, already ran"), "{stdout}");
    assert_eq!(runs(&log), 1);

    // A docs-only change keeps both results, promoted.
    repo.write("docs/guide.md", "# Guide\n");
    let stdout = String::from_utf8(repo.run(repo.root(), &["ci", "run"]).stdout).unwrap();
    assert!(stdout.contains("ci.test pass, promoted"), "{stdout}");
    assert!(stdout.contains("ci.lint fail, promoted"), "{stdout}");
    assert_eq!(runs(&log), 1);

    // A change to `src` runs both again.
    repo.write("src/lib.rs", "fn b() {}\n");
    let stdout = String::from_utf8(repo.run(repo.root(), &["ci", "run"]).stdout).unwrap();
    assert!(stdout.contains("ci.test pass in "), "{stdout}");
    assert_eq!(runs(&log), 2);

    // Nothing is left of the checkouts.
    let state = repo.config().join("state/origin-ci");
    let [key] = &fs::read_dir(&state).unwrap().collect::<Vec<_>>()[..] else {
        panic!("one repository's state");
    };
    let runs = key.as_ref().unwrap().path().join("runs");
    assert_eq!(fs::read_dir(runs).unwrap().count(), 0);
}

/// The policy rules that make `main` require a passing `ci.test` from the
/// runners, `user:ci`.
const RULES: &str = "\n[groups]\nrunners = [\"user:ci\"]\n";

/// A server whose admin, alice, made `main` require `ci.test`, landing the
/// rule over HTTP, and her clone of `main`, holding [`checks`].
fn server(log: &Path) -> (Server, Repo) {
    let server = Server::start_with(&["--admin", "user:alice"]);
    let alice = User::new("alice");
    let policy = Repo::clone(&alice, &server.url(), &["--target", "policy"]);
    let toml = policy.read("policy.toml").replace(
        "[targets.main]\nauthority = \"node:hosted\"\n",
        "[targets.main]\nauthority = \"node:hosted\"\n\
         require = [{ kind = \"ci.test\", verdict = \"pass\", issuer = \"group:runners\" }]\n",
    );
    policy.write("policy.toml", &(toml + RULES));
    policy.ok(&["describe", "Require tests"]);
    policy.ok(&["push"]);
    assert!(policy.ok(&["land"]).contains(" on policy #1\n"));

    let main = Repo::clone(&alice, &server.url(), &[]);
    main.write("checks.toml", &checks(log));
    main.write("src/lib.rs", "fn a() {}\n");
    (server, main)
}

/// Describes, pushes and lands the working copy's change, returning what
/// `land` printed.
fn land(repo: &Repo, description: &str) -> String {
    repo.ok(&["describe", description]);
    repo.ok(&["push"]);
    repo.ok(&["land"])
}

/// Starts a worker answering the server's requests as `user:ci`, with a
/// token that only reads and annotates `ci.*`, keeping its state in
/// `worker`'s configuration directory.
fn worker(server: &Server, worker: &User) -> (Child, Receiver<String>) {
    let token = server.issue_token("user:ci", &["read /", "annotate ci.*"]);
    let outside = tempfile::tempdir().unwrap();
    let (child, lines) = worker
        .with_token(&token)
        .spawn(outside.path(), &["ci", "worker", "--remote", &server.url()]);
    let started = lines.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(
        started.starts_with("Answering the check requests of "),
        "{started}"
    );
    (child, lines)
}

/// Waits for the worker's next outcome.
fn outcome(lines: &Receiver<String>) -> String {
    lines.recv_timeout(Duration::from_secs(20)).unwrap()
}

#[test]
fn workers_run_requested_checks_and_landings_continue() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    let (server, alice) = server(&log);
    let waiting = land(&alice, "Add the library");
    assert!(
        waiting.contains("waits for ci.test pass from group:runners"),
        "{waiting}"
    );

    // The worker answers the landing that waited before it started, and
    // the statement lands it.
    let runner = User::anonymous("ci");
    let (mut child, lines) = worker(&server, &runner);
    let ran = outcome(&lines);
    assert!(ran.starts_with("ci.test pass in "), "{ran}");
    assert_eq!(runs(&log), 1);
    assert_eq!(alice.ok(&["land", "--status"]), "No landing waits.\n");
    alice.ok(&["pull"]);
    assert!(alice.ok(&["log", "main"]).contains("Add the library"));

    // A docs-only change lands at once: its result is promoted.
    alice.write("docs/guide.md", "# Guide\n");
    let landed = land(&alice, "Document it");
    assert!(landed.contains("Landed change"), "{landed}");
    assert_eq!(runs(&log), 1);

    // Restarted, the worker resumes after its cursor, and answers the
    // requests made while it was down.
    child.kill().unwrap();
    child.wait().unwrap();
    let state = runner.config().join("state/origin-ci");
    let [key] = &fs::read_dir(&state).unwrap().collect::<Vec<_>>()[..] else {
        panic!("one remote's state");
    };
    assert!(key.as_ref().unwrap().path().join("cursor").exists());
    alice.ok(&["pull"]);
    alice.write("src/lib.rs", "fn b() {}\n");
    let waiting = land(&alice, "Change the library");
    assert!(waiting.contains("waits for ci.test"), "{waiting}");
    let (mut child, lines) = worker(&server, &runner);
    let ran = outcome(&lines);
    assert!(ran.starts_with("ci.test pass in "), "{ran}");
    assert_eq!(runs(&log), 2);
    alice.ok(&["pull"]);
    assert!(alice.ok(&["log", "main"]).contains("Change the library"));

    // A target landing on `main` is checked as the snapshot it would make.
    alice.ok(&["target", "new", "feature", "--from", "main"]);
    alice.ok(&["new", "--on", "feature", "Feature work"]);
    alice.write("src/feature.rs", "fn feature() {}\n");
    assert!(land(&alice, "Feature work").contains(" on feature #1"));
    let waiting = alice.ok(&["land", "--from", "feature", "--onto", "main"]);
    assert!(waiting.contains("waits for ci.test"), "{waiting}");
    let ran = outcome(&lines);
    assert!(
        ran.starts_with("ci.test pass in ") && ran.contains("snapshot:"),
        "{ran}"
    );
    assert_eq!(alice.ok(&["land", "--status"]), "No landing waits.\n");
    child.kill().unwrap();
    child.wait().unwrap();
}
