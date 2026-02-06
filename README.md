# Raffi Application Launcher

Raffi is an application launcher designed to sit on top of Fuzzel, or, if preferred, operate using its own built‑in interface. It allows commands and scripts to be defined in a YAML configuration file, with support for icons, arguments, conditional visibility, and script execution through configurable interpreters.

![image](https://github.com/chmouel/raffi/assets/98980/04d6af0f-2a80-47d5-a2ec-95443a629305)

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

Native mode uses an internal iced‑based graphical interface with fuzzy search, keyboard navigation, and a dark theme. It is suitable if you prefer a self‑contained graphical window.

### Native Interface Extras

The native interface includes a built‑in calculator which evaluates expressions as you type. Standard mathematical operators are supported, along with functions such as `sqrt`, `sin`, `cos`, `tan`, `log`, `ln`, `exp`, `abs`, `floor`, and `ceil`. Results can be copied to the clipboard using Enter, provided `wl-copy` is available.

The native interface also includes a currency converter. Enter an amount prefixed with `$` followed by a target currency. Exchange rates are fetched from the Frankfurter API and cached for one hour.

Example inputs:

```
$10 to eur
$50 gbp to usd
$100eur to jpy
```

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

## Development

Contributions are welcome. Issues, feature requests, and pull requests can be submitted via GitHub.

To enable pre‑commit hooks that run `cargo clippy` before pushing:

```sh
pip install pre-commit
pre-commit install
```

## Licence

This project is released under the MIT Licence.

## Author

Chmouel Boudjnah

- GitHub: [https://github.com/chmouel](https://github.com/chmouel)
- Fediverse: [https://fosstodon.org/@chmouel](https://fosstodon.org/@chmouel)
- Twitter: [https://twitter.com/chmouel](https://twitter.com/chmouel)
- Blog: [https://blog.chmouel.com](https://blog.chmouel.com)
