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
