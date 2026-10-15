# origin

A modern, git-compatible source control system, written in Rust.

**Status:** early prototype. Nothing here is stable.

## Development

```sh
just         # format, lint, test, and audit dependencies
just bench   # micro-benchmarks
```

rustup installs the toolchain `rust-toolchain.toml` pins. The minimum
supported Rust version is the `rust-version` in `Cargo.toml`.
