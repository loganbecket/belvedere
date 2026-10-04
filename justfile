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

bin_dir := home_directory() / ".local/bin"
unit_dir := home_directory() / ".config/systemd/user"

# Build the service in release mode, install it, and start it at login
install-service:
    cargo build --release -p belvedered
    install -Dm755 target/release/belvedered "{{bin_dir}}/belvedered"
    install -Dm644 packaging/systemd/belvedere.service "{{unit_dir}}/belvedere.service"
    systemctl --user daemon-reload
    systemctl --user enable belvedere.service
    systemctl --user restart belvedere.service
    systemctl --user --no-pager status belvedere.service

# Stop the service and remove it
uninstall-service:
    -systemctl --user disable --now belvedere.service
    rm -f "{{unit_dir}}/belvedere.service" "{{bin_dir}}/belvedered"
    systemctl --user daemon-reload
