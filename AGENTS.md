# origin

Version control for repositories that many people, agents, CI runners and
review bots read and write at the same time, written in Rust.
`docs/design.md` describes the model; this repository holds its prototype.

## Layout

- `crates/origin-shared`: the object model, wire formats, and API contracts
  shared by every component.
- `crates/origin-platform`: everything that differs between platforms
  (durable writes, file locks, copy-on-write clones, positioned reads,
  refusing network file systems, killing process groups), behind traits.
  It is the only place whose
  code differs by target OS. `std::os::unix`, which every supported platform
  has, may be used anywhere.
- `crates/origin-client`: the SDK, a typed client for origin servers. Each
  endpoint is one line of its `endpoints!` macro, over the `origin-shared`
  types; no reqwest type is exposed.
- `apps/origin-src`: the `origin` binary, both the command line and the
  server. `OriginService` (`service.rs`) is the one API over a repository:
  the HTTP routes (`http.rs`) and the command line's commands that the API
  offers only translate into its calls, so neither duplicates the other.
  `sync.rs` shares records with a remote (`remote.rs`), which answers the
  same calls over the SDK or, for a local path, in-process. Every call
  takes a `Caller`, authenticated by `auth.rs`; `permissions.rs` filters
  what it reads and checks what it writes, by the rights `policy.rs`
  resolves from `policy.toml`. `plugin.rs` runs `origin <name>` as
  `origin-<name>` from `PATH`, passing the repository in the environment.
- `apps/origin-ci`: the CI plugin, `origin-ci`. It reads `checks.toml`
  from the revision it checks as a tool (`origin ci run`), or as a worker
  answering `check.requested` (`origin ci worker`), from the tip of the
  target landed on, never the candidate. It runs each check in a checkout
  of the revision of its own, and issues the results. Like every plugin, it
  uses only the public API: `Remote`, which is the SDK over HTTP or the
  service in-process, never origin-src's internals.
- `tools/xtask`: development tasks, run with `cargo xtask`, such as timing
  origin against git (`just bench-git`).

## API changes

Every server feature lives in `OriginService`; transports like HTTP only
translate requests. A new HTTP endpoint also takes a line in the SDK's
`endpoints!` macro, and `just openapi` to update `docs/openapi.json`.

## Testing & Benchmarks

Performance is a primary goal, so benchmarks matter as much as tests. A change
to a hot path (hashing, storage, snapshots, transfer) should come with a
benchmark, and its numbers in the pull request.

- `just bench`: the divan micro-benchmarks (encoding, store writes,
  snapshots, input digests and promotion, landing, permission checks,
  reading events, the server's request latency and long-poll wakeups,
  syncing a 20,000-file tree over HTTP, a fetch that brings nothing from a
  long history, and the CI worker's overhead per check).
- `just bench-git`: times origin against git on a 20,000-file tree with
  hyperfine, and rewrites `docs/benchmarks.md`.

End-to-end tests run the `origin` binary with `ORIGIN_CONFIG_DIR` set to a
temporary directory, which holds the user's `config.toml` and node key.
The HTTP tests drive every SDK method against a real server. One fails
while `docs/openapi.json` is stale: `just openapi` rewrites it. The sync
tests clone, push and land through `origin serve` and through local paths;
`apps/origin-src/tests/common` holds the helpers every end-to-end test
shares, plugins' included, which run `origin` from the target directory
their own binary is built in.

## Commits & Pull Requests

Titles follow conventional commits with a scope, e.g.
`feat(server): add transactional push`.
