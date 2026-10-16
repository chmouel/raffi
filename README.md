<img src="assets/logo.png" alt="Raffi logo" width="120" align="right"/>

# Raffi Application Launcher

Raffi is an application launcher designed to sit on top of Fuzzel, or, if preferred, operate using its own built‑in interface. It allows commands and scripts to be defined in a YAML configuration file, with support for icons, arguments, conditional visibility, and script execution through configurable interpreters.

## Documentation

Primary documentation lives on the docs site:

- <https://chmouel.github.io/raffi/>

Useful entry points:

- Introduction: <https://chmouel.github.io/raffi/>
- Installation: <https://chmouel.github.io/raffi/installation/>
- Quick start: <https://chmouel.github.io/raffi/quickstart/>
- Configuration overview: <https://chmouel.github.io/raffi/configuration/overview/>
- Configuration reference: <https://chmouel.github.io/raffi/reference/yaml-schema/>
- CLI options: <https://chmouel.github.io/raffi/reference/cli-options/>
- UI modes: <https://chmouel.github.io/raffi/features/ui-modes/>
- Themes: <https://chmouel.github.io/raffi/features/themes/>
- Script filters: <https://chmouel.github.io/raffi/features/script-filters/>
- Text snippets: <https://chmouel.github.io/raffi/features/text-snippets/>
- Web search: <https://chmouel.github.io/raffi/features/web-search/>
- Calculator: <https://chmouel.github.io/raffi/features/calculator/>
- Currency converter: <https://chmouel.github.io/raffi/features/currency-converter/>
- File browser: <https://chmouel.github.io/raffi/features/file-browser/>
- Addon configuration: <https://chmouel.github.io/raffi/reference/addon-configuration/>
- Window manager integration (Sway): <https://chmouel.github.io/raffi/integration/sway/>
- Window manager integration (Hyprland): <https://chmouel.github.io/raffi/integration/hyprland/>

## Screenshot

![Raffi native launcher with right-hand preview pane](website/src/assets/screenshots/main.png)

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

This minimal build also leaves out config validation (`--check-config` and the startup warnings about unknown keys). Add it back with `--features config-validation` at the cost of a few extra MB.

## Usage

Running `raffi` launches configured entries through the selected interface. If your config does not define any valid launchers yet, Raffi falls back to auto-detected desktop applications from your system's `.desktop` files.

See the full CLI reference: <https://chmouel.github.io/raffi/reference/cli-options/>

## Window Manager Integration

### Sway

```config
set $menu raffi -p
set $super Mod4
bindsym $super+Space exec cmd=$($menu) && [ -n "$cmd" ] && swaymsg exec -- "$cmd"
```

### Hyprland

```conf
$super = SUPER
bind = $super, R, exec, (val=$(raffi -pI); [ -n "$val" ] && hyprctl dispatch exec "$val")
```

See also: [Sway integration](https://chmouel.github.io/raffi/integration/sway/) · [Hyprland integration](https://chmouel.github.io/raffi/integration/hyprland/) · [Fuzzel integration](https://chmouel.github.io/raffi/integration/fuzzel/)

## User Interfaces

Raffi supports two interface modes:

- **Fuzzel** — uses the external Fuzzel launcher, integrates naturally with Wayland environments.
- **Native** — built-in iced-based GUI with fuzzy search, keyboard navigation, and theme support.

The native interface includes built-in addons: calculator, currency converter, file browser (with drag-and-drop of files to other applications on Wayland), emoji/Nerd Fonts picker, script filters, web searches, and text snippets, along with an Alfred-style right-hand preview pane (**Ctrl+P** to toggle).
Press **Ctrl+/** to list every keyword, trigger and keyboard shortcut available as configured.

See: [UI modes](https://chmouel.github.io/raffi/features/ui-modes/) · [Themes](https://chmouel.github.io/raffi/features/themes/) · [Calculator](https://chmouel.github.io/raffi/features/calculator/) · [Currency converter](https://chmouel.github.io/raffi/features/currency-converter/) · [File browser](https://chmouel.github.io/raffi/features/file-browser/) · [Script filters](https://chmouel.github.io/raffi/features/script-filters/) · [Web search](https://chmouel.github.io/raffi/features/web-search/) · [Text snippets](https://chmouel.github.io/raffi/features/text-snippets/)

## Configuration

Configuration is stored in `$HOME/.config/raffi/raffi.yaml`. Basic example:

```yaml
version: 1

launchers:
  firefox:
    binary: firefox
    args: [--marionette]
    icon: firefox
    description: Firefox browser with marionette enabled
```

If you do not create a config yet, Raffi can still show installed desktop applications by scanning XDG application entries.

For full configuration reference see the docs:

- [Configuration overview](https://chmouel.github.io/raffi/configuration/overview/)
- [Entries](https://chmouel.github.io/raffi/configuration/entries/)
- [General settings](https://chmouel.github.io/raffi/configuration/general-settings/)
- [Scripts](https://chmouel.github.io/raffi/configuration/scripts/)
- [Conditions](https://chmouel.github.io/raffi/configuration/conditions/)
- [Icons](https://chmouel.github.io/raffi/configuration/icons/)
- [Addon configuration](https://chmouel.github.io/raffi/reference/addon-configuration/)
- [YAML schema reference](https://chmouel.github.io/raffi/reference/yaml-schema/)

## Development

Contributions are welcome. Issues, feature requests, and pull requests can be submitted via GitHub.

To enable pre‑commit hooks that run `cargo clippy` before pushing:

```sh
pip install pre-commit
pre-commit install
```

## Screenshots

All screenshots below were captured on an empty Hyprland workspace using a synthetic Raffi configuration and sample data.

### Interface Modes

<p><img width="800" alt="Fuzzel dmenu mode populated by Raffi launchers" src="website/src/assets/screenshots/fuzzel-mode.png" /></p>
<p><img width="800" alt="Native launcher with application preview" src="website/src/assets/screenshots/main.png" /></p>
<p><img width="800" alt="Calculator with expression preview and number bases" src="website/src/assets/screenshots/calculator.png" /></p>
<p><img width="800" alt="Currency converter with multi-currency preview" src="website/src/assets/screenshots/currency-converter.png" /></p>
<p><img width="800" alt="File browser with directory preview" src="website/src/assets/screenshots/file-browser.png" /></p>
<p><img width="800" alt="File browser source code preview" src="website/src/assets/screenshots/file-browser-code-preview.png" /></p>
<p><img width="800" alt="File browser image preview" src="website/src/assets/screenshots/file-browser-image-preview.png" /></p>
<p><img width="800" alt="File browser image preview scrolled to file details" src="website/src/assets/screenshots/file-browser-image-scrolled.png" /></p>
<p><img width="800" alt="File browser showing hidden files" src="website/src/assets/screenshots/file-browser-hidden.png" /></p>
<p><img width="800" alt="Text snippets with expanded content preview" src="website/src/assets/screenshots/text-snippets.png" /></p>
<p><img width="800" alt="Text snippet with expanded dynamic placeholders and raw template" src="website/src/assets/screenshots/text-snippets-placeholders.png" /></p>
<p><img width="800" alt="Emoji picker with Unicode and glyph preview" src="website/src/assets/screenshots/emoji-picker.png" /></p>
<p><img width="800" alt="Web search with resolved URL preview" src="website/src/assets/screenshots/web-search.png" /></p>
<p><img width="800" alt="Configured fallback search results and preview" src="website/src/assets/screenshots/fallbacks.png" /></p>

### Search, Help, and Scripts

<p><img width="800" alt="Keyword suggestion while typing" src="website/src/assets/screenshots/keyword-suggestion.png" /></p>
<p><img width="800" alt="Keyword and shortcut help list" src="website/src/assets/screenshots/keyword-help.png" /></p>
<p><img width="800" alt="Keyboard shortcut help filtered to Ctrl shortcuts" src="website/src/assets/screenshots/keyword-help-shortcuts.png" /></p>
<p><img width="800" alt="Inline launcher script shown in the preview pane" src="website/src/assets/screenshots/launcher-script-preview.png" /></p>
<p><img width="800" alt="Timezone script filter with ANSI-formatted preview" src="website/src/assets/screenshots/script-filter-tz.png" /></p>
<p><img width="800" alt="Pull request script filter with CI status and details" src="website/src/assets/screenshots/script-filter-pr.png" /></p>

### Themes

<p><img width="800" alt="Built-in light theme" src="website/src/assets/screenshots/light-theme.png" /></p>
<p><img width="800" alt="Catppuccin Mocha theme" src="website/src/assets/screenshots/theme-catppuccin.png" /></p>
<p><img width="800" alt="Nord theme" src="website/src/assets/screenshots/theme-nord.png" /></p>
<p><img width="800" alt="Gruvbox Dark theme" src="website/src/assets/screenshots/theme-gruvbox.png" /></p>
<p><img width="800" alt="Solarized Light theme" src="website/src/assets/screenshots/theme-solarized-light.png" /></p>
<p><img width="800" alt="Tokyo Night theme" src="website/src/assets/screenshots/theme-tokyonight.png" /></p>

## Licence

This project is released under the MIT Licence.

## Author

[![Sponsor](https://img.shields.io/badge/Sponsor-❤️-ff69b4?style=for-the-badge&logo=github)](https://github.com/sponsors/chmouel)

### Chmouel Boudjnah

- Fediverse - [@chmouel@chmouel.com](https://fosstodon.org/@chmouel)
- Twitter - [@chmouel](https://twitter.com/chmouel)
- Blog  - [https://blog.chmouel.com](https://blog.chmouel.com)
