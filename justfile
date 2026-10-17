# Validation formats Rust sources and runs every check CI runs.
default: validate

validate: fmt lint test doc-test deny

build:
    @cargo build --workspace --locked

run *ARGS:
    @cargo run --package origin-src --bin origin -- {{ARGS}}

fmt:
    @cargo fmt --all

# CI checks formatting without modifying sources; local validation uses fmt.
fmt-check:
    @cargo fmt --all -- --check

lint:
    @cargo clippy --locked --workspace --all-targets --all-features -- -D warnings

test:
    @cargo nextest run --locked --workspace --all-features --no-tests=pass

doc-test:
    @cargo test --locked --doc --workspace --all-features

deny:
    @cargo deny check

# Rewrite docs/openapi.json after changing the API. `just test` fails while
# it is stale.
openapi:
    @UPDATE_OPENAPI=1 cargo nextest run --locked --package origin-src --test http openapi

# Serve repositories over HTTP (see `origin serve --help`).
serve *ARGS:
    @cargo run --package origin-src --bin origin -- serve {{ARGS}}

# Micro-benchmarks of the hot paths (hashing, storage, snapshots).
bench *ARGS:
    @cargo bench --locked --workspace -- {{ARGS}}

# Times origin against git with hyperfine, which must be installed, and
# rewrites docs/benchmarks.md.
# Pass `--tree PATH` to benchmark a copy of a real working tree.
bench-git *ARGS:
    @cargo xtask bench-git {{ARGS}}
