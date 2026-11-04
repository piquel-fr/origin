# origin

Version control for repositories that many people, agents, CI runners and
review bots read and write at the same time, written in Rust. See
[`docs/design.md`](docs/design.md) for the model.

**Status:** early prototype. Nothing here is stable.

## Development

```sh
just   # format, lint, test, and audit dependencies
```

rustup installs the toolchain `rust-toolchain.toml` pins. The minimum
supported Rust version is the `rust-version` in `Cargo.toml`.
