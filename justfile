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
    cargo build --release -p belvedered -p belvedere-model
    install -Dm755 target/release/belvedered "{{bin_dir}}/belvedered"
    install -Dm755 target/release/belvedere-model "{{bin_dir}}/belvedere-model"
    install -Dm644 packaging/systemd/belvedere.service "{{unit_dir}}/belvedere.service"
    systemctl --user daemon-reload
    systemctl --user enable belvedere.service
    systemctl --user restart belvedere.service
    systemctl --user --no-pager status belvedere.service

app_dir := home_directory() / ".local/share/applications"
icon_dir := home_directory() / ".local/share/icons/hicolor/scalable/apps"

# Build the window app in release mode and put it in the launcher
install-app:
    cargo build --release -p belvedere
    install -Dm755 target/release/belvedere "{{bin_dir}}/belvedere"
    install -Dm644 packaging/org.belvedere.Belvedere.desktop "{{app_dir}}/org.belvedere.Belvedere.desktop"
    install -Dm644 packaging/icons/hicolor/scalable/apps/org.belvedere.Belvedere.svg "{{icon_dir}}/org.belvedere.Belvedere.svg"
    -update-desktop-database "{{app_dir}}" 2>/dev/null

# Build the panel applet in release mode and make it available to the COSMIC panel
install-applet:
    cargo build --release -p belvedere-applet
    install -Dm755 target/release/belvedere-applet "{{bin_dir}}/belvedere-applet"
    install -Dm644 packaging/org.belvedere.Applet.desktop "{{app_dir}}/org.belvedere.Applet.desktop"
    install -Dm644 packaging/icons/hicolor/scalable/apps/org.belvedere.Belvedere-symbolic.svg "{{icon_dir}}/org.belvedere.Belvedere-symbolic.svg"
    -update-desktop-database "{{app_dir}}" 2>/dev/null

# Remove the panel applet
uninstall-applet:
    rm -f "{{bin_dir}}/belvedere-applet" "{{app_dir}}/org.belvedere.Applet.desktop" "{{icon_dir}}/org.belvedere.Belvedere-symbolic.svg"
    -update-desktop-database "{{app_dir}}" 2>/dev/null

# Install everything: service, window, and panel applet
install: install-service install-app install-applet

# Remove everything. Keeps your data.
uninstall: uninstall-applet uninstall-app uninstall-service

# Remove the window app from the launcher
uninstall-app:
    rm -f "{{bin_dir}}/belvedere" "{{app_dir}}/org.belvedere.Belvedere.desktop" "{{icon_dir}}/org.belvedere.Belvedere.svg"
    -update-desktop-database "{{app_dir}}" 2>/dev/null

# Stop the service and remove it
uninstall-service:
    -systemctl --user disable --now belvedere.service
    rm -f "{{unit_dir}}/belvedere.service" "{{bin_dir}}/belvedered" "{{bin_dir}}/belvedere-model"
    systemctl --user daemon-reload
