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

## Running the background service

```sh
just install-service     # build, install to ~/.local/bin, start now and at every login
just uninstall-service   # stop it and remove it
```

Logs go to the journal: `journalctl --user -u belvedere -f`.

## License

GPL-3.0. See [LICENSE](LICENSE).
