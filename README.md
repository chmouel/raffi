# Raffi - Fuzzel Launcher Using YAML Configuration

![image](https://github.com/chmouel/raffi/assets/98980/04d6af0f-2a80-47d5-a2ec-95443a629305)

> [!NOTE]
> This uses my Fuzzel configuration, see below for more details

## Description

Raffi is a launcher for the [Fuzzel](https://codeberg.org/dnkl/fuzzel) utility
that uses a YAML configuration file
to define the commands to be executed.

## Features

- Launch applications with custom configurations.
- Support for icons.
- Script execution with a configurable interpreter.

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
### [Source](https://github.com/chmouel/raffi)

To install Raffi from source, clone the repository and build it using Cargo:

```sh
git clone https://github.com/chmouel/raffi.git
cd raffi
cargo build --release
```

## Usage

You can launch Raffi directly, and it will run the binary and arguments as defined in the [configuration](#configuration).

Use the `-p/--print-only` option to only print the command to be executed.

Specify a custom configuration file with the `-c/--configfile` option.

Icon paths are automatically searched on your system and cached. To refresh the
cache, use the `-r/--refresh-cache` option. If you want to have fuzzel running
faster you can use the option `-I/--disable-icons` to disable them.

### Command-line Options

```sh
raffi [OPTIONS]
```

Options:

- `--help`: Print help message.
- `--version`: Print version.
- `--configfile <FILE>`: Specify the config file location.
- `--print-only`: Print the command to stdout, do not run it.
- `--refresh-cache`: Refresh the icon cache.
- `--no-icons`: Do not show icons.
- `--default-script-shell <SHELL>`: Default shell when using scripts (default: `bash`).

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
  cached for optimization, use the `-r` option to refresh it. You can also
  specify a full path to the icon.
- **script**: [See below](#script-feature) for more information.
- **disabled**: If set to `true`, the entry will be disabled.

### Script Feature

You can define a script to be executed instead of a binary. The script will be executed using the default script shell `bash` unless you specify another one in `--default-script-shell`.

Here is an example configuration with a script:

```yaml
hello_script:
  script: |
    echo "hello world and show me your env"
    env
  description: "Hello Script"
  icon: "script"
```

If you want a script running, for example, with `python3`, you can specify it like this:

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

If you want to add some specific arguments to the interpreter, you can do it like this:

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

### Conditions

There is limited support for conditions, allowing you to run a command only if a specific condition is met. These conditions are optional and cannot be combined.

- **ifexist**: Display the entry if a binary exists in the PATH or if the full path is specified.
- **ifenvset**: Display the entry if the environment variable is set.
- **ifenvnotset**: Display the entry if the environment variable is not set.
- **ifenveq**: Display the entry if the environment variable equals a specified value.

#### Example

Here is an example of how to use conditions. This will only display the entry
if the `DESKTOP_SESSION` environment variable is set to `GNOME` and the
`WAYLAND_DISPLAY` environment variable is set and the `firefox` binary exists
in the PATH:

```yaml
ifenveq: [DESKTOP_SESSION, GNOME]
ifenvset: WAYLAND_DISPLAY
ifexist: firefox
```

See the file located in [examples/raffi.yaml](./examples/raffi.yaml) for a more comprehensive example.

## Troubleshooting

### Common Issues

- **Binary not found**: Ensure that the binary specified in the configuration file exists in the PATH.
- **Invalid configuration**: Verify that the YAML configuration file is correctly formatted and all required fields are provided.
- **Icons not displayed**: Ensure that the icon paths are correct and refresh the icon cache using the `--refresh-cache` option if necessary.

### Debugging

Use the `--print-only` option to print the command that will be executed. This can help identify issues with the configuration or command execution.

## Development

All contributions are welcome! If you have any suggestions, bug reports, or feature requests, please open an issue or a pull request.

### Pre-commit Hooks

To ensure code quality, you can set up pre-commit hooks to run `cargo clippy` automatically before pushing commits. First, install pre-commit:

```sh
pip install pre-commit
```

Then, install the pre-commit hooks:

```sh
pre-commit install
```

This will automatically run `cargo clippy` before each commit to catch any potential issues.

## License

This project is licensed under the MIT License.

## Authors

- Chmouel Boudjnah <https://github.com/chmouel>
  - Fediverse - <[@chmouel@fosstodon.org](https://fosstodon.org/@chmouel)>
  - Twitter - <[@chmouel](https://twitter.com/chmouel)>
  - Blog - <[https://blog.chmouel.com](https://blog.chmouel.com)>
