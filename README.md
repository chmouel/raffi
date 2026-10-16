<img src="assets/logo.png" alt="Raffi logo" width="120" align="right"/>

# Raffi Application Launcher

Raffi is an application launcher for Wayland. You define commands and scripts in a YAML file, with icons, arguments, conditions for when an entry shows up, and the interpreter each script runs with. Raffi shows them in its own native window, or in Fuzzel if you prefer.

## Documentation

The full documentation is at <https://chmouel.github.io/raffi/>. Good places to start:

- [Installation](https://chmouel.github.io/raffi/installation/)
- [Quick start](https://chmouel.github.io/raffi/quickstart/)
- [Configuration overview](https://chmouel.github.io/raffi/configuration/overview/)
- [CLI options](https://chmouel.github.io/raffi/reference/cli-options/)

## Screenshot

![Raffi native launcher with right-hand preview pane](website/src/assets/screenshots/main.png)

*See more screenshots [below](#screenshots)*

## Installation

Prebuilt binaries and packages are on the [GitHub releases page](https://github.com/chmouel/raffi/releases). They use the native interface by default. To use Fuzzel instead, install it and pass `-u fuzzel` or set `ui_type: fuzzel`.

On Arch Linux, install it from the AUR with a helper such as:

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

If you only use Fuzzel, you can build without the native UI:

```sh
cargo build --release --no-default-features
```

The binary drops from about 15 MB to about 1.1 MB. This build also leaves out config validation (`--check-config` and the startup warnings about unknown keys); `--features config-validation` adds it back for a few MB.

## Usage

Run `raffi` to pick one of your configured entries. Without a config, or with no valid launchers in it, Raffi lists the desktop applications installed on your system.

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

Raffi has two interfaces. The native one is the default: a built-in window with fuzzy search, keyboard navigation and themes. The other hands the list to the external Fuzzel launcher.

The native interface also comes with addons: a calculator, a currency converter, unit conversion, a color converter, a world clock, developer tools (encoders, hashes, UUID and password generators), a file browser, an emoji and Nerd Fonts picker, script filters, web searches, text snippets, clipboard history (through CopyQ), system commands (lock, suspend, log out, reboot, shut down), a window switcher for Hyprland, media controls and an audio device switcher. A preview pane on the right shows details for the selected item; `Ctrl+P` toggles it. `Ctrl+/` lists every keyword and shortcut your config defines.

See: [UI modes](https://chmouel.github.io/raffi/features/ui-modes/) · [Themes](https://chmouel.github.io/raffi/features/themes/) · [Calculator](https://chmouel.github.io/raffi/features/calculator/) · [Currency converter](https://chmouel.github.io/raffi/features/currency-converter/) · [Unit conversion](https://chmouel.github.io/raffi/features/unit-conversion/) · [Color converter](https://chmouel.github.io/raffi/features/color-converter/) · [World clock](https://chmouel.github.io/raffi/features/world-clock/) · [Developer tools](https://chmouel.github.io/raffi/features/developer-tools/) · [File browser](https://chmouel.github.io/raffi/features/file-browser/) · [Script filters](https://chmouel.github.io/raffi/features/script-filters/) · [Web search](https://chmouel.github.io/raffi/features/web-search/) · [Text snippets](https://chmouel.github.io/raffi/features/text-snippets/) · [Clipboard history](https://chmouel.github.io/raffi/features/clipboard-history/) · [System commands](https://chmouel.github.io/raffi/features/system-commands/) · [Window switcher](https://chmouel.github.io/raffi/features/window-switcher/) · [Media controls](https://chmouel.github.io/raffi/features/media-controls/) · [Audio switcher](https://chmouel.github.io/raffi/features/audio-switcher/)

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

The docs cover the rest of the configuration:

- [Configuration overview](https://chmouel.github.io/raffi/configuration/overview/)
- [Entries](https://chmouel.github.io/raffi/configuration/entries/)
- [General settings](https://chmouel.github.io/raffi/configuration/general-settings/)
- [Scripts](https://chmouel.github.io/raffi/configuration/scripts/)
- [Conditions](https://chmouel.github.io/raffi/configuration/conditions/)
- [Icons](https://chmouel.github.io/raffi/configuration/icons/)
- [Addon configuration](https://chmouel.github.io/raffi/reference/addon-configuration/)
- [YAML schema reference](https://chmouel.github.io/raffi/reference/yaml-schema/)

## Development

Issues and pull requests are welcome on GitHub.

To run `cargo clippy` through pre-commit hooks before pushing:

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
