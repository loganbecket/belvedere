# Belvedere

A private, self-contained butler for the COSMIC desktop.

Belvedere runs in the background, reads your Thunderbird mail and calendar, turns bills, deadlines, and requests into tasks with reminders, briefs you in the morning, and lets you chat with it about your schedule, your mail, and your files. Everything runs on your own machine, including the language model. Nothing leaves it.

Built for one person's setup, in public so others can read it. Not a supported product.

## What it does

- Reads new mail from every Thunderbird folder (except junk, trash, drafts, sent, outbox, and templates, which you can change) and, with a local model, decides whether each message needs something from you: a bill to pay, a deadline, a reply, an appointment, a renewal. Confident findings become tasks with reminders and a notification; unsure ones become suggestions you accept or reject.
- Follow-up emails about the same matter update the task instead of making another; a payment confirmation marks the task done (with Undo); replying to an email closes its "reply needed" task; standing rules you write in plain words change how certain mail is treated.
- Reads your Thunderbird calendar and reminds you before events, unless Thunderbird already will.
- Shows a morning briefing at 8:00: overdue, due today, today's events, coming up.
- Serves its tasks to Thunderbird as a calendar you subscribe to once, and takes changes back from Thunderbird.
- Chat can list and add tasks, answer schedule questions, search your mail, find and read your files, make a task from a bill in a file, and take standing rules.
- Models come from LM Studio, Ollama, a file you point it at, or a download from Hugging Face started by you.

## Requirements

- Pop!_OS with the COSMIC desktop (other Linux desktops with systemd user sessions and a notification daemon should work; the panel applet is COSMIC-only).
- Thunderbird, as the Flatpak (ESR or regular), a native package, or a Snap. Belvedere finds the default profile on its own.
- A graphics chip that Vulkan can use, or patience: the model runs on the CPU otherwise. A 4-billion-parameter model is the default; about 30 GB of memory is comfortable.
- Rust (the version in `rust-toolchain.toml` is picked up by `rustup`) and `just`:

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

`glslc` and `spirv-headers` are needed only while building: they compile the model library's graphics-chip code once. The installed Belvedere never calls them.

## Install

```sh
git clone https://github.com/loganbecket/belvedere.git
cd belvedere
just install
```

That builds everything in release mode and puts in place:

- `belvedered`, the background service, in `~/.local/bin`, with a systemd user unit at `~/.config/systemd/user/belvedere.service`, enabled and started. It starts at every login from then on.
- `belvedere-model`, the helper that runs the model in its own low-priority process. The service starts it when a model is needed and stops it after five idle minutes.
- `belvedere`, the window, in the launcher as "Belvedere".
- `belvedere-applet`, the panel applet. Add it from COSMIC Settings → Desktop → Panel → Configure panel applets.

`just check-install` lists what is in place and whether the service is enabled and running. Each piece can be installed alone: `just install-service`, `just install-app`, `just install-applet`.

On first run Belvedere scans the last 14 days of mail; with a few hundred messages that takes a few minutes of low-priority work and produces the first tasks. Logs go to the journal: `journalctl --user -u belvedere -f`.

## The one-time Thunderbird calendar setup

Belvedere's tasks can show up in Thunderbird's task list, and changes you make there flow back. The window's Thunderbird section shows an address, a user name, and a password with Copy buttons; then, in Thunderbird:

1. Open the Calendar tab, right-click the calendar list, and choose New Calendar.
2. Pick "On the Network", then CalDAV. Paste the address as the location and the user name as the user.
3. When Thunderbird asks for the password, paste it and let Thunderbird remember it.
4. Name the calendar Belvedere. In its properties, set Refresh to every minute so changes show quickly.

The calendar server listens only on this machine (127.0.0.1) and only answers with the password. Belvedere never edits Thunderbird's files to set this up.

## Settings

The window's Settings section covers how often mail is re-checked, which folders are never read for tasks, the confidence needed to make a task outright, the morning briefing and its time, the default time for dated tasks, reminder lead times, event reminders, how long an idle model stays loaded, and extra places file search must never open. Changes take effect at once and survive restarts.

## Uninstall

```sh
just uninstall         # removes the service, window, applet, desktop entries, and icons; keeps your data
just uninstall purge   # also removes the data folder: tasks, settings, downloaded models, the calendar password
```

Your data lives in `~/.local/share/belvedere` (or `$XDG_DATA_HOME/belvedere`). Nothing else is written anywhere.

## What Belvedere reads, and what it never writes

Reads, from disk only:

- Thunderbird's mail files, calendar databases, and search index, from snapshot copies or read-only handles. Reading an email never changes its read or unread status, in Thunderbird or on the server.
- Your calendar events and Thunderbird's own tasks.
- Files in your home folder when you ask chat to find or read them, by name and date for finding, by contents for reading. Places that hold passwords and keys (SSH and GPG keys, password stores and keyrings, browser and Thunderbird profiles, cloud credentials, `.env` files, and more) are never entered or listed.
- Model files in LM Studio's and Ollama's folders.

Never writes to:

- Thunderbird's profile or any of its files. Tasks reach Thunderbird only through Belvedere's own calendar server, which Thunderbird subscribes to.
- LM Studio's or Ollama's folders, or any file of yours. Removing a model from Belvedere deletes a file only when Belvedere downloaded or copied it into its own folder.

Network: nothing goes out, except a download from Hugging Face that you start in the window. The only thing listening is the calendar server, on this machine only, behind its password.

Everything the model acts on (your mail, files, calendar) is treated as untrusted input: instructions inside an email or a file are never followed. Deleting a task asks first and can be undone.

## Developing

```sh
just build        # compile everything
just test         # run the tests
just lint         # formatting and clippy, same as CI
just ci           # lint and test together
just eval <suite> # score the model against the cases in evals/<suite>
just              # list every recipe
```

`CLAUDE.md` describes how work on this repository is organized.

## License

GPL-3.0. See [LICENSE](LICENSE).
