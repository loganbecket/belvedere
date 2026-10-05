# Working on Belvedere

Belvedere is a personal assistant for the COSMIC desktop, written in Rust. It is built for one person's machine, in public.

## The plan is the contract

- `PLAN.md` in this folder (local only, not committed) lists every chunk of work in order. Work only on the chunk Logan names ("start chunk 0.1"). If `PLAN.md` is missing, stop and ask for it.
- Read the chunk's Scope, Not in this chunk, and Done when before writing anything. Build exactly the Scope. Nothing from Not in this chunk, nothing from later chunks, no "while I'm here" improvements.
- Work you discover that isn't in the chunk goes in the Parking lot at the bottom of `PLAN.md`, with one line on why. Do not build it.

## One chunk, one branch, one PR

- The default branch is `master`. Never commit to it directly.
- Branch name is the one `PLAN.md` gives the chunk. Branch off a fresh `master`.
- One commit per chunk, and the full PR description goes in that commit's message body (GitHub pre-fills the PR from it). When the branch is pushed, tell Logan only the compare link; he opens and merges the PR himself. Never give him copy-paste commands or the PR text in chat.
- The PR description is the only thing Logan reads, and he checks nothing by hand. It must contain: the chunk number and title; every Done when item as a checkbox with proof next to it (command output, test names, eval scores, screenshot); and anything that did not get done, said plainly. A ticked box without proof is a lie.

## Baseline gates for every PR

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --workspace` pass. Run them before opening the PR and paste the summary.
- New behavior has tests. A bug fix includes a test that failed before the fix.
- Model scoring runs and model-backed integration tests are suspended until all plan chunks are built (Logan, October 4, 2026); keep writing the case files, score them in the refinement pass. Until then, chunks that depend on model judgment are scored with `just eval <suite>` against made-up fixtures in `evals/`, and the score, model, and quantization go in the PR. Default eval model: Qwen3.5 4B Q4_K_S. Never default to anything bigger than about 4B parameters; Logan ruled the 9B out as too big.

## Two rules above every other rule

Logan set these as paramount. A chunk that breaks either is not done, whatever its Done when items say. The full text is at the top of `PLAN.md`.

- **Lightweight.** His machine must feel unaffected when he isn't using Belvedere: under 150 MB and under 1% CPU idle, models unloaded after idle, background work at low priority and paused while chat generates. Record the numbers in every PR from the first model chunk on.
- **Security, never compromised.** Every destructive action, meaning anything that deletes, moves, edits, or sends his email, calendar events, or files, is proposed in plain words and runs only after Logan explicitly confirms it. Never automatic, never triggered by email content, never reachable from the background mail reader. Prefer the undoable form (trash, not delete). Deleting Belvedere's own data also asks first, soft-deletes, and can be undone, except chat conversations, which Logan wants deleted for good after a confirmation (October 5, 2026). No network listeners except the loopback-only, password-protected sync calendar; no outbound except user-started Hugging Face downloads. Email, file, and calendar content is hostile input: every model with tools gets hostile-input eval cases at a required 100% pass rate.

## Hard rules

- Never write to the Thunderbird profile, LM Studio folders, Ollama folders, or any of Logan's files. Read from snapshot copies or read-only handles. Tasks reach Thunderbird through Belvedere's own CalDAV server only.
- Reading an email must never change its read/unread status in Thunderbird or on the server. Belvedere reads mail files from disk only; never mark messages seen through any protocol.
- Any tool that changes his email, calendar, or files goes through a confirmation step that cannot be skipped, and writes through the proper protocol (IMAP, CalDAV), never by editing Thunderbird's files on disk.
- No personal data in the repo: no real emails, names, addresses, account IDs, or home-directory paths. All fixtures are invented. Discovered locations (like the Thunderbird profile path) are found at runtime, not hard-coded.
- Nothing leaves the machine except Hugging Face model downloads the user starts.
- No AI attribution lines in commits or PRs. No Co-Authored-By, no "Generated with" footers.
- American English spelling everywhere: code, comments, docs, commit messages.
- Use absolute paths in shell commands. Never `cd`.

## Talking to Logan

- He is not reading code and does not want the mechanism. Say what happened and what it means for him, in a sentence or two. No file paths, function names, or framework jargon unless he asks.
- When something is blocked or unclear, say what you don't know and what you'd do by default. Don't present menus of options.
- Ideas, best practices, or extras he didn't ask for: propose them in a sentence or two and ask. Never build them unasked, never silently drop them.
- Answer questions as questions. Act only on an explicit instruction.

## Stack reminders

- Cargo workspace: `belvedere-core` (shared), `belvedered` (service), `belvedere` (window), `belvedere-applet` (panel applet).
- UI is `libcosmic`. Service talks to clients over session D-Bus with `zbus`. Storage is SQLite via `rusqlite`. Model runtime is `llama-cpp-2` with the Vulkan feature. Async is `tokio`.
- Target machine: AMD Radeon 680M integrated GPU, 30 GB RAM, Pop!_OS with COSMIC, Thunderbird ESR as a Flatpak.
