#!/usr/bin/env bash
# Exercises `just install`, `just uninstall`, and `just uninstall purge` against a
# throwaway home folder, without touching this account's systemd units. Prints
# what each step left behind. Run from the repository root.
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
home="$(mktemp -d)"
trap 'rm -rf "$home"' EXIT
real_home="$HOME"
# Keep the Rust toolchain where it is; only Belvedere's own files go into the throwaway home.
export RUSTUP_HOME="${RUSTUP_HOME:-$real_home/.rustup}" CARGO_HOME="${CARGO_HOME:-$real_home/.cargo}"
export HOME="$home" XDG_DATA_HOME="$home/.local/share" BELVEDERE_SYSTEMCTL=true
cd "$repo"
echo "== install into $home"
just install >/dev/null
just check-install
mkdir -p "$XDG_DATA_HOME/belvedere" && echo "pretend data" > "$XDG_DATA_HOME/belvedere/belvedere.db"
echo "== uninstall (keep data)"
just uninstall >/dev/null
left="$(find "$home/.local" "$home/.config" -type f 2>/dev/null | sort)"
echo "left behind:"; echo "$left" | sed "s#$home#~#"
test "$left" = "$home/.local/share/belvedere/belvedere.db" || { echo "unexpected leftovers"; exit 1; }
echo "== install again, then uninstall purge"
just install >/dev/null
just uninstall purge >/dev/null
left="$(find "$home/.local" "$home/.config" -type f 2>/dev/null | sort)"
if [ -n "$left" ]; then echo "left behind after purge:"; echo "$left"; exit 1; fi
echo "nothing left after purge"
