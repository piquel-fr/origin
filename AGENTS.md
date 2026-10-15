# origin

A modern, git-compatible source control system written in Rust. The repository
currently contains the first prototype.

## Layout

- `crates/origin-shared`: the object model, wire formats, and API contracts
  shared by every component.
- `crates/origin-platform`: everything that differs between platforms
  (durable writes, file locks, copy-on-write clones, positioned reads,
  refusing network file systems), behind traits. It is the only place whose
  code differs by target OS. `std::os::unix`, which every supported platform
  has, may be used anywhere.
- `crates/origin-client`: the HTTP client, generated from the server's OpenAPI
  document. Never edit generated code by hand.
- `apps/origin-src`: the `origin` binary, both the command line and the
  server.
- `tools/xtask`: development tasks, run with `cargo xtask`, such as timing
  origin against git (`just bench-git`).

## Testing & Benchmarks

Performance is a primary goal, so benchmarks matter as much as tests. A change
to a hot path (hashing, storage, snapshots, transfer) should come with a
benchmark, and its numbers in the pull request.

- `just bench`: the divan micro-benchmarks (encoding, store writes,
  snapshots, input digests and promotion, landing).
- `just bench-git`: times origin against git on a 20,000-file tree with
  hyperfine, and rewrites `docs/benchmarks.md`.

End-to-end tests run the `origin` binary with `ORIGIN_CONFIG_DIR` set to a
temporary directory, which holds the user's `config.toml` and node key.

## Commits & Pull Requests

Titles follow conventional commits with a scope, e.g.
`feat(server): add transactional push`.
