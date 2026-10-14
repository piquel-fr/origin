# origin

A modern, git-compatible source control system, written in Rust.

**Status:** early prototype. Nothing here is stable, and git compatibility
has not started.

## What works

```sh
origin init                       # a repository in the current directory
origin status                     # no staging area: every change counts
origin diff
origin commit -m "First"
origin log
```

Hosting repositories:

```sh
origin repo create repos/demo --description "A demo"
cat > origin-server.toml <<'EOF'
listen = "127.0.0.1:7878"

[repos]
demo = "repos/demo"

[users.alice]
token = "a long random secret"
groups = ["dev"]
EOF
origin serve
```

Working with them (`ORIGIN_TOKEN`, or `[tokens]` in
`~/.config/origin/config.toml`, authenticates you):

```sh
origin clone http://127.0.0.1:7878/demo   # the latest commit only; history is lazy
origin push                               # all branches move, or none do
origin pull                               # fast-forwards only, for now
origin permissions grant secrets all none
origin permissions grant secrets user:alice write
```

Files you may not read are withheld: they keep their place in the tree, but
their content never reaches you. Repository paths work as remotes too, served
in-process without a server.

The server's HTTP API is documented in
[`docs/openapi.json`](docs/openapi.json), and `crates/origin-client` is its
Rust SDK. [`docs/benchmarks.md`](docs/benchmarks.md) compares origin with git.

## Development

```sh
just             # format, lint, test, and audit dependencies
just openapi     # rewrite docs/openapi.json after API changes
just bench       # micro-benchmarks
just bench-git   # origin against git, with hyperfine (install it first)
```

rustup installs the toolchain `rust-toolchain.toml` pins. The minimum
supported Rust version is the `rust-version` in `Cargo.toml`.
