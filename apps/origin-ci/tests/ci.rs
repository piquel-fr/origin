//! End-to-end tests of `origin ci`, through the real `origin` and
//! `origin-ci` binaries: as a tool in a working copy, and as a worker
//! answering a server's check requests.

#[path = "../../origin-src/tests/common/mod.rs"]
mod common;

use std::fs;
use std::path::Path;
use std::process::Child;
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

use common::{Repo, Server, User};
use origin_platform::{Native, Processes};
use origin_shared::{Digest, Inputs, RecordId};
use origin_src::annotations::Annotations;
use origin_src::store::Store;

/// A `checks.toml` whose `test` reads `src`, and, as every check,
/// `checks.toml`, and appends a line to `log` each time it runs.
fn checks(log: &Path) -> String {
    format!(
        "[checks.test]\ncommand = \"test -f src/lib.rs && echo tested && echo ran >> {}\"\n\
         inputs = [\"src\"]\ntoolchain = \"echo toolchain 1\"\n",
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

    // A withdrawn result is none: it runs again.
    repo.ok(&["annotate", "@", "ci.test", "--withdraw"]);
    let stdout = String::from_utf8(repo.run(repo.root(), &["ci", "run"]).stdout).unwrap();
    assert!(stdout.contains("ci.test pass in "), "{stdout}");
    assert_eq!(runs(&log), 3);

    // Nothing is left of the checkouts.
    let state = repo.config().join("state/origin-ci");
    let [key] = &fs::read_dir(&state).unwrap().collect::<Vec<_>>()[..] else {
        panic!("one repository's state");
    };
    let runs = key.as_ref().unwrap().path().join("runs");
    assert_eq!(fs::read_dir(runs).unwrap().count(), 0);
}

/// The inputs of the `kind` statement about `revision`, from the
/// repository's records.
fn inputs_of(repo: &Repo, revision: &str, kind: &str) -> Inputs {
    let store = Store::new(repo.path(".origin"), Native::default());
    let revision = RecordId::Revision(revision.parse().unwrap());
    let about = Annotations::open(&store).unwrap().about(revision).unwrap();
    let current = about
        .into_iter()
        .rev()
        .find(|annotation| annotation.statement.kind.as_str() == kind)
        .unwrap();
    current.statement.inputs.unwrap()
}

#[test]
fn exec_runs_a_command_as_a_check() {
    let repo = Repo::new();
    repo.write("src/lib.rs", "fn a() {}\n");
    repo.ok(&["describe", "First"]);
    let first = repo.revision();
    repo.ok(&["new"]);
    repo.write("src/lib.rs", "fn b() {}\n");
    let exec = |command: &[&str]| {
        let args = [
            &[
                "ci", "exec", "lint", "--rev", &first, "--inputs", "src", "--",
            ],
            command,
        ];
        let output = repo.run(repo.root(), &args.concat());
        let stdout = String::from_utf8(output.stdout).unwrap();
        (output.status.success(), stdout)
    };

    // In a checkout of the revision, not of the files here.
    let reads = ["sh", "-c", "cat src/lib.rs; printf '%s\\n' \"$0\"", "a b"];
    let (passed, stdout) = exec(&reads);
    assert!(passed && stdout.starts_with("ci.lint pass in "), "{stdout}");
    let statements = repo.ok(&["statements", &first]);
    assert!(
        statements.contains("ci.lint pass by user:ada\n"),
        "{statements}"
    );
    assert!(statements.contains("    inputs: src\n"), "{statements}");
    assert!(
        statements.contains("    payload: fn a() {}\n"),
        "{statements}"
    );
    let definition = r#"sh -c 'cat src/lib.rs; printf '\''%s\n'\'' "$0"' 'a b'"#;
    let inputs = inputs_of(&repo, &first, "ci.lint");
    assert_eq!(inputs.definition, Digest::of(definition.as_bytes()));

    // Asked again, it runs again; a failure supersedes the pass.
    assert!(exec(&reads).1.starts_with("ci.lint pass in "));
    let (passed, stdout) = exec(&["sh", "-c", "echo broken; exit 3"]);
    assert!(!passed, "{stdout}");
    assert!(stdout.starts_with("ci.lint fail in "), "{stdout}");
    assert!(stdout.ends_with("broken\n"), "its output: {stdout}");
    let statements = repo.ok(&["statements", &first]);
    assert_eq!(
        statements.matches(" (superseded by ").count(),
        2,
        "{statements}"
    );
    assert!(
        statements.contains("ci.lint fail by user:ada\n"),
        "{statements}"
    );

    // Its inputs make it promotable where they're the same.
    repo.write("src/lib.rs", "fn a() {}\n");
    repo.ok(&["describe", "Same library"]);
    let promoted = repo.ok(&["promote"]);
    assert!(
        promoted.contains("Promoted ci.lint fail by user:ada"),
        "{promoted}"
    );
}

#[test]
fn a_failed_exec_wins_over_a_promoted_pass() {
    let repo = Repo::new();
    repo.require_tests();
    let exec = |command: &str| {
        let args = ["ci", "exec", "test", "--inputs", "src", "--", command];
        repo.run(repo.root(), &args).status.success()
    };
    repo.ok(&["new", "--on", "main", "Library"]);
    repo.write("src/lib.rs", "fn a() {}\n");
    assert!(exec("true"));
    assert!(repo.ok(&["land"]).contains(" on main #1\n"));

    // The same `src` gets the pass promoted, then fails there.
    repo.ok(&["new", "--on", "main", "Docs"]);
    repo.write("docs.md", "docs\n");
    assert!(repo.ok(&["promote"]).contains("Promoted ci.test pass"));
    assert!(!exec("false"));
    let landing = repo.ok(&["land"]);
    assert!(landing.starts_with("The landing of change "), "{landing}");
}

#[test]
fn runs_replace_promotions_a_result_there_replaces() {
    let repo = Repo::new();
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    repo.write("src/lib.rs", "fn a() {}\n");
    repo.write("checks.toml", &checks(&log));
    let run = || String::from_utf8(repo.run(repo.root(), &["ci", "run"]).stdout).unwrap();
    assert!(run().contains("ci.test pass in "));
    // Each docs-only change gets the pass promoted, then a statement of
    // its own: `ci run` doesn't take the promotion for a result then.
    let promoted = |file: &str| {
        repo.ok(&["new"]);
        repo.write(file, "docs\n");
        assert!(run().contains("ci.test pass, promoted"));
    };
    let ran = |runs_now| {
        let stdout = run();
        assert!(stdout.contains("ci.test pass in "), "{stdout}");
        assert_eq!(runs(&log), runs_now);
    };

    promoted("failed.md");
    repo.ok(&["annotate", "@", "ci.test", "--fail"]);
    ran(2);
    promoted("withdrawn.md");
    repo.ok(&["annotate", "@", "ci.test", "--neutral"]);
    repo.ok(&["annotate", "@", "ci.test", "--withdraw"]);
    ran(3);
}

#[test]
fn a_promotion_of_a_result_taken_back_covers_nothing() {
    let repo = Repo::new();
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    repo.write("src/lib.rs", "fn a() {}\n");
    repo.write("checks.toml", &checks(&log));
    let run = || String::from_utf8(repo.run(repo.root(), &["ci", "run"]).stdout).unwrap();
    assert!(run().contains("ci.test pass in "));
    let first = repo.revision();
    repo.ok(&["new"]);
    repo.write("docs.md", "docs\n");
    assert!(run().contains("ci.test pass, promoted"));

    // Its issuer takes the original back: the promotion holds nothing.
    repo.ok(&["annotate", &first, "ci.test", "--withdraw"]);
    let stdout = run();
    assert!(stdout.contains("ci.test pass in "), "{stdout}");
    assert_eq!(runs(&log), 2);
}

/// The policy rules that make `main` require a passing `ci.test` from the
/// runners, `user:ci`.
const RULES: &str = "\n[groups]\nrunners = [\"user:ci\"]\n";

/// A server whose admin, alice, landed [`checks`] on `main`, then made it
/// require `ci.test`, landing the rule over HTTP, and her clone of `main`.
fn server(log: &Path) -> (Server, Repo) {
    let server = Server::start_with(&["--admin", "user:alice"]);
    let alice = User::new("alice");
    let main = Repo::clone(&alice, &server.url(), &[]);
    main.write("checks.toml", &checks(log));
    main.write("src/lib.rs", "fn a() {}\n");
    assert!(land(&main, "Add the library").contains("Landed change"));

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
    alice.write("src/lib.rs", "fn b() {}\n");
    let waiting = land(&alice, "Change the library");
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
    assert!(alice.ok(&["log", "main"]).contains("Change the library"));

    // A docs-only change lands at once: its result is promoted.
    alice.write("docs/guide.md", "# Guide\n");
    let landed = land(&alice, "Document it");
    assert!(landed.contains("Landed change"), "{landed}");
    assert_eq!(runs(&log), 1);

    // A change to the check lands at once: the worker runs `main`'s,
    // which it doesn't change, on the same `src`, so the result is
    // promoted.
    alice.write(
        "checks.toml",
        &checks(&log).replace("echo tested", "echo tested again"),
    );
    let landed = land(&alice, "Change the check");
    assert!(landed.contains("Landed change"), "{landed}");
    assert_eq!(runs(&log), 1);

    // Landed, the new check runs: `main`'s `checks.toml` is one of its
    // inputs, so no result of the old one is promoted.
    alice.write("docs/guide.md", "# Guide\n\nMore.\n");
    let waiting = land(&alice, "Document more");
    assert!(waiting.contains("waits for ci.test"), "{waiting}");
    let ran = outcome(&lines);
    assert!(ran.starts_with("ci.test pass in "), "{ran}");
    assert_eq!(runs(&log), 2);

    // Restarted, the worker answers the requests made while it was down.
    child.kill().unwrap();
    child.wait().unwrap();
    alice.ok(&["pull"]);
    alice.write("src/lib.rs", "fn c() {}\n");
    let waiting = land(&alice, "Change it again");
    assert!(waiting.contains("waits for ci.test"), "{waiting}");
    let (mut child, lines) = worker(&server, &runner);
    let ran = outcome(&lines);
    assert!(ran.starts_with("ci.test pass in "), "{ran}");
    assert_eq!(runs(&log), 3);
    alice.ok(&["pull"]);
    assert!(alice.ok(&["log", "main"]).contains("Change it again"));

    // A target landing on `main` is checked as the merge it would make, a
    // revision on `main`'s tip.
    alice.ok(&["target", "new", "feature", "--from", "main"]);
    alice.ok(&["new", "--on", "feature", "Feature work"]);
    alice.write("src/feature.rs", "fn feature() {}\n");
    assert!(land(&alice, "Feature work").contains(" on feature #1"));
    let waiting = alice.ok(&["land", "--from", "feature", "--onto", "main"]);
    assert!(waiting.contains("waits for ci.test"), "{waiting}");
    let ran = outcome(&lines);
    assert!(
        ran.starts_with("ci.test pass in ") && ran.contains("revision:"),
        "{ran}"
    );
    assert_eq!(alice.ok(&["land", "--status"]), "No landing waits.\n");
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn results_stand_for_the_checks_in_force_on_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    let (server, alice) = server(&log);
    let runner = User::anonymous("ci");
    let (mut child, lines) = worker(&server, &runner);
    alice.write("src/lib.rs", "fn b() {}\n");
    let waiting = land(&alice, "Change the library");
    assert!(waiting.contains("waits for ci.test"), "{waiting}");
    let ran = outcome(&lines);
    assert!(ran.starts_with("ci.test pass in "), "{ran}");
    assert_eq!(runs(&log), 1);

    // With `main`'s checks unchanged, a docs-only change has the result
    // promoted, and so does a change to `checks.toml` that requires a
    // file `src` lacks.
    alice.ok(&["pull"]);
    alice.write("docs/guide.md", "# Guide\n\n");
    let landed = land(&alice, "Document it");
    assert!(landed.contains("Landed change"), "{landed}");
    let strict = checks(&log).replace("src/lib.rs", "src/missing.rs");
    alice.write("checks.toml", &strict);
    let landed = land(&alice, "Require more");
    assert!(landed.contains("Landed change"), "{landed}");

    // Restoring the old checks doesn't bring back the result they had
    // with the same `src`: the worker runs `main`'s, which fails.
    alice.write("checks.toml", &checks(&log));
    let waiting = land(&alice, "Restore the old checks");
    assert!(waiting.contains("waits for ci.test"), "{waiting}");
    let ran = outcome(&lines);
    assert!(ran.starts_with("ci.test fail in "), "{ran}");
    assert_eq!(runs(&log), 1);
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn workers_run_the_checks_of_the_target_not_the_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    let (server, alice) = server(&log);
    let runner = User::anonymous("ci");
    let (mut child, lines) = worker(&server, &runner);

    // A change that breaks `main`'s `ci.test`, and redefines it to pass.
    fs::remove_file(alice.path("src/lib.rs")).unwrap();
    alice.write("src/main.rs", "fn main() {}\n");
    alice.write("checks.toml", "[checks.test]\ncommand = \"true\"\n");
    let waiting = land(&alice, "Pass trivially");
    assert!(waiting.contains("waits for ci.test"), "{waiting}");

    // The worker runs `main`'s, which fails, and the landing waits.
    let ran = outcome(&lines);
    assert!(ran.starts_with("ci.test fail in "), "{ran}");
    let status = alice.ok(&["land", "--status"]);
    assert!(status.contains("waits for ci.test"), "{status}");

    // `origin ci run` checks the revision as its own `checks.toml` says.
    let stdout = String::from_utf8(alice.run(alice.root(), &["ci", "run"]).stdout).unwrap();
    assert!(stdout.contains("ci.test pass in "), "{stdout}");
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn checks_run_in_checkouts_of_their_own() {
    let repo = Repo::new();
    repo.write("input", "broken\n");
    repo.write(
        "checks.toml",
        "[checks.fix]\ncommand = \"echo fixed > input\"\n\
         [checks.verify]\ncommand = \"grep -q fixed input\"\n",
    );
    let output = repo.run(repo.root(), &["ci", "run"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!output.status.success(), "{stdout}");
    assert!(stdout.contains("ci.fix pass in "), "{stdout}");
    assert!(
        stdout.contains("ci.verify fail in "),
        "it never sees what ci.fix wrote: {stdout}"
    );
    assert_eq!(repo.read("input"), "broken\n");
}

#[test]
fn toolchains_are_told_apart_by_all_their_output() {
    let repo = Repo::new();
    let dir = tempfile::tempdir().unwrap();
    let tools = dir.path().join("tools");
    // Longer than the output a check keeps.
    let version = |version: &str| {
        fs::write(&tools, format!("tool {version}\n{}", "x".repeat(100_000))).unwrap();
    };
    version("1");
    repo.write(
        "checks.toml",
        &format!(
            "[checks.test]\ncommand = \"true\"\ntoolchain = \"cat {}\"\n",
            tools.display()
        ),
    );
    let run = || String::from_utf8(repo.run(repo.root(), &["ci", "run"]).stdout).unwrap();
    assert!(run().contains("ci.test pass in "));
    version("2");
    let again = run();
    assert!(again.contains("ci.test pass in "), "{again}");
}

#[test]
fn checks_never_see_the_runners_token() {
    let repo = Repo::new();
    repo.write(
        "checks.toml",
        "[checks.token]\ncommand = 'test -z \"$ORIGIN_TOKEN\"'\n",
    );
    let output = repo.run(repo.root(), &["ci", "run"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("ci.token pass in "), "{stdout}");
}

#[test]
fn workers_outlive_the_server_and_lose_no_check() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    let (mut server, alice) = server(&log);
    let runner = User::anonymous("ci");
    let (mut child, lines) = worker(&server, &runner);

    // The server goes away while the worker waits for events.
    server.restart(|| thread::sleep(Duration::from_millis(500)));
    alice.write("src/lib.rs", "fn b() {}\n");
    let waiting = land(&alice, "Change the library");
    assert!(waiting.contains("waits for ci.test"), "{waiting}");
    let ran = outcome(&lines);
    assert!(ran.starts_with("ci.test pass in "), "{ran}");
    assert_eq!(runs(&log), 1);
    alice.ok(&["pull"]);

    // And while a check runs, so its result cannot be issued: once the
    // server is back, the check runs again, and the landing goes on. A
    // slower check lands at once, with the result of `main`'s promoted.
    alice.write(
        "checks.toml",
        &format!(
            "[checks.test]\ncommand = \"echo ran >> {} && sleep 1\"\ninputs = [\"src\"]\n",
            log.display()
        ),
    );
    let landed = land(&alice, "Take longer");
    assert!(landed.contains("Landed change"), "{landed}");
    alice.write("src/lib.rs", "fn c() {}\n");
    let waiting = land(&alice, "Change it again");
    assert!(waiting.contains("waits for ci.test"), "{waiting}");
    while runs(&log) < 2 {
        thread::sleep(Duration::from_millis(20));
    }
    // The check ends meanwhile, and fails to issue its result.
    server.restart(|| thread::sleep(Duration::from_secs(2)));
    let ran = outcome(&lines);
    assert!(ran.starts_with("ci.test pass in "), "{ran}");
    assert_eq!(runs(&log), 3);
    assert_eq!(alice.ok(&["land", "--status"]), "No landing waits.\n");
    child.kill().unwrap();
    child.wait().unwrap();
}

/// Whether the process `pid` runs, as `kill -0` finds it.
fn alive(pid: &str) -> bool {
    let probe = std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status();
    probe.unwrap().success()
}

/// Waits for what `path` holds, written once.
fn written(path: &Path) -> String {
    loop {
        match fs::read_to_string(path) {
            Ok(text) if text.ends_with('\n') => return text.trim_end().to_owned(),
            _ => thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Waits until `pid` is gone, failing after a few seconds.
fn gone(pid: &str) {
    for _ in 0..250 {
        if !alive(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("process {pid} still runs");
}

/// Waits until `pid`, which a check left running, is gone: killed by
/// `origin-ci` where the platform tells which group holds what (see
/// `Processes::SEES_HELD_FILES`), or else by the test, so it leaves nothing
/// running.
fn ended(pid: &str) {
    if !Native::SEES_HELD_FILES {
        // Whether it ran still or not, `gone` tells.
        let _ = std::process::Command::new("kill")
            .args(["-9", pid])
            .status();
    }
    gone(pid);
}

#[test]
fn checks_leave_nothing_running_once_they_end_or_their_run_is_killed() {
    let repo = Repo::new();
    let dir = tempfile::tempdir().unwrap();
    let started = dir.path().join("started");
    let check = |command: &str| {
        let command = command.replace("{started}", &started.display().to_string());
        repo.write(
            "checks.toml",
            &format!("[checks.test]\ncommand = \"{command}\"\n"),
        );
        let _ = fs::remove_file(&started);
    };

    // What a check leaves running is killed once it ends, without waiting
    // for it to close the output it holds; where the platform can't tell
    // which group still holds what it started, it escapes instead, and the
    // run waits for it to close that output.
    check("sleep 60 & echo $! > {started}");
    let ran = Instant::now();
    let output = repo.run(repo.root(), &["ci", "run"]);
    assert!(output.status.success());
    if Native::SEES_HELD_FILES {
        assert!(
            ran.elapsed() < Duration::from_secs(30),
            "it waited for sleep"
        );
    }
    ended(&written(&started));

    // A run killed while its check runs leaves it running, until the next
    // run sweeps its checkout.
    check("sleep 60 & echo $! > {started}; wait");
    let (mut run, _) = repo.spawn(&["ci", "run"]);
    let sleep = written(&started);
    run.kill().unwrap();
    run.wait().unwrap();
    assert!(alive(&sleep), "nothing kills it yet");
    check("true");
    assert!(repo.run(repo.root(), &["ci", "run"]).status.success());
    ended(&sleep);
}
