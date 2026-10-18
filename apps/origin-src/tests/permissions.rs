//! End-to-end tests of permissions: what users see of a server's
//! repository, and what they may change, through the real binary.

mod common;

use std::fs;

use common::{Repo, Server, User};

/// Everyone reads, alice writes everything and changes the rules, bob
/// writes `src` and `policy.toml`, but isn't admin, both land, and only
/// alice sees `secrets`.
const RULES: &str = r#"
[grants]
"user:alice" = ["admin", "land *"]
"user:bob" = ["land *"]

[[paths]]
path = "/"
grants = { all = "read", "user:alice" = "write" }

[[paths]]
path = "src"
grants = { "user:bob" = "write" }

[[paths]]
path = "policy.toml"
grants = { "user:bob" = "write" }

[[paths]]
path = "secrets"
grants = { all = "none", "user:alice" = "write" }
"#;

/// A server with [`RULES`], and alice's clone, where she landed a README,
/// a library and a secret.
fn server() -> (Server, Repo) {
    let server = Server::start();
    server.add_policy(RULES);
    let alice = Repo::clone(&User::new("alice"), &server.url(), &[]);
    alice.write("README.md", "hello\n");
    alice.write("src/lib.rs", "fn lib() {}\n");
    alice.write("secrets/key", "hunter2\n");
    alice.ok(&["describe", "First"]);
    alice.ok(&["push"]);
    alice.ok(&["land"]);
    (server, alice)
}

#[test]
fn users_see_and_change_only_what_they_may() {
    let (server, alice) = server();
    assert_eq!(alice.ok(&["whoami"]), "user:alice\n");

    // Bob gets everything but the secret, and the working copy is clean.
    let bob = Repo::clone(&User::new("bob"), &server.url(), &[]);
    assert_eq!(bob.read("src/lib.rs"), "fn lib() {}\n");
    assert!(!bob.path("secrets/key").exists());
    assert!(bob.ok(&["status"]).ends_with("No changes.\n"));

    // Bob may change `src`, and the secret survives his landing.
    bob.write("src/lib.rs", "fn lib() { bob() }\n");
    bob.ok(&["describe", "Bob's"]);
    bob.ok(&["push"]);
    bob.ok(&["land"]);
    alice.ok(&["pull"]);
    assert_eq!(alice.read("src/lib.rs"), "fn lib() { bob() }\n");
    assert_eq!(alice.read("secrets/key"), "hunter2\n");

    // Changes to files bob may not write fail before reaching the server.
    bob.write("README.md", "bob was here\n");
    let error = bob.fails(&["push"]);
    assert!(error.contains("you may not change `README.md`"), "{error}");

    // Anonymous users read what everyone may, and change nothing.
    let anonymous = Repo::clone(&User::anonymous("eve"), &server.url(), &[]);
    assert_eq!(anonymous.read("README.md"), "hello\n");
    assert!(!anonymous.path("secrets").exists());
    assert!(anonymous.ok(&["whoami"]).starts_with("Anonymous"));
    let permissions = anonymous.ok(&["permissions"]);
    assert!(
        permissions.contains("read  /\nnone  secrets\n"),
        "{permissions}"
    );
}

#[test]
fn readable_files_sharing_a_secret_files_content_are_kept() {
    let (server, alice) = server();
    alice.write("public", "hunter2\n");
    alice.ok(&["push"]);
    alice.ok(&["land"]);

    let bob = Repo::clone(&User::new("bob"), &server.url(), &[]);
    assert_eq!(bob.read("public"), "hunter2\n");
    assert!(!bob.path("secrets/key").exists());
    assert!(bob.ok(&["status"]).ends_with("No changes.\n"));
}

#[test]
fn granting_read_access_restores_withheld_files() {
    let (server, _alice) = server();
    let bob = Repo::clone(&User::new("bob"), &server.url(), &[]);
    assert!(!bob.path("secrets/key").exists());

    // Bob can't change the rules: writing `policy.toml` isn't enough, only
    // an admin lands on `policy`.
    let rules = Repo::clone(&User::new("bob"), &server.url(), &["--target", "policy"]);
    let mut policy = rules.read("policy.toml");
    policy.push_str("\n[[paths]]\npath = \"secrets/key\"\ngrants = { \"user:bob\" = \"read\" }\n");
    rules.write("policy.toml", &policy);
    rules.ok(&["describe", "Let bob read the key"]);
    rules.ok(&["push"]);
    let error = rules.fails(&["land"]);
    assert!(error.contains("you may not land on `policy`"), "{error}");

    // Alice can.
    let admin = Repo::clone(&User::new("alice"), &server.url(), &["--target", "policy"]);
    admin.write("policy.toml", &policy);
    admin.ok(&["describe", "Let bob read the key"]);
    admin.ok(&["push"]);
    admin.ok(&["land"]);

    bob.ok(&["fetch"]);
    assert_eq!(bob.read("secrets/key"), "hunter2\n");
    assert!(bob.ok(&["status"]).ends_with("No changes.\n"));
}

#[test]
fn agents_act_with_scoped_tokens() {
    let (server, alice) = server();
    let output = alice.run(
        alice.root(),
        &[
            "token",
            "issue",
            "--scope",
            "read README.md",
            "--scope",
            "write src",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let token = String::from_utf8(output.stdout).unwrap();

    // The agent acts for alice, but only reads and writes what its token
    // allows: it lands nothing.
    let agent = Repo::clone(
        &User::new("alice").with_token(token.trim()),
        &server.url(),
        &[],
    );
    assert_eq!(agent.read("README.md"), "hello\n");
    assert!(
        !agent.path("secrets").exists(),
        "alice reads it, her token doesn't"
    );
    let whoami = agent.ok(&["whoami"]);
    assert_eq!(
        whoami,
        "user:alice\nToken scope: read README.md, write src\n"
    );
    agent.write("src/lib.rs", "fn lib() { agent() }\n");
    agent.ok(&["describe", "The agent's work"]);
    agent.ok(&["push"]);
    let error = agent.fails(&["land"]);
    assert!(error.contains("you may not land on `main`"), "{error}");
    agent.write("README.md", "agent was here\n");
    let error = agent.fails(&["push"]);
    assert!(error.contains("you may not change `README.md`"), "{error}");
    let error = agent.fails(&["token", "issue", "--scope", "write /"]);
    assert!(error.contains("which don't include `write /`"), "{error}");

    // An operator signs tokens for identities of their own: they act with
    // the rights the policy gives them.
    let token = server.issue_token("user:agent-1", &["read /"]);
    let other = Repo::clone(
        &User::anonymous("agent-1").with_token(&token),
        &server.url(),
        &[],
    );
    assert_eq!(other.read("README.md"), "hello\n");
    assert!(other.ok(&["whoami"]).starts_with("user:agent-1\n"));
}

#[test]
fn users_with_other_rights_take_changes_over() {
    let (server, alice) = server();
    // Alice changes the README and the library, and hands the change to
    // bob, who writes the library alone.
    alice.ok(&["new", "Shared work"]);
    alice.write("README.md", "hello, bob\n");
    alice.write("src/lib.rs", "fn lib() { alice() }\n");
    alice.ok(&["push"]);
    let change = alice.full_change();
    let bob = Repo::clone(&User::new("bob"), &server.url(), &[]);
    bob.ok(&["fetch", "--change", &change]);
    bob.ok(&["edit", &change]);
    bob.write("src/lib.rs", "fn lib() { bob() }\n");
    bob.ok(&["push", &change]);

    // He can't give it up for her: she created it.
    bob.ok(&["abandon", "--reason", "Mine now"]);
    let error = bob.fails(&["push", &change]);
    assert!(error.contains("cannot be put"), "{error}");

    // She lands it as he left it.
    alice.ok(&["fetch"]);
    alice.ok(&["land", &change]);
    alice.ok(&["pull"]);
    assert_eq!(alice.read("README.md"), "hello, bob\n");
    assert_eq!(alice.read("src/lib.rs"), "fn lib() { bob() }\n");
}

#[test]
fn users_sharing_a_repository_keep_their_own_rights() {
    let (_server, alice) = server();
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("bob");
    alice.workspace(&workspace, &[]);
    let bob = |args: &[&str]| User::new("bob").command(&workspace, args).output().unwrap();
    assert!(bob(&["fetch"]).status.success());

    // Bob's rights, which his fetch saved, aren't alice's.
    alice.write("README.md", "hello, alice\n");
    alice.ok(&["push"]);
    fs::write(workspace.join("README.md"), "hello, bob\n").unwrap();
    let pushed = bob(&["push"]);
    let error = String::from_utf8_lossy(&pushed.stderr);
    assert!(error.contains("you may not change `README.md`"), "{error}");
}

#[test]
fn users_who_never_synced_push_with_their_own_rights() {
    let server = Server::start();
    server.add_policy(RULES);
    // Bob, who writes `src` alone, clones; alice, in a workspace of his
    // clone, pushes a README before she ever fetches.
    let bob = Repo::clone(&User::new("bob"), &server.url(), &[]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("alice");
    let alice = User::new("alice");
    let path = workspace.to_str().unwrap();
    let made = alice
        .command(bob.root(), &["workspace", "new", path])
        .output();
    assert!(made.unwrap().status.success());
    let alice = |args: &[&str]| alice.command(&workspace, args).output().unwrap();
    fs::write(workspace.join("README.md"), "hello\n").unwrap();
    assert!(alice(&["describe", "Readme"]).status.success());
    let pushed = alice(&["push"]);
    assert!(
        pushed.status.success(),
        "{}",
        String::from_utf8_lossy(&pushed.stderr)
    );

    // Her rights are kept as hers, and those of each scope of her tokens
    // apart: a new token with the same scope keeps them.
    let access = bob.path(".origin/access");
    let kept = || fs::read_dir(&access).unwrap().count();
    let fetch = |scope: &[&str]| {
        let token = server.issue_token("user:alice", scope);
        let rotated = User::new("alice").with_token(&token);
        let fetched = rotated.command(&workspace, &["fetch"]).output().unwrap();
        assert!(fetched.status.success());
        kept()
    };
    let before = kept();
    let scoped = fetch(&["read /", "write /"]);
    assert_eq!(scoped, before + 1);
    assert_eq!(fetch(&["write /", "read /"]), scoped);
}

#[test]
fn tokens_of_one_user_keep_their_own_rights() {
    let (server, alice) = server();
    let dir = tempfile::tempdir().unwrap();
    // An agent of alice's, whose token reads and writes `src` alone,
    // fetches in a workspace of her clone.
    let token = server.issue_token("user:alice", &["read src", "write src"]);
    let agent = User::new("alice").with_token(&token);
    let workspace = dir.path().join("agent");
    alice.workspace(&workspace, &[]);
    let fetched = agent.command(&workspace, &["fetch"]).output().unwrap();
    assert!(fetched.status.success());

    // Alice's rights are still hers: a new workspace of hers has every
    // file, and her pushes are checked as she may write.
    let own = dir.path().join("own");
    alice.workspace(&own, &[]);
    assert_eq!(
        fs::read_to_string(own.join("secrets/key")).unwrap(),
        "hunter2\n"
    );
    fs::write(own.join("README.md"), "hello, alice\n").unwrap();
    let pushed = User::new("alice")
        .command(&own, &["push"])
        .output()
        .unwrap();
    assert!(
        pushed.status.success(),
        "{}",
        String::from_utf8_lossy(&pushed.stderr)
    );
}
