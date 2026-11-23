<img src="assets/logo.png" alt="Raffi logo" width="120" align="right"/>

# Raffi - A fast rust launcher on Wayland

[![Release](https://img.shields.io/github/v/release/chmouel/raffi?style=flat-square)](https://github.com/chmouel/raffi/releases)
[![CI](https://img.shields.io/github/actions/workflow/status/chmouel/raffi/rust.yaml?branch=main&style=flat-square&label=CI)](https://github.com/chmouel/raffi/actions/workflows/rust.yaml)
[![Crates.io](https://img.shields.io/crates/v/raffi?style=flat-square)](https://crates.io/crates/raffi)
[![AUR](https://img.shields.io/aur/version/raffi-bin?style=flat-square)](https://aur.archlinux.org/packages/raffi-bin)
[![Docs](https://img.shields.io/badge/docs-chmouel.github.io%2Fraffi-2563eb?style=flat-square)](https://chmouel.github.io/raffi/)
[![License](https://img.shields.io/github/license/chmouel/raffi?style=flat-square)](LICENSE)

[Documentation](https://chmouel.github.io/raffi/) · [Recipes](https://chmouel.github.io/raffi/recipes/) · [Releases](https://github.com/chmouel/raffi/releases) · [Issues](https://github.com/chmouel/raffi/issues) · [Screenshots](#screenshots)

![Raffi with the preview pane open](website/src/assets/screenshots/main.png)

Raffi is a fast, keyboard-driven launcher for Wayland written in Rust, inspired by Alfred and Raycast on macOS. Bind it to a shortcut and start typing: with no setup at all, one search box finds your apps, open windows, browser tabs, bookmarks, passwords, files, snippets, and clipboard history, ranking recent picks first and offering fallback actions like web search when nothing matches. You do not need a background daemon to use it. Running the optional [`raffi --server`](https://chmouel.github.io/raffi/reference/cli-options/#cache-management) keeps the window about a third faster to open, warms caches in the background, and records your clipboard history.

Built-in tools cover most daily desktop tasks out of the box. Math, currency and unit conversions, colors, and world clocks answer right in the list as you type. You can browse files with inline previews for code, images, videos, and PDFs; switch windows on Sway, Hyprland, or niri; control audio, media, Bluetooth, and power actions; pull logins from [pass](https://chmouel.github.io/raffi/features/password-store/) or [Bitwarden](https://chmouel.github.io/raffi/features/bitwarden/); and search [Slack](https://chmouel.github.io/raffi/features/slack/). The [clipboard history](https://chmouel.github.io/raffi/features/clipboard-history/) searches text inside screenshots with OCR, expires one-time codes, ignores password managers, and saves any entry as a [text snippet](https://chmouel.github.io/raffi/features/text-snippets/) that pastes straight into your active window with dynamic placeholders like today's date.

When you want to make Raffi yours, everything lives in plain YAML with editor autocomplete via JSON Schema. You can add custom commands, inline scripts, conditional entries that adapt to whichever machine you are on, web searches, and [Alfred-compatible script filters](https://chmouel.github.io/raffi/features/script-filters/) with rich previews and action menus. Bind extra shortcuts to jump straight into any mode (such as `Super+V` for clipboard history), pick from built-in [themes](https://chmouel.github.io/raffi/features/themes/), or use [Fuzzel](https://chmouel.github.io/raffi/integration/fuzzel/) as the frontend.

## Installation

Install from your package manager, Cargo, or [GitHub releases](https://github.com/chmouel/raffi/releases) (see the [installation guide](https://chmouel.github.io/raffi/installation/) to build from source):

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

## Quick Start

Bind `raffi` to a key in your compositor (and optionally bind a second key like `raffi -i "clip "` to open clipboard history directly):

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

Raffi works with no config file. When you are ready to customize it, drop settings in `~/.config/raffi/raffi.yaml` and your own commands in `~/.config/raffi/launchers.yaml`, or copy a ready-made setup from the [recipes](https://chmouel.github.io/raffi/recipes/) and [configuration guide](https://chmouel.github.io/raffi/configuration/overview/).

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
