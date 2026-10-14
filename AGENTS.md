# origin

A modern, git-compatible source control system written in Rust. The repository
currently contains the first prototype.

## Layout

- `crates/origin-shared`: the object model, wire formats, and API contracts
  shared by every component.
- `crates/origin-client`: the SDK, a typed client for origin servers. Each
  endpoint is one line of its `endpoints!` macro.
- `apps/origin-src`: the `origin` binary, both the command line and the
  server.

## API changes

Every server feature lives in `OriginService`; transports like HTTP only
translate requests. A new HTTP endpoint also takes a line in the SDK's
`endpoints!` macro, and `just openapi` to update `docs/openapi.json`.

## Testing & Benchmarks

Performance is a primary goal, so benchmarks matter as much as tests. A change
to a hot path (hashing, storage, snapshots, transfer) should come with a
benchmark, and its numbers in the pull request.

## Commits & Pull Requests

Titles follow conventional commits with a scope, e.g.
`feat(server): add transactional push`.
