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

Running `raffi` launches configured entries through the selected interface. If your config does not define any valid launchers yet, Raffi falls back to auto-detected desktop applications from your system's `.desktop` files.

See the full CLI reference: <https://chmouel.github.io/raffi/reference/cli-options/>

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

See also: [Sway integration](https://chmouel.github.io/raffi/integration/sway/) · [Hyprland integration](https://chmouel.github.io/raffi/integration/hyprland/) · [Fuzzel integration](https://chmouel.github.io/raffi/integration/fuzzel/)

## User Interfaces

Raffi supports two interface modes:

- **Fuzzel** — uses the external Fuzzel launcher, integrates naturally with Wayland environments.
- **Native** — built-in iced-based GUI with fuzzy search, keyboard navigation, and theme support.

The native interface includes built-in addons: calculator, currency converter, file browser, emoji/Nerd Fonts picker, script filters, web searches, and text snippets.

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
