# Herdr Agent Script Filter

This script lets Raffi browse Herdr agent sessions and focus the selected
terminal pane. Type the filter keyword to list sessions, then narrow the list
by agent type, status, project directory, or terminal title.

## Requirements

- Raffi native mode
- Python 3
- The Herdr CLI, with `herdr agent list`, `herdr agent get`, and
  `herdr agent read` available
- Optional: compositor command to bring the terminal window forward; the niri
  example also uses `jq`

## Install

```bash
mkdir -p ~/.config/raffi/scripts/herdr
cp scripts/herdr/herdr_filter.py ~/.config/raffi/scripts/herdr/herdr_filter.py
chmod +x ~/.config/raffi/scripts/herdr/herdr_filter.py
```

## Configure

Add this entry under `addons.script_filters` in `raffi.yaml`:

```yaml
addons:
  script_filters:
    - name: "Herdr Agents"
      keyword: "herdr"
      command: "~/.config/raffi/scripts/herdr/herdr_filter.py"
      icon: "utilities-terminal"
      action: 'herdr agent focus "$RAFFI_VALUE"'
```

To bring the terminal window forward as well, replace `action` with the
matching command below. Change `kitty` to your terminal's class or app ID.

Hyprland:

```yaml
action: >-
  hyprctl dispatch focuswindow class:kitty;
  herdr agent focus "$RAFFI_VALUE"
```

Sway:

```yaml
action: >-
  swaymsg '[app_id="kitty"] focus';
  herdr agent focus "$RAFFI_VALUE"
```

niri (requires `jq`):

```yaml
action: >-
  id=$(niri msg -j windows | jq -r '.[] | select(.app_id == "kitty") | .id' | head -n1);
  [ -z "$id" ] || niri msg action focus-window --id "$id";
  herdr agent focus "$RAFFI_VALUE"
```

Typing `herdr ` lists all agents. Further text filters by agent type, status,
project path, or terminal title. Press Enter to focus the selected session.

## Live preview

With the preview pane open (`Ctrl+P`), the selected agent shows its status,
directory, workspace, and the most recent terminal output with colours. The
preview refreshes every 2 seconds while the agent stays selected, so you can
watch it work before switching to it. The number of lines follows the height
of the preview pane.

Raffi asks for the preview by running the script again with `RAFFI_PREVIEW=1`
and the pane ID in `RAFFI_VALUE`. To try it by hand:

```bash
RAFFI_PREVIEW=1 RAFFI_VALUE=w1:p1 ~/.config/raffi/scripts/herdr/herdr_filter.py
```

Add `dynamic_preview: false` to the entry to turn this off.

## Tests

```bash
cd scripts/herdr && python3 -m unittest test_herdr_filter
```
