# Belvedere task runner. Run `just` to list recipes.

default:
    @just --list

# Build every crate
build:
    cargo build --workspace

# Run every test
test:
    cargo test --workspace

# Formatting and lint checks, the same ones CI runs
lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings

# Run the background service in the foreground
run-service:
    cargo run -p belvedered

# Run the window app
run-app:
    cargo run -p belvedere

# Everything CI checks, in one go
ci: lint test
