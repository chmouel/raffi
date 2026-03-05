# Raffi Design

This document describes the current design of Raffi as implemented in the repository today. It is intended as an engineering map for contributors, not a product roadmap.

## Overview

Raffi is a YAML-driven launcher with two runtime frontends:

- `fuzzel` mode: a lightweight external launcher integration
- native mode: an iced-based Wayland UI with built-in addons

At a high level, Raffi:

1. parses CLI arguments
2. loads and normalizes launcher configuration from YAML
3. selects a UI backend
4. returns the chosen item description/binary name from the UI
5. executes the matching command or inline script

## System Layout

```text
src/main.rs
  -> raffi::run(args)
       -> read_config(...)
            -> migrate v0 config if needed
            -> process_config(...)
       -> choose UI backend
       -> ui.show(...)
       -> execute_chosen_command(...)
```

Primary modules:

- `src/lib.rs`: CLI args, config schema/types, config loading, migration, validation, command execution, icon cache
- `src/ui/mod.rs`: backend selection and shared UI settings
- `src/ui/fuzzel.rs`: builds fuzzel input and reads the selected item
- `src/ui/wayland.rs`: launches the native iced application
- `src/ui/wayland/*.rs`: native UI state, routing, rendering, addon logic, tests

## Configuration Model

The current on-disk format is versioned YAML:

```yaml
version: 1
general: ...
addons: ...
launchers:
  name:
    binary: ...
```

Key properties of config processing:

- Old flat configs are migrated in place to v1, with a `.bak` backup.
- Launcher values are deserialized into `RaffiConfig`.
- `~/...` and `${VAR}` references are expanded in launcher fields and addon config.
- Invalid entries are dropped during load rather than shown in the UI.

Launcher validation currently filters entries by:

- binary existence
- script interpreter existence
- `ifenveq`
- `ifenvset`
- `ifenvnotset`
- `ifexist`
- `disabled: true`

The resulting `ParsedConfig` contains:

- `general`: UI/runtime defaults
- `addons`: native UI addon configuration
- `entries`: executable launcher entries

## Backend Selection

Backend selection happens in `run()`:

- explicit CLI `-u/--ui-type` wins
- otherwise `general.ui_type` wins
- otherwise Raffi prefers `fuzzel` if the binary exists in `PATH`
- otherwise, when built with the `wayland` feature, it falls back to the native UI

This split is intentional:

- `fuzzel` mode keeps the binary and runtime surface small
- native mode enables richer interactions that cannot fit the external dmenu-style protocol

## Fuzzel Backend

`src/ui/fuzzel.rs` is intentionally simple.

- It renders each launcher as a single line.
- When icons are enabled, it uses the cached icon map and the `description\0icon\x1fpath` format expected by `fuzzel -d`.
- It passes Raffi's MRU cache path to fuzzel so repeated launches benefit from backend-level ordering.

This backend only returns the chosen launcher label. All execution still happens centrally in `src/lib.rs`.

## Native Backend

The native backend is an iced application rooted in `LauncherApp`.

Important state buckets in `src/ui/wayland/state.rs`:

- launcher list and filtered indices
- current search query and selected row
- icon map and MRU data
- history state
- per-addon transient state:
  - calculator
  - currency
  - script filters
  - text snippets
  - web searches
  - file browser
  - emoji
- view state such as widget ids, theme, font sizes, and keyboard modifiers

The app initializes by:

- loading icons unless `no_icons` is active
- loading MRU and history caches
- sorting launchers using the configured `SortMode`
- focusing the search input
- replaying an initial query if one was provided

## Query Routing

Native mode does not treat every search as a plain fuzzy match. `route_query()` applies a precedence order:

1. file browser if the query looks like `/...`, `~`, or `~/...`
2. script filters by configured keyword
3. text snippets by configured keyword
4. emoji picker by trigger keyword
5. web search by configured keyword
6. standard launcher search

Currency handling is layered on top of that routing and is evaluated separately from the main `QueryMode`.

This structure matters because several addons can use overlapping prefixes. The tests in `src/ui/wayland/tests.rs` lock down the intended precedence.

## Addon Design

Native-only addons are configured under `addons` and activated from the search box.

- Calculator:
  - inline evaluation using `meval`
  - only activates for expression-like input
- Currency converter:
  - parses trigger-based queries such as `$10 to eur`
  - fetches rates over HTTP
  - caches rates in memory with validity checks
- File browser:
  - activates for filesystem-like queries
  - opens files with `xdg-open`
  - supports hidden-file toggling
- Script filters:
  - run external commands and expect Alfred Script Filter style JSON
- Text snippets:
  - source snippets from inline config, files, directories, or command output
- Web searches:
  - fill `{query}` into configured URL templates and open them with `xdg-open`
- Emoji picker:
  - loads cached emoji data or downloads it into the cache directory
  - supports primary and secondary actions such as copy or insert

The design pattern is consistent across addons:

- parse a query into addon-specific state
- optionally spawn async work
- render temporary results inside the single launcher window
- perform the action on submit
- persist search history before exit when appropriate

## Persistence and Cache Files

Raffi stores lightweight state under the XDG cache directory, defaulting to `~/.cache/raffi/`.

Current cache artifacts:

- `icon.cache`: serialized icon-name to icon-path map
- `mru.cache`: launch frequency and recency data
- `history.cache`: prior search queries for native history navigation
- `emoji/`: downloaded emoji datasets

On startup, `run()` also writes `raffi-schema.json` next to the user config if it does not already exist. That improves editor support for YAML authoring.

## Sorting and Ranking

Launcher ordering in native mode is configurable:

- `frequency`
- `recency`
- `hybrid`

MRU data is loaded before UI startup. For hybrid mode, Raffi computes a weighted score from normalized frequency and recency, then sorts launchers before any search query is applied. Fuzzy search is then applied on top of that base set.

## Execution Model

After a selection is returned from either backend, execution is centralized in `execute_chosen_command()`.

- Binary launchers use `std::process::Command`.
- Script launchers run through the configured interpreter or the default script shell.
- `--print-only` prints the resolved command or script wrapper instead of spawning it.

This separation keeps UI backends focused on selection rather than process management.

## Feature Flags and Build Modes

Cargo features deliberately shape the product:

- default build enables `wayland`, which pulls in `iced`, `ureq`, `regex`, and `meval`
- `--no-default-features` removes native UI support and the native-only addons

That smaller build path is a first-class design choice, not a degraded afterthought.

## Documentation and Change Surface

Behavior changes usually need updates in more than one place:

- `README.md` for high-level user-facing behavior
- `website/src/content/docs/` for detailed docs
- `examples/raffi.yaml` for canonical config examples
- `examples/raffi-schema.json` when config structs change
- `src/ui/wayland/tests.rs` or nearby unit tests when routing or ranking behavior changes

## Design Constraints

Current design constraints visible in the code:

- native mode is centered on Wayland/iced, while `fuzzel` mode provides the minimal external integration path
- launcher identity is largely derived from `description` or `binary`, which keeps the system simple but couples selection matching to user-visible labels
- some addon behavior is native-only by design and is not mirrored in `fuzzel` mode
- cache files are simple local artifacts rather than a formal database, which keeps the project lightweight
