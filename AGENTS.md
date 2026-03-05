# Raffi Repository Guide

Repository-local instructions for coding agents working in this tree.

## First Read

- Read `CLAUDE.md` before changing code. Today it adds one repo-specific rule: update docs and `examples/raffi.yaml` when behavior changes.
- Do not modify git history or create commits unless the user explicitly asks.
- The worktree may already contain user edits. Do not revert unrelated changes.

## Project Shape

- Rust application code lives in `src/`.
- CLI entrypoint is `src/main.rs`; runtime orchestration and config parsing live in `src/lib.rs`.
- UI backends live in `src/ui/`:
  - `src/ui/fuzzel.rs` for the external `fuzzel` integration.
  - `src/ui/wayland*` for the native iced UI and addons.
- User-facing examples live in `examples/`.
- Documentation site lives in `website/` and is built with Astro Starlight.
- Release and CI workflows live in `.github/workflows/`.

## Expected Workflow

- Prefer the existing task runner when it matches the task:
  - `make build`
  - `make test`
  - `make clippy`
  - `make sanity`
- If you run Rust commands directly, mirror CI:
  - `cargo check`
  - `cargo test`
  - `cargo fmt --all -- --check`
  - `cargo clippy -- -D warnings`
- For docs site work, use the existing `website/package.json` scripts.
- Use `uv` for Python tooling when Python is necessary. Use `python3`, not `python`.

## Change Coupling

When changing behavior, keep the surrounding docs and examples aligned.

- Config format, CLI flags, addon behavior, or screenshots/docs references:
  - update `README.md` when the top-level project description changes
  - update relevant files under `website/src/content/docs/`
  - update `examples/raffi.yaml` if the example config should reflect the new behavior
- Config schema changes:
  - refresh `examples/raffi-schema.json`
- Native UI behavior changes:
  - check `src/ui/wayland/tests.rs` and add or adjust tests
- Sorting, history, MRU, or search behavior changes:
  - check `src/ui/wayland/support.rs` tests as well as the app tests

## Architecture Notes

- `run()` in `src/lib.rs` is the main runtime path:
  - resolve config path
  - optionally clear caches
  - auto-write `raffi-schema.json` next to the user config if missing
  - read and normalize config
  - choose UI backend
  - execute the selected launcher or script
- Config loading supports migration from the old flat format to the current v1 `launchers:` format.
- The native UI is feature-gated by the `wayland` Cargo feature. A no-default-features build is intentionally smaller and only supports the `fuzzel` path.

## Validation Expectations

- Run the smallest relevant verification set for the files you changed.
- For docs-only changes, say that you did not run Rust tests if you skipped them.
- For code changes, prefer at least targeted `cargo test`, and run broader checks when the change touches shared code paths.

## Documentation Deliverables

Before finishing, report:

- commands/tests run and their results
- files changed
- remaining risks, TODOs, or anything not verified
