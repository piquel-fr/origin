# origin

Version control for repositories that many people, agents, CI runners and
review bots read and write at the same time, written in Rust.
`docs/design.md` describes the model; this repository holds its prototype.

## Layout

- `crates/origin-shared`: the object model, wire formats, and API contracts
  shared by every component.
- `crates/origin-client`: the HTTP client, generated from the server's OpenAPI
  document. Never edit generated code by hand.
- `apps/origin-src`: the `origin` binary, both the command line and the
  server.

## Testing & Benchmarks

Performance is a primary goal, so benchmarks matter as much as tests. A change
to a hot path (hashing, storage, snapshots, transfer) should come with a
benchmark, and its numbers in the pull request.

## Commits & Pull Requests

Titles follow conventional commits with a scope, e.g.
`feat(server): add transactional push`.
