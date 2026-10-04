# Belvedere

A private, self-contained virtual assistant for the COSMIC desktop.

Belvedere runs in the background, reads your Thunderbird mail and calendar, turns what it finds into to-dos and reminders, and lets you chat with it about your schedule. Everything runs locally, including the AI model. Nothing leaves your machine.

Built for one person's setup. It is public so others can read it, not as a supported product.

## Status

Early development. Nothing works yet.

## Building

Requires Rust (the version in `rust-toolchain.toml` is picked up automatically by `rustup`) and `just`:

```sh
cargo install just
```

System packages, on Pop!_OS or Ubuntu:

```sh
sudo apt install build-essential pkg-config cmake clang \
  libxkbcommon-dev libwayland-dev libinput-dev libudev-dev \
  libgbm-dev libseat-dev libpixman-1-dev libdbus-1-dev \
  libvulkan-dev libfontconfig-dev libfreetype-dev libexpat1-dev \
  libssl-dev libsqlite3-dev
```

Then:

```sh
just build   # compile everything
just test    # run the tests
just lint    # formatting and clippy, same as CI
just         # list every recipe
```

## Installing

```sh
just install     # service (starts at login), window (in the launcher), panel applet
just uninstall   # remove all three; keeps your data
```

Each piece can also be installed on its own: `just install-service`, `just install-app`, `just install-applet`. After installing the applet, add "Belvedere" to the panel from COSMIC Settings → Desktop → Panel → Configure panel applets.

Logs go to the journal: `journalctl --user -u belvedere -f`.

## License

GPL-3.0. See [LICENSE](LICENSE).
