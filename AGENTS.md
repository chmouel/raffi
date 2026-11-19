# Raffi

Raffi is a YAML-driven application launcher written in Rust, with a `fuzzel`
frontend and a native iced UI with built-in addons. `DESIGN.md` maps the
architecture; user docs live in `website/src/content/docs/` (Astro Starlight).

## Commands

- Build and check: `make build`, `make test`, `make clippy`, `make sanity`.
- After a Rust change, run what CI runs:
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo test`
- Docs site: `cd website && npm run dev` or `npm run build`.
- Python tooling goes through `uv run`; call `python3`, never `python`.

## Layout

- `src/lib.rs` holds CLI args, config loading, and `run()`. Old flat configs
  are migrated to the v1 `launchers:` format on load, so keep both readable.
- `src/ui/fuzzel.rs` is the fuzzel backend; `src/ui/wayland.rs` and
  `src/ui/wayland/` are the native UI and its addons.
- The native UI sits behind the `wayland` Cargo feature (on by default).
  `--no-default-features` builds a fuzzel-only binary, so code outside
  `src/ui/wayland*` must still compile without it.

## Keep related files in sync

A behavior change is not done until these match it:

- Config, CLI flags, or addon behavior: the relevant page under
  `website/src/content/docs/`, the config reference (below), and `README.md`
  if the top-level description changes.
- Config structs: regenerate both checked-in schemas with
  `cargo run -- --generate-schemas --configfile examples/raffi.yaml`, and the
  reference config with `cargo run -- --print-config > examples/raffi.full.yaml`.
  The schema and reference tests in `src/lib.rs` fail when they drift.
- Native UI behavior: tests in `src/ui/wayland/tests.rs`. Sorting, history,
  MRU, and search changes also have tests in `src/ui/wayland/support.rs`.
- Visible UI changes: re-shoot the docs screenshots with
  `make screenshots SHOTS="name ..."`, adding the shot to
  `misc/screenshots/catalogue.py` if it is new (see
  `misc/screenshots/README.md`).

## Example configs

- `examples/raffi.full.yaml` is the complete reference. Every new or changed
  setting belongs there, with its values, default, and what it does. It is
  generated from the `///` doc comments on the config structs in
  `src/lib.rs`, so write the detail in those comments, state defaults as
  `(default: X)`, and regenerate. Never edit the file by hand.
- `examples/raffi.yaml` is a short starter for new users. Do not touch it as
  part of a feature, a new setting, or a docs sync. Change it only when the
  starter itself is wrong (a renamed key, a broken example, a failing test),
  and ask the user before changing it, explaining why.
- `examples/launchers.yaml` follows the same rule as the starter: about ten
  generic entries, one per launcher feature, using placeholders and current,
  widely used Wayland tools (btop, satty, swaync), never a personal setup.
  Document a new launcher field in the docs and the full reference. Add an
  entry here only to show a field no entry covers yet, and ask the user
  first.

## Conventions

- Switching views or modes inside a list always uses Ctrl+Tab, with
  Ctrl+Shift+Tab going backwards. Hook the new view into `handle_cycle_view`
  and `cycle_view_label` in `src/ui/wayland/app.rs`, so the footer shows the
  current view and the help lists it. Do not add a one-off key for this. If a
  request asks for a different key, remind the requester of this rule and
  use Ctrl+Tab unless they insist.

## Gotchas

- Unit tests must not write to `~/.cache/raffi`. `get_mru_cache_path()` in
  `src/ui/mod.rs` errors under `cfg!(test)`, which disables the usage caches in
  tests; derive any new cache path from it to keep that guarantee.
- The main view reads `window_session` from compositor env vars such as
  `NIRI_SOCKET`. Tests that involve recent window picks need to pin
  `app.main_view.window_session`, or they fail on a developer's desktop.
- iced's `text_input` loses focus on Escape. A `Message::Cancel` path that
  keeps raffi open has to re-focus the search input, as `close_help` does in
  `src/ui/wayland/app.rs`, or typing stops working.
- To run the native UI headlessly, use Xvfb with `DISPLAY` set and
  `WAYLAND_DISPLAY` unset. Headless sway fails because it has no seat pointer.
  Window transparency only works with the GPU renderer.

## Finishing

Run the smallest checks that cover the change. Report the commands you ran and
their results, and say what was not verified, for example "docs only, no cargo
tests run".
