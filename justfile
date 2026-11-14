default:
    @just --list

# Unit and integration tests (real git repos in tempdirs)
test:
    cargo test --workspace

# Format and lint
check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
