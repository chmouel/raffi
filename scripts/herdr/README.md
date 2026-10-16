# Herdr Agent Script Filter

This script lets Raffi browse Herdr agent sessions and focus the selected
terminal pane. Type the filter keyword to list sessions, then narrow the list
by agent type, status, project directory, or terminal title.

## Requirements

- Raffi native mode
- Python 3
- The Herdr CLI, with `herdr agent list` available
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
