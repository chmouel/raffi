<img src="assets/logo.png" alt="Raffi logo" width="120" align="right"/>

# Raffi

[![Release](https://img.shields.io/github/v/release/chmouel/raffi?style=flat-square)](https://github.com/chmouel/raffi/releases)
[![CI](https://img.shields.io/github/actions/workflow/status/chmouel/raffi/rust.yaml?branch=main&style=flat-square&label=CI)](https://github.com/chmouel/raffi/actions/workflows/rust.yaml)
[![Crates.io](https://img.shields.io/crates/v/raffi?style=flat-square)](https://crates.io/crates/raffi)
[![AUR](https://img.shields.io/aur/version/raffi-bin?style=flat-square)](https://aur.archlinux.org/packages/raffi-bin)
[![Docs](https://img.shields.io/badge/docs-chmouel.github.io%2Fraffi-2563eb?style=flat-square)](https://chmouel.github.io/raffi/)
[![License](https://img.shields.io/github/license/chmouel/raffi?style=flat-square)](LICENSE)

[Documentation](https://chmouel.github.io/raffi/) · [Releases](https://github.com/chmouel/raffi/releases) · [Issues](https://github.com/chmouel/raffi/issues) · [Screenshots](#screenshots)

![Raffi with the preview pane open](website/src/assets/screenshots/main.png)

Raffi is a keyboard-driven launcher for Wayland written in Rust. Drop it into a keybinding and start typing: with no configuration file at all, it searches your installed desktop applications, open windows, and system commands from a single box. You do not need a background daemon to run it. When you do want the extra speed or built-in clipboard recording, starting the optional [`raffi --server`](https://chmouel.github.io/raffi/reference/cli-options/#cache-management) keeps the graphics stack and fonts loaded so the window opens about a third faster, warms slow caches while Raffi is closed, and records everything you copy.

Once the window is up, type to filter, move with the arrow keys, and press Enter. A preview pane sits on the right (`Ctrl+P` toggles it) to show what the selected row will open or run. Lists with multiple views switch with `Ctrl+Tab` and `Ctrl+Shift+Tab`, and the footer shows where you are. Pressing `Ctrl+/` or typing `?` lists every active keyword and shortcut; if you forget one, typing a word like `volume` or `password` brings up the right keyword anyway.

Most built-in tools need no setup. Expressions like `2*(3+4)`, `$10 to eur`, `10 km to mi`, `#ff8800`, and `15:00 paris to nyc` answer directly in the list through the [calculator](https://chmouel.github.io/raffi/features/calculator/), [currency](https://chmouel.github.io/raffi/features/currency-converter/), [unit](https://chmouel.github.io/raffi/features/unit-conversion/), [color](https://chmouel.github.io/raffi/features/color-converter/), and [world clock](https://chmouel.github.io/raffi/features/world-clock/) addons. Starting a query with `/` or `~/` opens the [file browser](https://chmouel.github.io/raffi/features/file-browser/) with inline previews for source code, images, videos, and PDFs. For day-to-day desktop control, keywords handle [window switching](https://chmouel.github.io/raffi/features/window-switcher/) on Sway, Hyprland, and niri (`win `), [system actions](https://chmouel.github.io/raffi/features/system-commands/) (`sys `), [audio outputs and volume](https://chmouel.github.io/raffi/features/audio-switcher/) (`audio `), [media playback](https://chmouel.github.io/raffi/features/media-controls/) (`media `), [Bluetooth](https://chmouel.github.io/raffi/features/bluetooth/) (`bt `), [killing processes](https://chmouel.github.io/raffi/features/kill-process/) (`kill `), [developer utilities](https://chmouel.github.io/raffi/features/developer-tools/) (`dev `), emoji and Nerd Font icons (`emoji `), and running a command in a [terminal](https://chmouel.github.io/raffi/features/terminal/) (`> htop`).

Around your browser and daily work, Raffi reads [open tabs](https://chmouel.github.io/raffi/features/browser-tabs/) (`tabs `, via the Vicinae browser extension), [bookmarks](https://chmouel.github.io/raffi/features/bookmarks/) (`bm `), and [browser history](https://chmouel.github.io/raffi/features/browser-history/) (`hist `). It pulls credentials from [pass](https://chmouel.github.io/raffi/features/password-store/) (`pass `) or [Bitwarden](https://chmouel.github.io/raffi/features/bitwarden/) via `rbw` (`bw `), and an optional [Slack](https://chmouel.github.io/raffi/features/slack/) addon (`slack `) searches messages, opens conversations, lists unreads, and sets your status. The [clipboard history](https://chmouel.github.io/raffi/features/clipboard-history/) (`clip `, or `raffi clipboard` from scripts) works like Alfred's clipboard manager on macOS: it runs OCR over screenshots so you can search text inside images, expires one-time codes, skips password managers, tags each item with the source application's icon, and lets you save any text entry straight into your [snippets](https://chmouel.github.io/raffi/features/text-snippets/#saving-from-the-clipboard). You can extend the launcher with custom [web searches](https://chmouel.github.io/raffi/features/web-search/), [text snippets](https://chmouel.github.io/raffi/features/text-snippets/), and [script filters](https://chmouel.github.io/raffi/features/script-filters/), style it with built-in [themes](https://chmouel.github.io/raffi/features/themes/) or your own palette, or point it at [Fuzzel](https://chmouel.github.io/raffi/integration/fuzzel/) if you prefer that frontend.

## Installation

Install Raffi from your package manager, with Cargo, or from the [GitHub releases](https://github.com/chmouel/raffi/releases) page (see the [installation guide](https://chmouel.github.io/raffi/installation/) for building from source):

```sh
# Arch Linux (AUR)
yay -S raffi-bin

# Nix
nix-shell -p raffi

# Homebrew / Linuxbrew
brew tap chmouel/raffi https://github.com/chmouel/raffi
brew install raffi

# Cargo
cargo install raffi
```

## Keybindings

Bind `raffi` to a shortcut in your compositor. Inside Sway, Hyprland, and niri, Raffi asks the compositor to launch whatever you pick.

```config
# Sway (~/.config/sway/config)
bindsym Mod4+Space exec raffi
```

```lua
-- Hyprland (Lua)
hl.bind("SUPER + R", hl.dsp.exec_cmd("raffi"))
```

```kdl
// niri (~/.config/niri/config.kdl)
binds {
    Mod+R { spawn "raffi"; }
}
```

Window rules and fuller setups live on the [Sway](https://chmouel.github.io/raffi/integration/sway/), [Hyprland](https://chmouel.github.io/raffi/integration/hyprland/), and [niri](https://chmouel.github.io/raffi/integration/niri/) pages. Every command-line flag is documented in the [CLI reference](https://chmouel.github.io/raffi/reference/cli-options/), and the full keymap is on the [keyboard shortcuts](https://chmouel.github.io/raffi/reference/keyboard-shortcuts/) page.

## Configuration

Raffi starts without any config files. When you want to customize it, general settings and addons go in `~/.config/raffi/raffi.yaml`:

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

`main_view` chooses which sources appear in the default list (omitting it keeps the defaults; setting `null` or `[]` limits the list to apps and your own entries). Custom commands and inline scripts go in `~/.config/raffi/launchers.yaml`:

```yaml
firefox_private:
  binary: firefox
  args: [--private-window]
  icon: firefox
  description: Firefox private window
```

Check [`examples/raffi.yaml`](examples/raffi.yaml) for a minimal starter, [`examples/raffi.full.yaml`](examples/raffi.full.yaml) (also printed by `raffi --print-config`) for every available option, and [`examples/launchers.yaml`](examples/launchers.yaml) for sample entries. The [configuration guide](https://chmouel.github.io/raffi/configuration/overview/) walks through all of them.

## Screenshots

<details open>
<summary>Search and Recent Picks</summary>

<p><img width="800" alt="Main view with launchers and a recently picked demo snippet" src="website/src/assets/screenshots/main-recent.png" /></p>
<p><img width="800" alt="One search listing a launcher and matching demo windows" src="website/src/assets/screenshots/main-search.png" /></p>
<p><img width="800" alt="Main view narrowed to Launchers with Ctrl+Tab" src="website/src/assets/screenshots/main-source.png" /></p>
<p><img width="800" alt="A demo installed app and its private-window desktop action" src="website/src/assets/screenshots/installed-apps.png" /></p>

</details>

<details>
<summary>Desktop and Utilities</summary>

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

</details>

<details>
<summary>Clipboard and Passwords</summary>

<p><img width="800" alt="Demo clipboard history with multiline text preview" src="website/src/assets/screenshots/clipboard-history.png" /></p>
<p><img width="800" alt="Fictional password-store entries without decrypted secrets" src="website/src/assets/screenshots/password-store.png" /></p>
<p><img width="800" alt="Demo Bitwarden logins with public entry details" src="website/src/assets/screenshots/bitwarden.png" /></p>

</details>

<details>
<summary>Files and Previews</summary>

<p><img width="800" alt="File browser with directory preview" src="website/src/assets/screenshots/file-browser.png" /></p>
<p><img width="800" alt="File browser source code preview" src="website/src/assets/screenshots/file-browser-code-preview.png" /></p>
<p><img width="800" alt="File browser image preview" src="website/src/assets/screenshots/file-browser-image-preview.png" /></p>
<p><img width="800" alt="File browser image preview scrolled to file details" src="website/src/assets/screenshots/file-browser-image-scrolled.png" /></p>
<p><img width="800" alt="File browser showing hidden files" src="website/src/assets/screenshots/file-browser-hidden.png" /></p>
<p><img width="800" alt="Bundled file previewer highlighting demo Python source" src="website/src/assets/screenshots/file-browser-rich-preview.png" /></p>
<p><img width="800" alt="File browser rendering a demo PDF page" src="website/src/assets/screenshots/file-browser-pdf.png" /></p>
<p><img width="800" alt="File-only listing sorted by modification time" src="website/src/assets/screenshots/file-browser-filter.png" /></p>

</details>

<details>
<summary>Search, Web, Help, and Scripts</summary>

<p><img width="800" alt="Text snippets with expanded content preview" src="website/src/assets/screenshots/text-snippets.png" /></p>
<p><img width="800" alt="Text snippet with expanded dynamic placeholders and raw template" src="website/src/assets/screenshots/text-snippets-placeholders.png" /></p>
<p><img width="800" alt="Emoji picker with Unicode and glyph preview" src="website/src/assets/screenshots/emoji-picker.png" /></p>
<p><img width="800" alt="Web search with resolved URL preview" src="website/src/assets/screenshots/web-search.png" /></p>
<p><img width="800" alt="Browser tabs with site icons and a page preview" src="website/src/assets/screenshots/browser-tabs.png" /></p>
<p><img width="800" alt="Browser tabs setup steps when no browser is connected" src="website/src/assets/screenshots/browser-tabs-setup.png" /></p>
<p><img width="800" alt="Bookmarks toolbar folder with a bookmark's details in the preview" src="website/src/assets/screenshots/bookmarks.png" /></p>
<p><img width="800" alt="Browser history rows with site, profile and visit time" src="website/src/assets/screenshots/browser-history.png" /></p>
<p><img width="800" alt="Configured fallback search results and preview" src="website/src/assets/screenshots/fallbacks.png" /></p>
<p><img width="800" alt="Keyword suggestion while typing" src="website/src/assets/screenshots/keyword-suggestion.png" /></p>
<p><img width="800" alt="Keyword and shortcut help list" src="website/src/assets/screenshots/keyword-help.png" /></p>
<p><img width="800" alt="Keyboard shortcut help filtered to the preview shortcuts" src="website/src/assets/screenshots/keyword-help-shortcuts.png" /></p>
<p><img width="800" alt="Inline launcher script shown in the preview pane" src="website/src/assets/screenshots/launcher-script-preview.png" /></p>
<p><img width="800" alt="Timezone script filter with ANSI-formatted preview" src="website/src/assets/screenshots/script-filter-tz.png" /></p>
<p><img width="800" alt="Pull request script filter with CI status and details" src="website/src/assets/screenshots/script-filter-pr.png" /></p>
<p><img width="800" alt="Herdr demo workspace, session and pane tree" src="website/src/assets/screenshots/herdr-tree.png" /></p>
<p><img width="800" alt="Herdr demo agents grouped by status with terminal preview" src="website/src/assets/screenshots/herdr-status.png" /></p>

</details>

<details>
<summary>Themes</summary>

<p><img width="800" alt="Built-in light theme" src="website/src/assets/screenshots/light-theme.png" /></p>
<p><img width="800" alt="Catppuccin Mocha theme" src="website/src/assets/screenshots/theme-catppuccin.png" /></p>
<p><img width="800" alt="Nord theme" src="website/src/assets/screenshots/theme-nord.png" /></p>
<p><img width="800" alt="Gruvbox Dark theme" src="website/src/assets/screenshots/theme-gruvbox.png" /></p>
<p><img width="800" alt="Solarized Light theme" src="website/src/assets/screenshots/theme-solarized-light.png" /></p>
<p><img width="800" alt="Tokyo Night theme" src="website/src/assets/screenshots/theme-tokyonight.png" /></p>

</details>

## Contributing

Bug reports and pull requests are welcome on [GitHub](https://github.com/chmouel/raffi).

## License

Released under the [Apache 2.0 License](LICENSE).

## Author

[![Sponsor](https://img.shields.io/badge/Sponsor-❤️-ff69b4?style=for-the-badge&logo=github)](https://github.com/sponsors/chmouel)

Chmouel Boudjnah · [Fediverse](https://fosstodon.org/@chmouel) · [Twitter](https://twitter.com/chmouel) · [Blog](https://blog.chmouel.com)
