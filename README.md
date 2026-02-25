# Raffi Application Launcher

Raffi is an application launcher designed to sit on top of Fuzzel, or, if preferred, operate using its own built‑in interface. It allows commands and scripts to be defined in a YAML configuration file, with support for icons, arguments, conditional visibility, and script execution through configurable interpreters.

![image](https://github.com/chmouel/raffi/assets/98980/04d6af0f-2a80-47d5-a2ec-95443a629305)

*See more screenshots [below](#screenshots)*

## Installation

Prebuilt binaries are available from the GitHub releases page. Download the archive or package suitable for your platform. If you intend to use the default interface, Fuzzel must also be installed.

On Arch Linux, Raffi can be installed from the AUR using a helper such as:

```sh
yay -S raffi-bin
```

On NixOS or using Nix (unstable channel):

```sh
nix-shell -p raffi
```

With LinuxBrew or Homebrew:

```sh
brew tap chmouel/raffi https://github.com/chmouel/raffi
brew install raffi
```

From crates.io:

```sh
cargo install raffi
```

To build from source:

```sh
git clone https://github.com/chmouel/raffi.git
cd raffi
cargo build --release
```

If you only require Fuzzel integration and want a significantly smaller binary, build without the native UI:

```sh
cargo build --release --no-default-features
```

This reduces the binary from roughly 15 MB to around 1.1 MB by removing the iced GUI dependency.

## Usage

Running `raffi` launches configured entries through the selected interface. The chosen item is executed according to the configuration.

Common options include:

`-p` or `--print-only` prints the command rather than executing it.
`-c` or `--configfile <FILE>` selects a custom configuration file.
`-r` or `--refresh-cache` refreshes cached icon paths.
`-I` or `--disable-icons` disables icons for slightly faster startup.
`-u` or `--ui-type <TYPE>` selects `fuzzel` or `native` (default is `fuzzel`).
`-i` or `--initial-query <QUERY>` pre-fills the search field on launch (native mode only).
`-t` or `--theme <THEME>` selects `dark` or `light` theme (default is `dark`, native mode only).
`--default-script-shell <SHELL>` sets the default interpreter for scripts.

## Window Manager Integration

### Sway

```config
set $menu raffi -p
set $super Mod4
bindsym $super+Space exec $menu | xargs swaymsg exec --
```

### Hyprland

```conf
$super = SUPER
bind = $super, R, exec, (val=$(raffi -pI); echo $val | grep -q . && hyprctl dispatch exec "$val")
```

## User Interfaces

Raffi supports two interface modes.

Fuzzel mode uses the external Fuzzel launcher and integrates naturally with Wayland environments.

Native mode uses an internal iced‑based graphical interface with fuzzy search, keyboard navigation, and theme support (dark by default, light available via `-t light` or config). It is suitable if you prefer a self‑contained graphical window.

### Native Interface Extras

#### Calculator

The native interface includes a built‑in calculator which evaluates expressions as you type. Standard mathematical operators are supported, along with functions such as `sqrt`, `sin`, `cos`, `tan`, `log`, `ln`, `exp`, `abs`, `floor`, and `ceil`. Results can be copied to the clipboard using Enter, provided `wl-copy` is available.

#### Currency Converter

The native interface also includes a currency converter.
Enter an amount prefixed with the configured trigger (default `$`) followed by a target currency. Exchange rates are fetched from the Frankfurter API and cached for one hour.

Example inputs:

```
$10 to eur
$50 gbp to usd
$100eur to jpy
€10 to usd    (with trigger set to €)
```

#### Dynamic Script Filters

The native interface supports script filters, which allow external commands to provide
dynamic results in the launcher. This feature uses a subset of the
[Alfred Script Filter JSON format](https://www.alfredapp.com/help/workflows/inputs/script-filter/json/).

When the configured keyword is typed, the script is executed with the remaining input
passed as the final argument. The script must print JSON to stdout. On selection, the
item's `arg` value (or `title` if `arg` is absent) is copied to the clipboard
(auto-detecting `wl-copy`, `xclip`, or `xsel`). Alternatively, a custom `action` can be configured to run any command with
the selected value substituted via `{value}` placeholder. A `secondary_action` can
also be defined, which is triggered with Alt+Enter instead of Enter.

 The `title` and `subtitle` fields support ANSI color codes in JSON output,
 which the launcher renders as colored text. For example, the [pr.py script
 from alfred-pr-workflow](https://gitlab.com/chmouel/alfred-pr-workflow) uses
 colors to display pull request states.

[See below for how to configure this](#script-filters-configuration)

#### Web Searches

The native interface supports quick web searches via URL templates. Type a configured keyword followed by your query, and the launcher will open your default browser with the search results.

Example: typing `g rust traits` opens Google search for "rust traits" in your browser.

Common search engines are pre-configured in the example config (Google, DuckDuckGo, GitHub, Wikipedia, etc.), and you can add any search engine by providing a URL template with a `{query}` placeholder.

[See below for how to configure this](#web-search-configuration)

#### Text Snippets

The native interface supports text snippets, which let you define reusable text values that can be searched and copied to the clipboard. Snippets can come from three sources: inline in the config, an external YAML file, or a command's output (using the same Alfred JSON format as script filters).

Type a configured keyword to display the snippets from that source, then continue typing to fuzzy-filter by name. By default, selecting a snippet copies its value to the clipboard (Enter) using the first available tool (`wl-copy`, `xclip`, or `xsel`), or types it into the focused app via `wtype`/`ydotool` (Ctrl+Enter). These actions can be customised with the `action` and `secondary_action` fields.

[See below for how to configure this](#text-snippets-configuration)

#### File Browser

The native interface includes a built-in file browser. Typing `/` browses the root filesystem and `~` browses the home directory. Selecting a directory navigates into it, while selecting a file opens it with `xdg-open`. Alt+Enter copies the file path to the clipboard instead. Tab autocompletes the selected entry into the search bar. Ctrl+H toggles hidden file visibility. Text after the last `/` fuzzy-filters the current directory listing (e.g., `/home/us` filters `/home/` by "us"). Directories are listed first in accent colour.

The file browser is enabled by default and can be disabled or configured under `addons.file_browser` ([see below](#addon-configuration)).

## Configuration

### Fuzzel Configuration

Fuzzel appearance can be configured through `~/.config/fuzzel/fuzzel.ini`. Refer to the [Fuzzel manual](https://man.archlinux.org/man/fuzzel.ini.5.en) for full details.

Example:

```ini
dpi-aware=yes
font=RobotoMonoNerdFont-Thin:size=16
terminal=kitty
width=50
layer=overlay
exit-on-keyboard-focus-loss=no
inner-pad=15
fields=filename,name

[colors]
background=282a36ff
text=f8f8f2ff
match=8be9fdff
selection-match=8be9fdff
selection=44475add
selection-text=f8f8f2ff
border=bd93f9ff
```

### Raffi Configuration

Configuration is stored in:

```
$HOME/.config/raffi/raffi.yaml
```

Basic example:

```yaml
firefox:
  binary: firefox
  args: [--marionette]
  icon: firefox
  description: Firefox browser with marionette enabled
```

The `binary` field defines the executable. If the binary is not present in PATH, the entry is ignored.
The `description` field defines the label shown in the launcher.
The `args` field defines optional command‑line arguments.
The `icon` field defines an icon name or absolute path. Icons are cached and can be refreshed with `-r`.
The `script` field defines inline script content.
The `disabled` field hides the entry when set to true.

### General Settings

Persistent defaults can be set under a `general` key. These are equivalent to command‑line flags and are overridden by them.

```yaml
general:
  ui_type: native
  theme: light
  no_icons: true
  default_script_shell: /bin/zsh
  font_size: 20
  font_family: "Inter"
  window_width: 900
  window_height: 500
  theme_colors:
    accent: "#cba6f7"
```

The `ui_type` field selects the interface (`fuzzel` or `native`).
The `theme` field selects the colour theme for the native interface. `dark` (default) uses a Dracula‑inspired palette; `light` uses Rose Pine Dawn.
The `no_icons` field disables icon loading when set to true.
The `default_script_shell` field sets the default interpreter for inline scripts.
The `font_size` field sets the base font size in pixels (default: 20). Other UI sizes and paddings scale proportionally: input text is 1.2×, subtitles are 0.7×, and hint text is 0.6× the base size.
The `font_family` field sets the font family name (e.g. `"Inter"`, `"Fira Sans"`). When omitted, the system default sans-serif font is used.
The `window_width` and `window_height` fields set the launcher window dimensions in pixels (defaults: 800×600).
The `padding` field overrides the outer window padding in pixels (default: 20). When not set, it scales automatically with `font_size`.

Individual theme colours can be customised under `theme_colors`. Each field
accepts a hex colour string (`#RGB`, `#RRGGBB`, or `#RRGGBBAA`). Only the
colours you specify are overridden; the rest come from the base theme.

Available colour keys: `bg_base`, `bg_input`, `accent`, `accent_hover`,
`text_main`, `text_muted`, `selection_bg`, `border`.

Example (Catppuccin Mocha on top of the dark base theme):

```yaml
general:
  theme: dark
  theme_colors:
    bg_base: "#1e1e2e"
    bg_input: "#313244"
    accent: "#cba6f7"
    accent_hover: "#89b4fa"
    text_main: "#cdd6f4"
    text_muted: "#6c7086"
    selection_bg: "#45475a"
    border: "#585b70"
```

### Scripts

Entries can run inline scripts instead of binaries. Scripts use `bash` by default, or another interpreter if configured via `--default-script-shell` or by explicitly setting `binary`.

Example:

```yaml
hello_script:
  script: |
    echo "hello world and show me your env"
    env
  description: "Hello Script"
  icon: "script"
```

Python example:

```yaml
hello_script:
  binary: python3
  script: |
    import os
    print("hello world and show me your env")
    print(os.environ)
  description: "Hello Python script"
  icon: "script"
```

### Conditional Display

Entries can be shown or hidden based on simple conditions. Only one condition is supported per entry.

Conditions include checking whether a binary exists, whether an environment variable is set, not set, or equal to a specific value.

Example:

```yaml
ifenveq: [DESKTOP_SESSION, GNOME]
ifenvset: WAYLAND_DISPLAY
ifexist: firefox
```

### Path Expansion

Config values for most fields except the script field support path expansion:

- `~/` is expanded to the user's home directory
- `${VAR}` is replaced with the environment variable value (unset variables
expand to empty string)

Example:

```yaml
myapp:
  binary: ${HOME}/bin/myapp
  args: ["${XDG_DATA_HOME}/files", "~/Documents"]
  icon: ~/icons/myapp.png
  ifexist: ~/bin/myapp
```

### Addon Configuration

The native interface includes optional addons for calculations, currency conversion, file browsing, script filters, text snippets, and web searches.

```yaml
addons:
  currency:
    enabled: true
    trigger: "€"
    default_currency: EUR
    currencies: ["EUR", "USD", "GBP"]
  calculator:
    enabled: true
  file_browser:
    enabled: true
    show_hidden: false
```

The `enabled` field controls whether the addon is active.
The `show_hidden` field for the file browser sets the initial hidden-file visibility (default `false`; toggled at runtime with Ctrl+H).
The `trigger` field sets the character that activates currency conversion. Defaults to `$` if omitted. Can be set to `€`, `£`, or any other symbol.
The `default_currency` field sets the source currency used when none is specified (e.g., `€10 to GBP` converts from EUR). Defaults to USD if omitted.
The `currencies` field for the currency addon defines which currencies are available for conversion.
You don't need to add a prefix for the calculator; simply typing a valid expression will show the result.

All three addons are enabled by default.

#### Script Filters Configuration

Script filters are configured under `addons.script_filters`. Here is an example using the [batz](https://github.com/chmouel/batzconverter) time converter (shown in the screenshot above):

```yaml
addons:
  script_filters:
    - name: "Timezones"
      keyword: "tz"
      command: "batz"
      args: ["-j"]
      icon: "clock"
```

This will parse the output of `batz -j` and display it in the launcher when the
user types `tz` followed by a query. The script must output JSON in the format
described below.

An example with both a primary and secondary action:

```yaml
addons:
  script_filters:
    - name: "Bookmarks"
      keyword: "bm"
      command: "my-bookmark-script"
      args: ["-j"]
      action: "echo -n {value}|wl-copy"
      secondary_action: "xdg-open {value}"
```

Here, pressing Enter copies the selected bookmark URL to the clipboard, while
Alt+Enter opens it in the default browser.

Here is the meaning of each field in the script filter configuration:

| Field     | Required | Description                                                          |
|-----------|----------|----------------------------------------------------------------------|
| `name`    | yes      | Display name shown during loading                                    |
| `keyword` | yes      | Text that activates the script filter                                |
| `command` | yes      | Executable to run                                                    |
| `args`    | no       | Arguments passed before the query                                    |
| `icon`    | no       | Fallback icon name for results without their own                     |
| `action`  | no       | Action on Enter. Use `"copy"` to copy to clipboard, `"insert"` to type into the focused app via wtype/ydotool, or a command template where `{value}` is replaced with the selected value (executed via `sh -c`). Defaults to `"copy"`. |
| `secondary_action` | no | Action on Ctrl+Enter. Accepts the same values as `action`. If omitted, Ctrl+Enter behaves the same as Enter. |

The script must output JSON matching this structure (a subset of Alfred's format):

```json
{
  "items": [
    {
      "title": "New York",
      "subtitle": "EST (UTC-5) — 14:30",
      "arg": "America/New_York",
      "icon": { "path": "/usr/share/icons/clock.png" }
    }
  ]
}
```

| Field          | Required | Description                                     |
|----------------|----------|-------------------------------------------------|
| `title`        | yes      | Main text displayed for the item                |
| `subtitle`     | no       | Secondary text shown below the title            |
| `arg`          | no       | Value copied to clipboard (falls back to title) |
| `icon.path`    | no       | Absolute path to a PNG or SVG icon              |

### Web Search Configuration

Web searches are configured under `addons.web_searches`. Each entry defines a keyword and URL template:

```yaml
addons:
  web_searches:
    - name: "Google"
      keyword: "g"
      url: "https://google.com/search?q={query}"
      icon: "google"
```

When you type the keyword followed by a space and query (e.g., `g rust async`), the launcher displays a search row. Pressing Enter opens your browser with the URL template, replacing `{query}` with your search terms (properly percent-encoded).

Field descriptions:

| Field     | Required | Description                                                          |
|-----------|----------|----------------------------------------------------------------------|
| `name`    | yes      | Display name shown in the search row (e.g., "Search Google for...")  |
| `keyword` | yes      | Text that activates the web search (e.g., "g", "ddg")                |
| `url`     | yes      | URL template with `{query}` placeholder for the search terms         |
| `icon`    | no       | Icon name from your icon cache to display next to the search row     |

The URL template's `{query}` placeholder is replaced with your search terms, automatically percent-encoded for safe URL use. For example, `g hello world` becomes `https://google.com/search?q=hello%20world`.

### Text Snippets Configuration

Text snippets are configured under `addons.text_snippets`. Each entry defines a keyword and a source for the snippets (inline, file, or command).

Inline snippets:

```yaml
addons:
  text_snippets:
    - name: "Emails"
      keyword: "em"
      icon: "mail"
      snippets:
        - name: "Personal Email"
          value: "user@example.com"
        - name: "Work Email"
          value: "user@company.com"
```

File source (YAML file containing a list of `name`/`value` pairs):

```yaml
addons:
  text_snippets:
    - name: "Templates"
      keyword: "tpl"
      icon: "document"
      file: "~/.config/raffi/snippets.yaml"
```

The snippet file uses the same format:

```yaml
- name: "Greeting"
  value: "Hello, world!"
- name: "Signature"
  value: "Best regards, User"
```

Command source (outputs Alfred Script Filter JSON):

```yaml
addons:
  text_snippets:
    - name: "Dynamic"
      keyword: "dyn"
      icon: "terminal"
      command: "my-snippet-gen"
      args: ["-j"]
```

The command must output JSON in the same format used by [script filters](#script-filters-configuration). The `title` field maps to the snippet name and `arg` to the snippet value.

Directory source (a directory of `.snippet` files):

```yaml
addons:
  text_snippets:
    - name: "Snippets"
      keyword: "sn"
      icon: "snippets"
      directory: "~/.local/share/desktop-config/snippets"
```

Each `.snippet` file has the format:

```
Description (first line)
---
Actual snippet content (everything after the separator)
```

The first line becomes the snippet name, everything after `---` becomes the value. Files are sorted alphabetically by name and cached per session.

Field descriptions:

| Field       | Required | Description                                                            |
|-------------|----------|------------------------------------------------------------------------|
| `name`      | yes      | Display name shown during loading (command source)                     |
| `keyword`   | yes      | Text that activates the snippet source                                 |
| `icon`      | no       | Icon name from your icon cache to display next to each snippet         |
| `snippets`  | no       | Inline list of snippets (each with `name` and `value`)                 |
| `file`      | no       | Path to a YAML file containing snippets (cached per session)           |
| `command`   | no       | Executable that outputs Alfred Script Filter JSON                      |
| `directory` | no       | Path to a directory of `.snippet` files (cached per session)           |
| `args`      | no       | Arguments passed to the command                                        |
| `action`    | no       | Action on Enter. Use `"copy"` to copy to clipboard (auto-detects wl-copy/xclip/xsel), `"insert"` to type into the focused app via wtype/ydotool, or a command template where `{value}` is replaced with the selected value (executed via `sh -c`). Defaults to `"copy"`. |
| `secondary_action` | no | Action on Ctrl+Enter. Accepts the same values as `action`. Defaults to `"insert"`. |

Exactly one of `snippets`, `file`, `command`, or `directory` should be specified per entry.

An example with custom actions (type on Enter, copy on Ctrl+Enter):

```yaml
addons:
  text_snippets:
    - name: "Emails"
      keyword: "em"
      icon: "mail"
      action: "insert"
      secondary_action: "copy"
      snippets:
        - name: "Personal Email"
          value: "user@example.com"
        - name: "Work Email"
          value: "user@company.com"
```

## Development

Contributions are welcome. Issues, feature requests, and pull requests can be submitted via GitHub.

To enable pre‑commit hooks that run `cargo clippy` before pushing:

```sh
pip install pre-commit
pre-commit install
```

## Screenshots

### File Browser (`/`)

<img width="321" height="224" alt="optimized-file-browse" src="https://github.com/user-attachments/assets/bbfbe937-c590-43c9-8853-9aaf786a3dc6" />

### Currency Converter (`$`)

<img width="522" height="150" alt="image" src="https://github.com/user-attachments/assets/aaf35e3f-1cef-4604-b87a-ecfa626300c1" />

## Calculator

<img width="522" height="150" alt="image" src="https://github.com/user-attachments/assets/eb7069c9-21f7-413d-b455-c2db186591d5" />

### Script Filter with github PR browser

<img width="441" height="559" alt="optimized-pull-requests-dashboard" src="https://github.com/user-attachments/assets/48da3b90-b8dd-4f4d-8465-3ae27fe267c3" />

### Script Filter with timezone converter

<img width="522" height="400" alt="image" src="https://github.com/user-attachments/assets/f65acf34-b499-477d-9952-48590723d5bb" />

### Light Theme

<img width="403" height="473" alt="suspend-to-sleep-then-hibernate" src="https://github.com/user-attachments/assets/9c6549dc-be51-422d-9b82-5dbbb89779b6" />

## Licence

This project is released under the MIT Licence.

## Author

Chmouel Boudjnah

- GitHub: [https://github.com/chmouel](https://github.com/chmouel)
- Fediverse: [https://fosstodon.org/@chmouel](https://fosstodon.org/@chmouel)
- Twitter: [https://twitter.com/chmouel](https://twitter.com/chmouel)
- Blog: [https://blog.chmouel.com](https://blog.chmouel.com)
