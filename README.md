# Raffi - Fuzzel Launcher Using YAML Configuration

![image](https://github.com/chmouel/raffi/assets/98980/04d6af0f-2a80-47d5-a2ec-95443a629305)

*(This uses my Fuzzel configuration, see below for more details)*

## Description

Raffi is a launcher for the [Fuzzel](https://codeberg.org/dnkl/fuzzel) utility that uses a YAML configuration file
to define the commands to be executed.

## Installation

### [Binaries](https://github.com/chmouel/raffi/releases)

Visit the [release](https://github.com/chmouel/raffi/releases) page and download the archive or package for your platform.

Ensure you have [Fuzzel](https://codeberg.org/dnkl/fuzzel) installed.

### [Homebrew](https://homebrew.sh)

```shell
brew tap chmouel/raffi https://github.com/chmouel/raffi
brew install raffi
```

### [Crates.io](https://crates.io/crates/raffi)

```shell
cargo install raffi
```

### [Arch](https://aur.archlinux.org/packages/raffi-bin)

Using your preferred AUR helper, for example, [yay](https://github.com/Jguer/yay):

```shell
yay -S raffi-bin
```

### [NixOS / Nix](https://nixos.org) (unstable)

```shell
nix-shell -p raffi
```

## Usage

You can launch Raffi directly, and it will run the binary and arguments as defined in the [configuration](#configuration).

Use the `-p/--print-only` option to only print the command to be executed.

Specify a custom configuration file with the `-c/--configfile` option.

Icon paths are automatically searched on your system and cached. To refresh the cache, use the `-r/--refresh-cache` option.

### Sway

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

## Configuration

### Fuzzel

First, configure your Fuzzel appearance and behavior by editing the file `~/.config/fuzzel/fuzzel.ini`. See the manpages [here](https://man.archlinux.org/man/fuzzel.ini.5.en). Below is my configuration, which matches the screenshot above:

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

### Raffi

The Raffi configuration file is located at `$HOME/.config/raffi/raffi.yaml` and has the following structure:

```yaml
firefox:
  binary: firefox
  args: [--marionette]
  icon: firefox
  description: Firefox browser with marionette enabled
```

- **binary**: The binary to be executed (if it does not exist in the PATH, it will be skipped).
- **description**: The description to be displayed in the launcher.
- **args**: The arguments to be passed to the binary as an array, e.g., `[foo, bar]` (optional).
- **icon**: The icon to be displayed in the launcher. If not specified, it will
  try to use the binary name (optional). Icons are searched in
  `/usr/share/icons`, `/usr/share/pixmaps`, `$HOME/.local/share/icons`, or
  `$XDG_DATA_HOME` if set and matched to the icon name. The icons paths are
  cached for optimisation, use the `-r` option to refresh it. You can also
  specify a full path to the icon.

### Conditions

There is limited support for conditions, allowing you to run a command only if a specific condition is met. These conditions are optional and cannot be combined.

- **ifexist**: Display the entry if a binary exists in the PATH or if the full path is specified.
- **ifenvset**: Display the entry if the environment variable is set.
- **ifenvnotset**: Display the entry if the environment variable is not set.
- **ifenveq**: Display the entry if the environment variable equals a specified value.

#### Example

Here is an example of how to use conditions, this will only display the entry
if the `DESKTOP_SESSION` environment variable is set to `GNOME` and the
`WAYLAND_DISPLAY` environment variable is set and the `firefox` binary exists
in the PATH:

```yaml
ifenveq: [DESKTOP_SESSION, GNOME]
ifenvset: WAYLAND_DISPLAY
ifexist: firefox
```

See the file located in [examples/raffi.yaml](./examples/raffi.yaml) for a more comprehensive example.

## Copyright

[Apache-2.0](./LICENSE)

## Authors

- Chmouel Boudjnah <https://github.com/chmouel>
  - Fediverse - <[@chmouel@fosstodon.org](https://fosstodon.org/@chmouel)>
  - Twitter - <[@chmouel](https://twitter.com/chmouel)>
  - Blog - <[https://blog.chmouel.com](https://blog.chmouel.com)>
