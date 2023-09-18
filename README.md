# raffi - wofi launcher on yaml config file

## Description

raffi is a launcher for wofi, it uses a yaml config file to define the commands to be executed.

## Installation

### [Binaries](https://github.com/chmouel/raffi/releases)

Go to the [release](https://github.com/chmouel/raffi/releases) page and grab
the archive or package targeting your platform.

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

With your favourite aurhelper for example [yay](https://github.com/Jguer/yay) :

```shell
yay -S raffi-bin
```
## Usage

You can launch it directly and it will run the binary and args as defined in the [configuration](#configuration).

With the option `-p/--print-only` it will only print the command to be executed.

You can specify a custom config file with the `-c/--configfile` option.

Icon path are automatically searched on your system and cached, if you need to refresh the cache 
you can use the option `-r/--refresh-cache`.

### Sway

Here is an example on how to use this with Sway:

```c
// set a variable that can be easily used later in the config file
// those variables are optionals
set $menu raffi -p

// Mod4 is the Super key for me but use whatever you want.
set $super Mod4

// will bind the super+space key to launch the launcher
bindsym $super+Space exec $menu|xargs swaymsg exec --
```

## Configuration

The configuration file is located at `$HOME/.config/raffi/raffi.yaml` and it has the following structure:

```yaml
firefox:
    binary: firefox
    args: [--marionette]
    icon: firefox
    description: Firefox browser with marionette enabled
```

**binary**: The binary to be executed
**description**: The description to be displayed in the launcher
**args**: The arguments to be passed to the binary as array i.e: `[foo, bar]` (optional)
**icon**: The icon to be displayed in the launcher if not specified it will try to use binary name (optional)

icons are searched in /usr/share/icons, /usr/share/pixmaps,
$HOME/.local/share/icons or in $XDG_DATA_HOME if set  and matched to the icon
name. You can as well specify the full path in there.

### Conditions

there is some mininal support for conditions, to let you run a command only if
a condition is met. They are all optional and cannot be used in combination.

**ifexist**: show entry if a file exist in PATH or the full path can be specified
**ifenvset**: show entry if the environment variable is set
**ifenvnotset**: show entry if the environment variable is not set
**ifenveq**: show entry if the environment variable is equal to the value, for example:

```yaml
ifenvset: [FOO, bar]
```

will only show the entry if an environment variable FOO is set and its value is bar.

## Example

See the file located in [examples/raffi.yaml](./examples/raffi.yaml) for a comprehensive example.

## Copyright

[Apache-2.0](./LICENSE)

## Authors

- Chmouel Boudjnah <https://github.com/chmouel>
  - Fediverse - <[@chmouel@fosstodon.org](https://fosstodon.org/@chmouel)>
  - Twitter - <[@chmouel](https://twitter.com/chmouel)>
  - Blog  - <[https://blog.chmouel.com](https://blog.chmouel.com)>
