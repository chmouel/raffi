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

## Native Search Ranking

`ranking.rs` normalizes and scores prebuilt search documents for launchers and
command-list results. The title and executable basename are primary fields;
keywords, subtitles and metadata have lower weights. Whole-query matches,
primary word prefixes, accepted compact fuzzy matches, mixed-field matches
and secondary-only matches form ordered relevance categories. Recognized URL
domains precede them. The `sort_mode` usage score breaks ties within a source,
not between relevance categories or configured sources.

`main_view.rs` materializes an ordered list of launcher and addon row references
for standard searches. Typed local matches interleave across sources and each
addon is capped after its own matching; provider-owned rows follow the ranked
ones. Empty queries and Ctrl+Tab focused lists retain their source order. The
same ordered list drives rendering, selection, actions, previews and row counts.
After an async load, selection follows the first result until the user moves
or clicks it; it then follows the row's source-scoped identity if still visible.

## Addon Design

Native-only addons are configured under `addons` and activated from the search box.

- Calculator:
  - inline evaluation using `evalexpr`
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
  - rank by snippet keyword, then name, then content
  - expand placeholders (`placeholders.rs`) on selection, off the UI thread, with
    previews computed without running shell commands
- Web searches:
  - fill `{query}` into configured URL templates and open them with `xdg-open`
- Emoji picker:
  - loads cached emoji data or downloads it into the cache directory
  - supports primary and secondary actions such as copy or insert
- Bookmarks (`bookmarks.rs`, `bookmarks_mode.rs`):
  - read Chromium `Bookmarks` JSON and a private copy of Firefox
    `places.sqlite` (with its WAL) once per entry, off the UI thread
  - the folder path is part of the query; rows are rebuilt from the cached
    tree only when the path or the source view changes

The design pattern is consistent across addons:

- parse a query into addon-specific state
- optionally spawn async work
- render temporary results inside the single launcher window
- perform the action on submit
- persist search history before exit when appropriate
- switch between the addon's views with Ctrl+Tab through `handle_cycle_view`
  (see `AGENTS.md`), never with a key of its own

## Persistence and Cache Files

Raffi stores lightweight state under the XDG cache directory, defaulting to `~/.cache/raffi/`.

Current cache artifacts:

- `icon.cache`: serialized icon-name to icon-path map
- `mru.cache`: launch frequency and recency data
- `history.cache`: prior search queries for native history navigation
- `last-query.cache`: raw text of the last query typed, read by `--last-query`
- `emoji/`: downloaded emoji datasets
- `slack/`: Slack directory, view and preview caches
- `script-filters/lists/` and `script-filters/previews/`: opt-in
  stale-while-revalidate snapshots of Script Filter output
- `browser-history/init.json`: parsed browser history plus SQLite file
  fingerprints

These snapshots are versioned JSON or binary files written atomically with
private permissions (`0700` directories, `0600` files). `raffi --refresh-cache`
removes them together with the other caches.

An optional foreground service, `raffi --cache-daemon`, speaks
newline-delimited JSON over `$XDG_RUNTIME_DIR/raffi/cache.sock`. It warms
opted-in Script Filters, Slack views and browser history while Raffi is
closed. The UI never blocks on it: it reads the same snapshots directly and
falls back to running sources itself when the socket is absent or slow. At
startup the UI sends a short `status` probe; when it answers with a recent
enough `protocol`, F5 sends `refresh_source` (`slack:<view>` for one Slack
view) or `get_script_filter` with `wait: true` (Ctrl+Shift+F5 sends
`refresh_all`), and the UI reloads the
view from the snapshot once the service replies. See
`src/ui/wayland/cache.rs` and `misc/systemd/raffi-cache.service`.

On startup, `run()` writes `raffi-schema.json` and `launchers-schema.json` next
to the user config when either file is missing or outdated.
`--generate-schemas` explicitly writes both files and exits. These schemas
improve editor support for YAML authoring.

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
- `--print-only` prints the resolved command as a shell-ready, quoted command line instead of spawning it.

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
- `examples/raffi-schema.json` and `examples/launchers-schema.json` when config
  structs change
- `src/ui/wayland/tests.rs` or nearby unit tests when routing or ranking behavior changes

## Design Constraints

Current design constraints visible in the code:

- native mode is centered on Wayland/iced, while `fuzzel` mode provides the minimal external integration path
- launcher identity is largely derived from `description` or `binary`, which keeps the system simple but couples selection matching to user-visible labels
- some addon behavior is native-only by design and is not mirrored in `fuzzel` mode
- cache files are simple local artifacts rather than a formal database, which keeps the project lightweight
