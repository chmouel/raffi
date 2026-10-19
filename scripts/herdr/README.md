# Herdr Script Filter

This script lets Raffi browse a Herdr session and focus what you pick. By
default it lists workspaces and tabs as an indented tree, showing pane
children only when a tab has multiple panes. The query matches workspaces,
tabs, panes, agent types, statuses, project directories, and terminal
titles, and keeps the ancestors of each match so results stay in tree
order.

Pass `--agents` (or export `HERDR_SCOPE=agents`) to list only panes hosting
a recognized agent, like earlier versions did.

## Requirements

- Raffi native mode
- Python 3
- The Herdr CLI, with `herdr workspace list`, `herdr tab list`,
  `herdr pane list`, `herdr pane get`, `herdr pane read`, and, for
  `--agents`, `herdr agent list` available
- Optional: compositor command to bring the terminal window forward; the
  niri example also uses `jq`

## Install

```bash
mkdir -p ~/.config/raffi/scripts/herdr
cp scripts/herdr/herdr_filter.py ~/.config/raffi/scripts/herdr/herdr_filter.py
cp -R scripts/herdr/icons ~/.config/raffi/scripts/herdr/
chmod +x ~/.config/raffi/scripts/herdr/herdr_filter.py
```

Copy the `icons` directory when upgrading too. It contains the bundled SVGs
and their license notices; the script resolves it beside its own file.

## Configure

Add this entry under `addons.script_filters` in `raffi.yaml`:

```yaml
addons:
  script_filters:
    - name: "Herdr"
      keyword: "herdr"
      command: "~/.config/raffi/scripts/herdr/herdr_filter.py"
      icon: "utilities-terminal"
      action: >-
        case "$HERDR_KIND" in
          workspace) herdr workspace focus "$HERDR_FOCUS" ;;
          tab) herdr tab focus "$HERDR_FOCUS" ;;
          agent) herdr agent focus "$HERDR_FOCUS" ;;
          *) herdr tab focus "$HERDR_FOCUS" ;;
        esac
```

Every item exports `HERDR_KIND` (`workspace`, `tab`, `agent`, or `pane`)
and `HERDR_FOCUS`, the Herdr ID the action should focus. `HERDR_FOCUS`
matches the item's `arg` except for plain shell panes, whose tab is
focused instead, because Herdr can only focus neighboring panes directly.

Since there is no single `herdr` command that focuses any ID, the action
dispatches on the kind. If the inline `case` above is too verbose, put a
tiny wrapper in your `PATH` and keep the action short:

```bash
#!/bin/sh
# herdr-focus: focus a Herdr workspace, tab, or agent pane from Raffi
case "$1" in
  workspace) herdr workspace focus "$2" ;;
  tab) herdr tab focus "$2" ;;
  agent) herdr agent focus "$2" ;;
  *) herdr tab focus "$2" ;;
esac
```

```yaml
action: 'herdr-focus "$HERDR_KIND" "$HERDR_FOCUS"'
```

To list only agents, like the original version of this filter, add
`args: ["--agents"]` (an `env: {HERDR_SCOPE: agents}` block works too):

```yaml
addons:
  script_filters:
    - name: "Herdr Agents"
      keyword: "hagents"
      command: "~/.config/raffi/scripts/herdr/herdr_filter.py"
      args: ["--agents"]
      icon: "utilities-terminal"
      action: 'herdr agent focus "$HERDR_FOCUS"'
```

To bring the terminal window forward as well, prepend the matching
compositor command to the action above. Change `kitty` to your terminal's
class or app ID.

Hyprland:

```yaml
action: >-
  hyprctl dispatch focuswindow class:kitty;
  case "$HERDR_KIND" in
    workspace) herdr workspace focus "$HERDR_FOCUS" ;;
    tab) herdr tab focus "$HERDR_FOCUS" ;;
    agent) herdr agent focus "$HERDR_FOCUS" ;;
    *) herdr tab focus "$HERDR_FOCUS" ;;
  esac
```

Sway:

```yaml
action: >-
  swaymsg '[app_id="kitty"] focus';
  case "$HERDR_KIND" in
    workspace) herdr workspace focus "$HERDR_FOCUS" ;;
    tab) herdr tab focus "$HERDR_FOCUS" ;;
    agent) herdr agent focus "$HERDR_FOCUS" ;;
    *) herdr tab focus "$HERDR_FOCUS" ;;
  esac
```

niri (requires `jq`):

```yaml
action: >-
  id=$(niri msg -j windows | jq -r '.[] | select(.app_id == "kitty") | .id' | head -n1);
  [ -z "$id" ] || niri msg action focus-window --id "$id";
  case "$HERDR_KIND" in
    workspace) herdr workspace focus "$HERDR_FOCUS" ;;
    tab) herdr tab focus "$HERDR_FOCUS" ;;
    agent) herdr agent focus "$HERDR_FOCUS" ;;
    *) herdr tab focus "$HERDR_FOCUS" ;;
  esac
```

## The tree

Typing `herdr` plus a space lists the session as an indented tree, using
the script filter `indent` field:

```text
raffi [working] · focused · 3 tabs · 3 panes
  Implement Raffi Builtin Feature [working] · copilot
  Workspace Agent Script Filters [working] · pi
  ~/git/perso/raffi · shell
pac [working] · 1 tab · 2 panes
  Investigate failures [working] · copilot, shell
    copilot [working] - pac
    shell - pac
```

Single-pane tabs, including plain shells, appear once as the tab/session
row. Their pane details remain searchable, and their preview still shows
the terminal output. Tabs with multiple panes keep their pane children,
even when a query matches only one pane.

Statuses are coloured: working green, blocked red, done yellow, idle cyan,
unknown grey. Further text filters the tree; matching a workspace or tab
keeps everything below it, matching a pane keeps its ancestors so you
still see where it lives. Press Enter to focus the selected workspace,
tab, or agent pane.

## Agent status tree

Keep the workspace tree as the default, or group agents across workspaces
by status:

```yaml
env:
  HERDR_VIEW: status
```

You can also pass `args: ["--status"]`. `--tree` selects the workspace tree
and `--agents` selects the flat agent list; the last mode flag overrides
the environment.

```text
Working · 2 agents
  Implement tree icons · raffi · copilot
  Investigate failures · pac · pi
Waiting · 1 agent
  Review changes · raffi · claude
Idle · 1 agent
  Documentation · liseur · copilot
```

Groups appear in Working, Waiting, Idle, Done, Unknown order, with empty
groups hidden. Waiting represents Herdr's `blocked` state. Unexpected states
appear under Unknown with their original status in the subtitle.
Queries match workspace/session names, agent details, and status names;
`waiting` and `blocked` find the same agents.

Each agent pane appears once, with its session name and workspace context.
Split tabs can contribute agents to different groups; plain shells are
omitted. Enter focuses the agent pane, and its preview shows terminal output.
Group headings show counts and a summary of the matching agents. They are
non-actionable: Enter, Ctrl+Enter, and clicking leave Raffi open.
Counts describe the current results; run the query again to refresh membership.

## Icon sets

The default `color` set uses padded, colored tiles with agent logos.
Workspace stacks, session windows, and shell prompts have distinct symbols.
Single-pane agent sessions use their agent's logo; unknown agents use a
generic mark. Preview headers use the same icons as the list.

Choose a set in the filter's `env`:

```yaml
env:
  HERDR_ICON_SET: color
```

Use `mono-dark` for pale icons on dark themes or `mono-light` for dark icons
on light themes. Selection is explicit, independent of Raffi's theme.
All sets are bundled; no icon font or download is needed. Raffi's
`no_icons` setting still hides them.

The artwork comes from herdr-gpui and Lobe Icons. Sources, licenses, and
modifications are recorded in `icons/NOTICE`. To regenerate the presets:

```bash
uv run --no-project python3 scripts/herdr/generate_icons.py
uv run --no-project python3 scripts/herdr/generate_icons.py --check
```

## Live preview

With the preview pane open (`Ctrl+P`), the selected row shows a live
preview that refreshes every 2 seconds while it stays selected:

- an agent pane: its status, directory, workspace, tab, and the most
  recent terminal output with colours
- a plain pane: the same, without the agent rows
- a tab: its status, pane list, and, for single-pane tabs, the terminal
  output of that pane
- a workspace: its status, tab list with pane counts, and active tab

The number of terminal lines follows the height of the preview pane.

Raffi asks for the preview by running the script again with
`RAFFI_PREVIEW=1` and the Herdr ID in `RAFFI_VALUE`. The item also exports
`HERDR_KIND`, which the script uses to pick the preview; without it, the
ID shape decides (`w1` workspace, `w1:t1` tab, `w1:p1` pane). To try it by
hand:

```bash
RAFFI_PREVIEW=1 RAFFI_VALUE=w1:p1 ~/.config/raffi/scripts/herdr/herdr_filter.py
```

Add `dynamic_preview: false` to the entry to turn this off.

## Tests

```bash
cd scripts/herdr && python3 -m unittest test_herdr_filter
```
