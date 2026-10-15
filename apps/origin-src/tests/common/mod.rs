//! Helpers for tests driving the real `origin` binary.

// Each test binary uses a different subset of the helpers.
#![allow(dead_code)]

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

/// Runs `origin` in `cwd` as a configured user.
pub fn origin(cwd: &Path, args: &[&str]) -> Output {
    origin_as(cwd, None, args)
}

/// Runs `origin` in `cwd` as a configured user, authenticating to servers
/// with `token`.
pub fn origin_as(cwd: &Path, token: Option<&str>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_origin"));
    command
        .args(args)
        .current_dir(cwd)
        .env("ORIGIN_USER_NAME", "Ada Lovelace")
        .env("ORIGIN_USER_EMAIL", "ada@example.com")
        // Keep the user's own configuration and tokens out of tests.
        .env("XDG_CONFIG_HOME", cwd)
        .env_remove("ORIGIN_TOKEN");
    if let Some(token) = token {
        command.env("ORIGIN_TOKEN", token);
    }
    command.output().unwrap()
}

/// A temporary working copy.
pub struct Repo {
    dir: tempfile::TempDir,
    token: Option<String>,
}

impl Repo {
    /// A new, empty repository.
    pub fn init() -> Self {
        Self::init_as(None)
    }

    /// A new, empty repository whose user authenticates with `token`.
    pub fn init_as(token: Option<&str>) -> Self {
        let repo = Self {
            dir: tempfile::tempdir().unwrap(),
            token: token.map(str::to_owned),
        };
        fs::create_dir(repo.path()).unwrap();
        repo.ok(&["init"]);
        repo
    }

    /// A clone of `url`.
    pub fn clone(url: &str) -> Self {
        Self::clone_as(url, None)
    }

    /// A clone of `url` by a user authenticating with `token`.
    pub fn clone_as(url: &str, token: Option<&str>) -> Self {
        let repo = Self {
            dir: tempfile::tempdir().unwrap(),
            token: token.map(str::to_owned),
        };
        let output = origin_as(repo.dir.path(), token, &["clone", url, "work"]);
        assert!(
            output.status.success(),
            "clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        repo
    }

    /// The root of the working copy.
    pub fn path(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    /// Runs a command that must succeed, returning its output.
    pub fn ok(&self, args: &[&str]) -> String {
        let output = origin_as(&self.path(), self.token.as_deref(), args);
        assert!(
            output.status.success(),
            "`origin {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// Runs a command that must fail, returning its error.
    pub fn fails(&self, args: &[&str]) -> String {
        let output = origin_as(&self.path(), self.token.as_deref(), args);
        assert!(
            !output.status.success(),
            "`origin {}` succeeded",
            args.join(" ")
        );
        String::from_utf8(output.stderr).unwrap()
    }

    pub fn write(&self, path: &str, content: &str) {
        let path = self.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    pub fn read(&self, path: &str) -> String {
        fs::read_to_string(self.path().join(path)).unwrap()
    }

    pub fn exists(&self, path: &str) -> bool {
        self.path().join(path).exists()
    }

    pub fn remove(&self, path: &str) {
        fs::remove_file(self.path().join(path)).unwrap();
    }

    /// The commit messages of the current branch, newest first.
    pub fn log(&self) -> Vec<String> {
        self.ok(&["log"])
            .lines()
            .filter_map(|line| line.strip_prefix("    "))
            .map(str::to_owned)
            .collect()
    }
}

/// An empty server repository, reached over HTTP through `origin serve`, or
/// through its path.
pub struct Remote {
    _dir: tempfile::TempDir,
    server: Option<Child>,
    /// What to clone or push to.
    pub url: String,
}

impl Remote {
    fn create() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let output = origin(dir.path(), &["repo", "create", "demo"]);
        assert!(output.status.success());
        dir
    }

    /// A repository on a local path.
    pub fn path() -> Self {
        let dir = Self::create();
        let url = dir.path().join("demo").to_string_lossy().into_owned();
        Self {
            _dir: dir,
            server: None,
            url,
        }
    }

    /// A repository served over HTTP by a running `origin serve`.
    pub fn http() -> Self {
        Self::guarded("", "")
    }

    /// A repository served over HTTP to `users` (the `[users]` tables of
    /// the server configuration), with `permissions` (the `[permissions]`
    /// tables of `repo.toml`).
    pub fn guarded(users: &str, permissions: &str) -> Self {
        let dir = Self::create();
        let config = dir.path().join("demo/repo.toml");
        let repo = fs::read_to_string(&config).unwrap();
        fs::write(config, format!("{repo}\n{permissions}")).unwrap();
        fs::write(
            dir.path().join("server.toml"),
            format!("listen = \"127.0.0.1:0\"\n[repos]\ndemo = \"demo\"\n{users}"),
        )
        .unwrap();
        let mut server = Command::new(env!("CARGO_BIN_EXE_origin"))
            .args(["serve", "--config", "server.toml"])
            .current_dir(dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(server.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let base = line.trim().strip_prefix("Listening on ").unwrap();
        Self {
            _dir: dir,
            server: Some(server),
            url: format!("{base}/demo"),
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        if let Some(server) = &mut self.server {
            let _ = server.kill();
            let _ = server.wait();
        }
    }
}
