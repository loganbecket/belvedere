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

# Score a suite of eval cases against the model: just eval chat-tasks
eval suite *args:
    cargo build --release -p belvedere-model -p belvedere-eval
    cargo run --release -p belvedere-eval -- --suite {{suite}} {{args}}

# Everything CI checks, in one go
ci: lint test

bin_dir := home_directory() / ".local/bin"
unit_dir := home_directory() / ".config/systemd/user"
data_dir := env("XDG_DATA_HOME", home_directory() / ".local/share") / "belvedere"
# Tests point this at `true` to exercise the file steps without touching systemd.
systemctl := env("BELVEDERE_SYSTEMCTL", "systemctl")

# Build the service in release mode, install it, and start it at login
install-service:
    cargo build --release -p belvedered -p belvedere-model
    install -Dm755 target/release/belvedered "{{bin_dir}}/belvedered"
    install -Dm755 target/release/belvedere-model "{{bin_dir}}/belvedere-model"
    install -Dm644 packaging/systemd/belvedere.service "{{unit_dir}}/belvedere.service"
    {{systemctl}} --user daemon-reload
    {{systemctl}} --user enable belvedere.service
    {{systemctl}} --user restart belvedere.service
    -{{systemctl}} --user --no-pager status belvedere.service

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

# Remove everything. Keeps your data; `just uninstall purge` removes the data folder too
# (tasks, settings, downloaded models, the calendar password).
uninstall *mode: uninstall-applet uninstall-app uninstall-service
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "{{mode}}" = "purge" ]; then
        rm -rf "{{data_dir}}"
        echo "removed {{data_dir}}"
    else
        echo "kept your data in {{data_dir}}; run 'just uninstall purge' to remove it"
    fi

# Check what an install left in place (or that an uninstall left nothing)
check-install:
    #!/usr/bin/env bash
    set -euo pipefail
    status=0
    for f in "{{bin_dir}}/belvedered" "{{bin_dir}}/belvedere-model" "{{bin_dir}}/belvedere" "{{bin_dir}}/belvedere-applet" \
             "{{unit_dir}}/belvedere.service" "{{app_dir}}/org.belvedere.Belvedere.desktop" "{{app_dir}}/org.belvedere.Applet.desktop" \
             "{{icon_dir}}/org.belvedere.Belvedere.svg" "{{icon_dir}}/org.belvedere.Belvedere-symbolic.svg"; do
        if [ -e "$f" ]; then echo "present  $f"; else echo "missing  $f"; status=1; fi
    done
    if command -v desktop-file-validate >/dev/null; then
        desktop-file-validate "{{app_dir}}/org.belvedere.Belvedere.desktop" && echo "window desktop entry valid"
        # COSMIC's own applets use the unregistered "COSMIC" category too; the validator's note about it is expected.
        desktop-file-validate "{{app_dir}}/org.belvedere.Applet.desktop" 2>&1 | grep -v "COSMIC" || echo "applet desktop entry valid (COSMIC category as System76's applets use)"
    fi
    if [ "{{systemctl}}" = "systemctl" ]; then
        echo "service: $({{systemctl}} --user is-enabled belvedere.service 2>/dev/null || true) / $({{systemctl}} --user is-active belvedere.service 2>/dev/null || true)"
    fi
    exit $status

# Remove the window app from the launcher
uninstall-app:
    #!/usr/bin/env bash
    set -euo pipefail
    rm -f "{{bin_dir}}/belvedere" "{{app_dir}}/org.belvedere.Belvedere.desktop" "{{icon_dir}}/org.belvedere.Belvedere.svg"
    update-desktop-database "{{app_dir}}" 2>/dev/null || true
    # The desktop cache is shared with other apps; drop it only when nothing else is there.
    if [ -d "{{app_dir}}" ] && ! ls "{{app_dir}}"/*.desktop >/dev/null 2>&1; then
        rm -f "{{app_dir}}/mimeinfo.cache"
    fi

# Stop the service and remove it
uninstall-service:
    -{{systemctl}} --user disable --now belvedere.service
    rm -f "{{unit_dir}}/belvedere.service" "{{bin_dir}}/belvedered" "{{bin_dir}}/belvedere-model"
    {{systemctl}} --user daemon-reload
