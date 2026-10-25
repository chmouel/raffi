<img src="assets/logo.png" alt="Raffi logo" width="120" align="right"/>

# Raffi

Raffi is a keyboard-driven search box for your Wayland desktop. From one place you can open apps, browse files, switch windows, paste from your clipboard history, fill in passwords, do sums, convert currencies and a lot more. It is written in Rust, starts fast, and needs no daemon or database.

It works as soon as it is installed. A YAML file lets you pick a theme, choose which addons are on, add web searches and snippets, and add commands or scripts of your own.

![Raffi with the preview pane open](website/src/assets/screenshots/main.png)

The full documentation lives at <https://chmouel.github.io/raffi/>. More screenshots are [further down](#screenshots).

## Installation

Arch Linux (AUR):

```sh
yay -S raffi-bin
```

Nix:

```sh
nix-shell -p raffi
```

Homebrew or Linuxbrew:

```sh
brew tap chmouel/raffi https://github.com/chmouel/raffi
brew install raffi
```

With Cargo:

```sh
cargo install raffi
```

Prebuilt binaries are on the [releases page](https://github.com/chmouel/raffi/releases). The [installation guide](https://chmouel.github.io/raffi/installation/) covers building from source.

## Bind it to a key

Run `raffi` and start typing. With no config yet, Raffi searches your installed applications, open windows and system commands in one list, so you can try it straight away.

Most people bind it to a key in their compositor. For Sway:

```config
bindsym Mod4+Space exec raffi
```

For Hyprland's Lua config:

```lua
hl.bind("SUPER + R", hl.dsp.exec_cmd("raffi"))
```

For niri:

```kdl
binds {
    Mod+R { spawn "raffi"; }
}
```

Inside Hyprland, niri and Sway, Raffi asks the compositor to start the app you pick. The [Sway](https://chmouel.github.io/raffi/integration/sway/), [Hyprland](https://chmouel.github.io/raffi/integration/hyprland/) and [niri](https://chmouel.github.io/raffi/integration/niri/) pages cover window rules and more examples, and the [CLI reference](https://chmouel.github.io/raffi/reference/cli-options/) lists every flag.

## Features

Type to search, use the arrow keys to move, and press Enter. A preview pane on the right shows what the selected entry will do, and `Ctrl+P` hides or shows it. Press `Ctrl+/`, or type `?` on its own, to see every keyword and shortcut your config turns on. If you forget a keyword, type what you want, such as `volume` or `password`, and Raffi suggests the right one.

Some lists have more than one view, and `Ctrl+Tab` always switches between them, with `Ctrl+Shift+Tab` going back. The footer tells you which view you are on. See [keyboard shortcuts](https://chmouel.github.io/raffi/reference/keyboard-shortcuts/) for the full list.

Most addons start with a keyword followed by a space. Others answer as soon as your search looks like something they understand.

Answers as you type:

- [Calculator](https://chmouel.github.io/raffi/features/calculator/): type a sum like `2*(3+4)`
- [Currency converter](https://chmouel.github.io/raffi/features/currency-converter/): `$10 to eur`
- [Unit conversion](https://chmouel.github.io/raffi/features/unit-conversion/): `10 km to mi`, `70f to c`
- [Color converter](https://chmouel.github.io/raffi/features/color-converter/): `#ff8800` or `rgb(255 136 0)`
- [World clock](https://chmouel.github.io/raffi/features/world-clock/): `time tokyo`, `15:00 paris to nyc`
- [Developer tools](https://chmouel.github.io/raffi/features/developer-tools/): `dev ` to encode, decode or hash text, plus UUID and password generators

Search and pick:

- [File browser](https://chmouel.github.io/raffi/features/file-browser/): start with `/` or `~/`, with previews of PDFs, images, videos and source code
- Emoji and Nerd Font icons: `emoji `
- [Clipboard history](https://chmouel.github.io/raffi/features/clipboard-history/) from CopyQ: `clip `
- [Text snippets](https://chmouel.github.io/raffi/features/text-snippets/), [web searches](https://chmouel.github.io/raffi/features/web-search/) and [script filters](https://chmouel.github.io/raffi/features/script-filters/), each under a keyword you choose
- [Installed apps](https://chmouel.github.io/raffi/features/installed-apps/), with their actions such as opening a private window

Control your desktop:

- [System commands](https://chmouel.github.io/raffi/features/system-commands/) to lock, suspend, log out or shut down: `sys `
- [Window switcher](https://chmouel.github.io/raffi/features/window-switcher/) for Hyprland, niri and sway: `win `
- [Browser tabs](https://chmouel.github.io/raffi/features/browser-tabs/) through the Vicinae browser extension: `tabs `
- [Bookmarks](https://chmouel.github.io/raffi/features/bookmarks/): search and open browser bookmarks: `bm `
- [Browser history](https://chmouel.github.io/raffi/features/browser-history/): search visited pages browser by browser: `hist `
- [Media controls](https://chmouel.github.io/raffi/features/media-controls/): `media `
- [Audio devices and volume](https://chmouel.github.io/raffi/features/audio-switcher/): `audio `
- [Bluetooth](https://chmouel.github.io/raffi/features/bluetooth/): `bt `
- [Stop a process](https://chmouel.github.io/raffi/features/kill-process/): `kill `
- [Run a command in a terminal](https://chmouel.github.io/raffi/features/terminal/): `> htop`

Passwords, copied or typed for you:

- [pass](https://chmouel.github.io/raffi/features/password-store/): `pass `
- [Bitwarden](https://chmouel.github.io/raffi/features/bitwarden/) through rbw: `bw `

[Slack](https://chmouel.github.io/raffi/features/slack/) is off until you set it up. Then `slack ` opens conversations, searches messages, lists what you haven't read and sets your status.

You can change how Raffi looks with the built-in [themes](https://chmouel.github.io/raffi/features/themes/) or your own colors.

If you already use [Fuzzel](https://codeberg.org/dnkl/fuzzel), Raffi can show your apps and entries there instead. See the [Fuzzel page](https://chmouel.github.io/raffi/integration/fuzzel/) for what that gives up.

## Configuration

Raffi needs no config to start. When you want to change something, settings go in `~/.config/raffi/raffi.yaml`:

```yaml
general:
  theme: dark
  main_view: [launchers, windows, browser_tabs, system, clipboard]
addons:
  apps:
    enabled: true
  web_searches:
    - name: Google
      keyword: g
      url: "https://google.com/search?q={query}"
```

`main_view` picks what shows up in the main list as you type. Omitting it keeps the default sources even after you add a config file; `null` or `[]` shows only apps and your own entries. `apps` keeps installed applications listed after you add entries of your own, and `web_searches` turns `g rust` into a Google search.

Commands and scripts of your own go in `~/.config/raffi/launchers.yaml`:

```yaml
firefox_private:
  binary: firefox
  args: [--private-window]
  icon: firefox
  description: Firefox private window
```

[`examples/raffi.yaml`](examples/raffi.yaml) shows every setting with a comment, and [`examples/launchers.yaml`](examples/launchers.yaml) has entries to copy from. The [configuration guide](https://chmouel.github.io/raffi/configuration/overview/) covers settings, addons and custom entries.

## Contributing

Bug reports and pull requests are welcome on [GitHub](https://github.com/chmouel/raffi).

## Screenshots

### Search and Recent Picks

<p><img width="800" alt="Main view with launchers and a recently picked demo snippet" src="website/src/assets/screenshots/main-recent.png" /></p>
<p><img width="800" alt="One search listing a launcher and matching demo windows" src="website/src/assets/screenshots/main-search.png" /></p>
<p><img width="800" alt="Main view narrowed to Launchers with Ctrl+Tab" src="website/src/assets/screenshots/main-source.png" /></p>
<p><img width="800" alt="A demo installed app and its private-window desktop action" src="website/src/assets/screenshots/installed-apps.png" /></p>

### Desktop and Utilities

<p><img width="800" alt="Window switcher with demo application and workspace details" src="website/src/assets/screenshots/window-switcher.png" /></p>
<p><img width="800" alt="Typed command ready to run in a terminal" src="website/src/assets/screenshots/terminal.png" /></p>
<p><img width="800" alt="Audio outputs with demo headphones selected" src="website/src/assets/screenshots/audio-switcher.png" /></p>
<p><img width="800" alt="Typed output volume of 40 percent" src="website/src/assets/screenshots/audio-volume.png" /></p>
<p><img width="800" alt="Media playback controls and demo track details" src="website/src/assets/screenshots/media-controls.png" /></p>
<p><img width="800" alt="Demo Bluetooth devices with connection and battery details" src="website/src/assets/screenshots/bluetooth.png" /></p>
<p><img width="800" alt="Session and power commands" src="website/src/assets/screenshots/system-commands.png" /></p>
<p><img width="550" alt="Disposable demo worker's process details and signal actions" src="website/src/assets/screenshots/kill-process.png" /></p>
<p><img width="800" alt="Calculator with expression preview and number bases" src="website/src/assets/screenshots/calculator.png" /></p>
<p><img width="800" alt="Currency converter with multi-currency preview" src="website/src/assets/screenshots/currency-converter.png" /></p>
<p><img width="800" alt="Unit conversion from kilometres per hour to miles per hour" src="website/src/assets/screenshots/unit-conversion.png" /></p>
<p><img width="800" alt="Color conversion with swatches and CSS formats" src="website/src/assets/screenshots/color-converter.png" /></p>
<p><img width="800" alt="Developer encoding and hashing tools" src="website/src/assets/screenshots/developer-tools.png" /></p>
<p><img width="800" alt="Time conversion between London and Tokyo" src="website/src/assets/screenshots/world-clock.png" /></p>

### Clipboard and Passwords

<p><img width="800" alt="Demo clipboard history with multiline text preview" src="website/src/assets/screenshots/clipboard-history.png" /></p>
<p><img width="800" alt="Fictional password-store entries without decrypted secrets" src="website/src/assets/screenshots/password-store.png" /></p>
<p><img width="800" alt="Demo Bitwarden logins with public entry details" src="website/src/assets/screenshots/bitwarden.png" /></p>

### Files and Previews

<p><img width="800" alt="File browser with directory preview" src="website/src/assets/screenshots/file-browser.png" /></p>
<p><img width="800" alt="File browser source code preview" src="website/src/assets/screenshots/file-browser-code-preview.png" /></p>
<p><img width="800" alt="File browser image preview" src="website/src/assets/screenshots/file-browser-image-preview.png" /></p>
<p><img width="800" alt="File browser image preview scrolled to file details" src="website/src/assets/screenshots/file-browser-image-scrolled.png" /></p>
<p><img width="800" alt="File browser showing hidden files" src="website/src/assets/screenshots/file-browser-hidden.png" /></p>
<p><img width="800" alt="Bundled file previewer highlighting demo Python source" src="website/src/assets/screenshots/file-browser-rich-preview.png" /></p>
<p><img width="800" alt="File browser rendering a demo PDF page" src="website/src/assets/screenshots/file-browser-pdf.png" /></p>
<p><img width="800" alt="File-only listing sorted by modification time" src="website/src/assets/screenshots/file-browser-filter.png" /></p>

### Search and Web

<p><img width="800" alt="Text snippets with expanded content preview" src="website/src/assets/screenshots/text-snippets.png" /></p>
<p><img width="800" alt="Text snippet with expanded dynamic placeholders and raw template" src="website/src/assets/screenshots/text-snippets-placeholders.png" /></p>
<p><img width="800" alt="Emoji picker with Unicode and glyph preview" src="website/src/assets/screenshots/emoji-picker.png" /></p>
<p><img width="800" alt="Web search with resolved URL preview" src="website/src/assets/screenshots/web-search.png" /></p>
<p><img width="800" alt="Browser tabs with site icons and a page preview" src="website/src/assets/screenshots/browser-tabs.png" /></p>
<p><img width="800" alt="Browser tabs setup steps when no browser is connected" src="website/src/assets/screenshots/browser-tabs-setup.png" /></p>
<p><img width="800" alt="Bookmarks toolbar folder with a bookmark's details in the preview" src="website/src/assets/screenshots/bookmarks.png" /></p>
<p><img width="800" alt="Browser history rows with site, profile and visit time" src="website/src/assets/screenshots/browser-history.png" /></p>
<p><img width="800" alt="Configured fallback search results and preview" src="website/src/assets/screenshots/fallbacks.png" /></p>

### Search, Help, and Scripts

<p><img width="800" alt="Keyword suggestion while typing" src="website/src/assets/screenshots/keyword-suggestion.png" /></p>
<p><img width="800" alt="Keyword and shortcut help list" src="website/src/assets/screenshots/keyword-help.png" /></p>
<p><img width="800" alt="Keyboard shortcut help filtered to the preview shortcuts" src="website/src/assets/screenshots/keyword-help-shortcuts.png" /></p>
<p><img width="800" alt="Inline launcher script shown in the preview pane" src="website/src/assets/screenshots/launcher-script-preview.png" /></p>
<p><img width="800" alt="Timezone script filter with ANSI-formatted preview" src="website/src/assets/screenshots/script-filter-tz.png" /></p>
<p><img width="800" alt="Pull request script filter with CI status and details" src="website/src/assets/screenshots/script-filter-pr.png" /></p>
<p><img width="800" alt="Herdr demo workspace, session and pane tree" src="website/src/assets/screenshots/herdr-tree.png" /></p>
<p><img width="800" alt="Herdr demo agents grouped by status with terminal preview" src="website/src/assets/screenshots/herdr-status.png" /></p>

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
