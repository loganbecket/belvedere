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
  libssl-dev libsqlite3-dev glslc spirv-headers
```

`glslc` and `spirv-headers` are build-time only: they compile the AI library's graphics-chip code once, during the build. The installed Belvedere never calls them.

Then:

```sh
just build   # compile everything
just test    # run the tests
just lint    # formatting and clippy, same as CI
just eval chat-tasks   # score the model against the cases in evals/chat-tasks
just         # list every recipe
```

## Installing

```sh
just install     # service (starts at login), window (in the launcher), panel applet
just uninstall   # remove all three; keeps your data
```

Each piece can also be installed on its own: `just install-service`, `just install-app`, `just install-applet`. The service install also places `belvedere-model`, the helper that runs AI models in its own low-priority process; the service starts it when a model is needed and stops it after five idle minutes. After installing the applet, add "Belvedere" to the panel from COSMIC Settings → Desktop → Panel → Configure panel applets.

Logs go to the journal: `journalctl --user -u belvedere -f`.

## License

GPL-3.0. See [LICENSE](LICENSE).
