//! Helpers for tests driving the real `origin` binary.

// Each test binary uses a different subset of the helpers.
#![allow(dead_code)]

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

/// Runs `origin` in `cwd` as a configured user.
pub fn origin(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_origin"))
        .args(args)
        .current_dir(cwd)
        .env("ORIGIN_USER_NAME", "Ada Lovelace")
        .env("ORIGIN_USER_EMAIL", "ada@example.com")
        .output()
        .unwrap()
}

/// A temporary working copy.
pub struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    /// A new, empty repository.
    pub fn init() -> Self {
        let repo = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir(repo.path()).unwrap();
        repo.ok(&["init"]);
        repo
    }

    /// A clone of `url`.
    pub fn clone(url: &str) -> Self {
        let repo = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        let output = origin(repo.dir.path(), &["clone", url, "work"]);
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
        let output = origin(&self.path(), args);
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
        let output = origin(&self.path(), args);
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
        let dir = Self::create();
        fs::write(
            dir.path().join("server.toml"),
            "listen = \"127.0.0.1:0\"\n[repos]\ndemo = \"demo\"\n",
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
