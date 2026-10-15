//! End-to-end tests of permissions: what users see, and what they may
//! change, through the real binary and server.

mod common;

use common::{Remote, Repo};

const USERS: &str = r#"
[users.alice]
token = "alice-token"
[users.bob]
token = "bob-token"
"#;

/// Everyone reads, alice writes everything, bob writes `src`, and only
/// alice sees `secrets`.
const PERMISSIONS: &str = r#"
[permissions."/"]
all = "read"
users.alice = "write"

[permissions."src"]
users.bob = "write"

[permissions."secrets"]
all = "none"
users.alice = "write"
"#;

#[test]
fn users_see_and_change_only_what_they_may() {
    let remote = Remote::guarded(USERS, PERMISSIONS);
    let alice = Repo::init_as(Some("alice-token"));
    alice.write("README.md", "hello\n");
    alice.write("src/lib.rs", "fn lib() {}\n");
    alice.write("secrets/key", "hunter2\n");
    alice.ok(&["commit", "-m", "First"]);
    alice.ok(&["remote", &remote.url]);
    alice.ok(&["push"]);
    assert_eq!(alice.ok(&["whoami"]), "alice\n");

    // Bob gets everything but the secret, and the working copy is clean.
    let bob = Repo::clone_as(&remote.url, Some("bob-token"));
    assert_eq!(bob.read("src/lib.rs"), "fn lib() {}\n");
    assert!(!bob.exists("secrets/key"));
    assert_eq!(bob.ok(&["status"]), "On branch main\nNo changes.\n");

    // Bob may change `src`, and the secret survives his commit.
    bob.write("src/lib.rs", "fn lib() { bob() }\n");
    bob.ok(&["commit", "-m", "Bob's"]);
    bob.ok(&["push"]);
    alice.ok(&["pull"]);
    assert_eq!(alice.read("src/lib.rs"), "fn lib() { bob() }\n");
    assert_eq!(alice.read("secrets/key"), "hunter2\n");

    // Commits touching files bob may not write fail before reaching the
    // server.
    bob.write("README.md", "bob was here\n");
    assert!(
        bob.fails(&["commit", "-m", "Nope"])
            .contains("you may not change `README.md`")
    );

    // Anonymous users read what everyone may.
    let anonymous = Repo::clone(&remote.url);
    assert_eq!(anonymous.read("README.md"), "hello\n");
    assert!(!anonymous.exists("secrets/key"));
    assert_eq!(anonymous.ok(&["whoami"]), "Anonymous.\n");
}

#[test]
fn root_writers_manage_permissions() {
    let remote = Remote::guarded(USERS, PERMISSIONS);
    let bob = Repo::clone_as(&remote.url, Some("bob-token"));
    assert!(
        bob.fails(&["permissions", "grant", "/", "user:bob", "write"])
            .contains("may not write")
    );

    let alice = Repo::clone_as(&remote.url, Some("alice-token"));
    alice.ok(&["permissions", "grant", "/", "user:bob", "write"]);
    let rules = bob.ok(&["permissions"]);
    assert!(
        rules.contains("/  all: read, user:alice: write, user:bob: write"),
        "{rules}"
    );
}

#[test]
fn readable_files_sharing_a_secret_files_content_are_kept() {
    let remote = Remote::guarded(USERS, PERMISSIONS);
    let alice = Repo::init_as(Some("alice-token"));
    alice.write("public", "same\n");
    alice.write("secrets/key", "same\n");
    alice.ok(&["commit", "-m", "First"]);
    alice.ok(&["remote", &remote.url]);
    alice.ok(&["push"]);

    let bob = Repo::clone_as(&remote.url, Some("bob-token"));
    assert_eq!(bob.read("public"), "same\n");
    assert_eq!(bob.ok(&["status"]), "On branch main\nNo changes.\n");
}

#[test]
fn granting_read_access_restores_withheld_files() {
    let remote = Remote::guarded(USERS, PERMISSIONS);
    let alice = Repo::init_as(Some("alice-token"));
    alice.write("secrets/key", "hunter2\n");
    alice.write("secrets/line\nbreak", "odd names are fine\n");
    alice.ok(&["commit", "-m", "First"]);
    alice.ok(&["remote", &remote.url]);
    alice.ok(&["push"]);
    let bob = Repo::clone_as(&remote.url, Some("bob-token"));
    assert!(!bob.exists("secrets/key"));
    assert_eq!(bob.ok(&["status"]), "On branch main\nNo changes.\n");

    alice.ok(&["permissions", "grant", "secrets", "user:bob", "read"]);
    assert_eq!(bob.ok(&["pull"]), "Already up to date.\n");
    assert_eq!(bob.read("secrets/key"), "hunter2\n");
    assert_eq!(bob.read("secrets/line\nbreak"), "odd names are fine\n");
    assert_eq!(bob.ok(&["status"]), "On branch main\nNo changes.\n");
}

#[test]
fn failed_restores_are_retried() {
    use std::os::unix::fs::PermissionsExt;

    let remote = Remote::guarded(USERS, PERMISSIONS);
    let alice = Repo::init_as(Some("alice-token"));
    alice.write("secrets/key", "hunter2\n");
    alice.ok(&["commit", "-m", "First"]);
    alice.ok(&["remote", &remote.url]);
    alice.ok(&["push"]);
    let bob = Repo::clone_as(&remote.url, Some("bob-token"));
    alice.ok(&["permissions", "grant", "secrets", "user:bob", "read"]);

    // A read-only directory where the restored file must go.
    let secrets = bob.path().join("secrets");
    std::fs::create_dir(&secrets).unwrap();
    std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o555)).unwrap();
    bob.fails(&["pull"]);
    std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o755)).unwrap();

    bob.ok(&["pull"]);
    assert_eq!(bob.read("secrets/key"), "hunter2\n");
}
