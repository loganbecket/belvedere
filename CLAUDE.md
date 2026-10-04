# Working on Belvedere

Belvedere is a personal assistant for the COSMIC desktop, written in Rust. It is built for one person's machine, in public.

## The plan is the contract

- `PLAN.md` in this folder (local only, not committed) lists every chunk of work in order. Work only on the chunk Logan names ("start chunk 0.1"). If `PLAN.md` is missing, stop and ask for it.
- Read the chunk's Scope, Not in this chunk, and Done when before writing anything. Build exactly the Scope. Nothing from Not in this chunk, nothing from later chunks, no "while I'm here" improvements.
- Work you discover that isn't in the chunk goes in the Parking lot at the bottom of `PLAN.md`, with one line on why. Do not build it.

## One chunk, one branch, one PR

- The default branch is `master`. Never commit to it directly.
- Branch name is the one `PLAN.md` gives the chunk. Branch off a fresh `master`.
- Open the PR with `gh pr create` against `master` when the chunk's Done when items are all met. Logan merges by hand; never merge yourself.
- The PR description is the only thing Logan reads, and he checks nothing by hand. It must contain: the chunk number and title; every Done when item as a checkbox with proof next to it (command output, test names, eval scores, screenshot); and anything that did not get done, said plainly. A ticked box without proof is a lie.

## Baseline gates for every PR

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --workspace` pass. Run them before opening the PR and paste the summary.
- New behavior has tests. A bug fix includes a test that failed before the fix.
- Chunks that depend on model judgment are scored with `just eval <suite>` against made-up fixtures in `evals/`, and the score, model, and quantization go in the PR. Default eval model: Qwen3.5 9B Q4_K_M.

## Hard rules

- Never write to the Thunderbird profile, LM Studio folders, Ollama folders, or any of Logan's files. Read from snapshot copies or read-only handles. Tasks reach Thunderbird through Belvedere's own CalDAV server only.
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
