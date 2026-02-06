# Raffi

![image](https://github.com/chmouel/raffi/assets/98980/04d6af0f-2a80-47d5-a2ec-95443a629305)

## Overview

Raffi is a launcher that wraps the [Fuzzel](https://codeberg.org/dnkl/fuzzel) utility (or via its own UI), letting you to define commands and scripts in a YAML configuration file. It supports icons, custom arguments, conditional display, and script execution with configurable interpreters.

## Installation

### [Binaries](https://github.com/chmouel/raffi/releases)

Visit the [release](https://github.com/chmouel/raffi/releases) page and download the archive or package for your platform.

Ensure you have [Fuzzel](https://codeberg.org/dnkl/fuzzel) installed.


### [Arch](https://aur.archlinux.org/packages/raffi-bin)

Using your preferred AUR helper, for example, [yay](https://github.com/Jguer/yay):

```shell
yay -S raffi-bin
```

### [NixOS / Nix](https://nixos.org) (unstable)

```shell
nix-shell -p raffi
```

### [LinuxBrew/Homebrew](https://homebrew.sh)

```shell
brew tap chmouel/raffi https://github.com/chmouel/raffi
brew install raffi
```

### [Crates.io](https://crates.io/crates/raffi)

```shell
cargo install raffi
```

### [Source](https://github.com/chmouel/raffi)

To install Raffi from source, clone the repository and build it using Cargo:

```sh
git clone https://github.com/chmouel/raffi.git
cd raffi
cargo build --release
```

#### Building Without Wayland UI

If you only need the Fuzzel UI and want to reduce binary size significantly, build with the Native feature disabled:

```sh
cargo build --release --no-default-features
```

This reduces the binary size from **15 MB** (with Native UI) to **1.1 MB** (93% smaller), as the Native UI depends on the heavy `iced` GUI framework. Use this option if you only plan to use Fuzzel or need a minimal installation.

## Usage

Run `raffi` to launch the configured items through Fuzzel. The application will execute the selected entry according to your configuration.

Common options:

- `-p/--print-only`: Print the command instead of executing it
- `-c/--configfile <FILE>`: Specify a custom configuration file
- `-r/--refresh-cache`: Refresh the cached icon paths
- `-I/--disable-icons`: Run without icons (marginally faster)
- `-u/--ui-type <TYPE>`: Select UI type (`fuzzel` or `native`, default: `fuzzel`)
- `--default-script-shell <SHELL>`: Shell for scripts (default: `bash`)
- `--version`: Show version
- `--help`: Show all options

### Integration with Window Managers

#### Sway

Here is an example of how to use Raffi with Sway:

```config
// Set a variable that can be easily used later in the config file
// These variables are optional
set $menu raffi -p

// Mod4 is the Super key for me, but use whatever you want.
set $super Mod4

// Bind the Super+Space key to launch the launcher
bindsym $super+Space exec $menu | xargs swaymsg exec --
```

#### Hyprland

```conf
$super = SUPER
bind = $super, R, exec, (val=$(raffi -pI); echo $val | grep -q . && hyprctl dispatch exec "$val")
```

### User Interface Options

Raffi supports two UI options via the `--ui-type` flag:

**Fuzzel** (default): External launcher using [Fuzzel](https://codeberg.org/dnkl/fuzzel). Good integration on Wayland.

**Native**: Built-in GUI using the [iced](https://iced.rs/) framework. Displays a dark-themed window with fuzzy search. Navigation via arrow keys, `Enter` to select, `Esc` to cancel. Useful if you prefer a native window over an external launcher.

#### Calculator Feature (Native UI only)

The Native UI includes a built-in calculator that automatically detects and evaluates math expressions as you type:

- Type any math expression like `2+2`, `sqrt(16)`, or `(10+5)*2`
- The result appears as the first item with accent color: `= 2 + 2 = 4`
- Press `Enter` to copy the result to clipboard (requires `wl-copy`)
- Supports basic operators: `+`, `-`, `*`, `/`, `^`, `%`
- Supports functions: `sqrt()`, `sin()`, `cos()`, `tan()`, `log()`, `ln()`, `exp()`, `abs()`, `floor()`, `ceil()`

#### Currency Converter (Native UI only)

Type `$` followed by an amount to convert currencies:

- `$10 to eur` - convert 10 USD to EUR
- `$50 gbp to usd` - convert 50 GBP to USD
- `$100eur to jpy` - convert 100 EUR to JPY

Type just `$` to see usage hints. Press `Enter` to copy the result to clipboard. Rates fetched from [Frankfurter API](https://frankfurter.dev/) and cached for 1 hour.

#### Configuration Example

Example with Native UI in Sway:

```config
set $super Mod4
bindsym $super+Space exec raffi -u native
for_window [app_id="com.chmouel.raffi"] floating enable, resize set 800 600, move position center
```

<img width="2575" height="1978" alt="raffi-native" src="https://github.com/user-attachments/assets/843fdce9-bcb3-4fc0-8f05-0e4ce5131f6c" />

## Configuration

### Fuzzel Configuration

Configure Fuzzel's appearance via `~/.config/fuzzel/fuzzel.ini`. See the [manpage](https://man.archlinux.org/man/fuzzel.ini.5.en) for options. Example:

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

Configuration goes in `$HOME/.config/raffi/raffi.yaml`. Basic example:

```yaml
firefox:
  binary: firefox
  args: [--marionette]
  icon: firefox
  description: Firefox browser with marionette enabled
```

Fields:

- `binary`: The executable to run. Skipped if not in PATH.
- `description`: Label shown in the launcher.
- `args`: Command-line arguments as an array (optional).
- `icon`: Icon name or full path (optional). Searched in standard directories. Icon paths are cached; refresh with `-r`.
- `script`: Inline script to execute (see below).
- `disabled`: Set to `true` to hide the entry.

### Scripts

Define inline scripts instead of binaries. Scripts run via `bash` by default, or specify a different interpreter with `--default-script-shell`. Use the `binary` field to set the interpreter explicitly. Example:

```yaml
hello_script:
  script: |
    echo "hello world and show me your env"
    env
  description: "Hello Script"
  icon: "script"
```

With a different interpreter:

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

With interpreter arguments:

```yaml
hello_script:
  binary: sh
  args: ["-xv"]
  script: |
    echo "hello world and show me your env"
    env
  description: "Hello debug"
  icon: "script"
```

### Conditional Display

Entries can be shown or hidden based on conditions. Conditions are optional and cannot be combined.

- `ifexist`: Show if binary exists in PATH or at full path.
- `ifenvset`: Show if environment variable is set.
- `ifenvnotset`: Show if environment variable is not set.
- `ifenveq`: Show if environment variable equals a specified value.

Example:

```yaml
ifenveq: [DESKTOP_SESSION, GNOME]
ifenvset: WAYLAND_DISPLAY
ifexist: firefox
```

See the file located in [examples/raffi.yaml](./examples/raffi.yaml) for a more comprehensive example.

## Development

Contributions welcome. For issues, feature requests, or pull requests, see the [GitHub repository](https://github.com/chmouel/raffi).

To set up pre-commit hooks that run `cargo clippy` before pushing:

```sh
pip install pre-commit
pre-commit install
```

## License

This project is licensed under the MIT License.

## Authors

- Chmouel Boudjnah <https://github.com/chmouel>
  - Fediverse - <[@chmouel@fosstodon.org](https://fosstodon.org/@chmouel)>
  - Twitter - <[@chmouel](https://twitter.com/chmouel)>
  - Blog - <[https://blog.chmouel.com](https://blog.chmouel.com)>
