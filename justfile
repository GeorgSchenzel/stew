default:
    @just --list

# Unit and integration tests (real git repos in tempdirs; hc from the dev shell, isolated in
# each test's own dirs). The build first: the runtime tests run the in-box client.
test:
    cargo build --workspace
    cargo test --workspace

# Format and lint
check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
