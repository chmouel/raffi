"""Every native screenshot, declared once.

Each Shot launches Raffi with the demo config, types `query` through `-i`,
then plays `keys`. Keys are X keysym names, optionally prefixed with
`ctrl+`, `alt+` or `shift+`, or `type:text` to type literal text.

To add a shot: add an entry here, run `make screenshots SHOTS=<name>`, and
reference website/src/assets/screenshots/<name>.png from the docs.
"""
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import datetime
from zoneinfo import ZoneInfo


@dataclass(frozen=True)
class Shot:
    name: str
    query: str = ""
    keys: tuple = ()
    width: int = 1120
    height: int = 560
    theme: str | None = None
    tweak: Callable[[dict], None] | None = None
    # The 23 shots that predate the scripted batch: smaller window, web
    # search fallbacks and the extra demo sources they show.
    classic: bool = False
    # Window-relative (left, top, right, bottom) to keep instead of the framed window.
    crop: tuple | None = None
    needs: frozenset = field(default_factory=frozenset)
    settle: float = 2.0
    palette: str | None = None


def down(count=1):
    return ("Down",) * count


def addon(name, value):
    def tweak(config):
        config["addons"][name] = value
    return tweak


SOLARIZED_LIGHT = {
    "bg_base": "#fdf6e3", "bg_input": "#eee8d5", "accent": "#268bd2", "accent_hover": "#2aa198",
    "text_main": "#657b83", "text_muted": "#93a1a1", "selection_bg": "#eee8d5", "border": "#93a1a1",
}

CHECK_DISK_SCRIPT = """import shutil
usage = shutil.disk_usage("/")
pct = (usage.used / usage.total) * 100
print(f"Root disk usage: {pct:.1f}% ({usage.free // (2**30)} GiB free)")
"""
CHECK_DISK_LAST_USED = int(datetime(2026, 10, 15, 16, 36, tzinfo=ZoneInfo("Europe/Paris")).timestamp())


def classic(config):
    general = config["general"]
    general.update({"window_width": 960, "window_height": 560, "font_size": 20})
    general["preview"].update({"font_size": 16, "width_percent": 50})
    general["fallbacks"] = [
        'addons.web_searches."Google"',
        'addons.web_searches."DuckDuckGo"',
        'addons.script_filters."Pull requests"',
    ]
    addons = config["addons"]
    addons["web_searches"] = [
        {"name": "Google", "keyword": "g", "url": "https://google.com/search?q={query}", "icon": "google"},
        {"name": "DuckDuckGo", "keyword": "ddg", "url": "https://duckduckgo.com/?q={query}", "icon": "duckduckgo"},
        {"name": "GitHub", "keyword": "gh", "url": "https://github.com/search?q={query}&type=repositories", "icon": "github"},
    ]
    addons["text_snippets"] += [
        {"name": "Emails", "keyword": "em", "icon": "mail", "action": "copy", "snippets": [
            {"name": "Personal email", "icon": "📨", "keyword": "!me", "value": "user@example.com"},
            {"name": "Follow-up reminder", "icon": "📅", "keyword": "!followup",
             "value": "Hi team,\n\nFollowing up on our review scheduled for {date +2d:%A %d %B %Y}.\n"
                      "Reference link: https://raffi.example.com/pull/142\n\nAction items:\n- {cursor}"},
            {"name": "Meeting notes", "icon": "📝", "value": "Notes from {date:%A %d %B}\n\n- {cursor}"},
        ]},
        {"name": "Snippets", "keyword": "sn", "icon": "snippets", "action": "copy",
         "directory": "~/.local/share/snippets"},
    ]
    addons["script_filters"].append({
        "name": "Timezones", "keyword": "tz", "command": "batz", "args": ["-j"], "icon": "clock",
        "min_query_length": 2, "action": "copy",
    })


def check_disk(config):
    config["launchers"]["Check Disk Usage"] = {
        "binary": "python3", "script": CHECK_DISK_SCRIPT, "icon": "python", "description": "Check Disk Usage",
    }


def solarized_light(config):
    config["general"]["theme"] = "light"
    config["general"]["theme_colors"] = SOLARIZED_LIGHT


SHOTS = [
    # Main view and built-in addons, 1120x560.
    Shot("main", theme="dark"),
    Shot("main-recent", height=720),
    Shot("main-search", "raffi"),
    Shot("main-source", "raffi", ("ctrl+Tab",)),
    Shot("unit-conversion", "100 km/h to mph"),
    Shot("color-converter", "#89b4fa"),
    Shot("developer-tools", "dev hello world"),
    Shot("world-clock", "3pm london in tokyo"),
    Shot("system-commands", "sys "),
    Shot("window-switcher", "win "),
    Shot("audio-switcher", "audio outputs", down()),
    Shot("audio-volume", "audio 40"),
    Shot("media-controls", "media "),
    Shot("bluetooth", "bt ", down()),
    Shot("clipboard-history", "clip "),
    Shot("password-store", "pass "),
    Shot("bitwarden", "bw "),
    Shot("terminal", "> cargo test --all-features"),
    Shot("installed-apps", "Demo browser", tweak=addon("apps", {"enabled": True})),
    Shot("file-browser-filter", "~/Documents/", ("ctrl+Tab", "ctrl+s")),
    Shot("theme-nord", theme="nord"),
    Shot("kill-process", "kill {worker}", crop=(550, 110, 1100, 505), needs=frozenset({"worker"})),
    # Taller window for tree and rich previews, 1120x720.
    Shot("herdr-tree", "herdr ", down(), height=720),
    Shot("herdr-status", "agents ", down(), height=720),
    Shot("script-filter-pr", "pr review", down(), height=720),
    Shot("file-browser-rich-preview", "~/Documents/preview.py", height=720),
    Shot("file-browser-pdf", "~/Documents/keyboard-first.pdf", height=720),
    Shot("browser-tabs", "tabs ", height=720, needs=frozenset({"browser"})),
    Shot("bookmarks", "bm Firefox/Bookmarks Toolbar/", down(2), height=720),
    Shot("bookmarks-search", "bm nixsearch mkShell"),
    # Older batch, 960x560 with fallbacks.
    Shot("calculator", "(2 + 3) * 64", classic=True),
    Shot("currency-converter", "$100", classic=True, settle=5,
         tweak=addon("currency", {"enabled": True})),
    Shot("emoji-picker", "emoji rocket", classic=True, settle=8),
    Shot("fallbacks", "wayland compositor", down(), classic=True),
    Shot("web-search", "gh wayland launcher", classic=True),
    Shot("file-browser", "~/Projects/raffi/", classic=True),
    Shot("file-browser-hidden", "~/Projects/raffi/", ("ctrl+h",), classic=True),
    Shot("file-browser-code-preview", "~/Projects/raffi/Cargo.toml", classic=True),
    Shot("file-browser-image-preview", "~/Projects/raffi/assets/logo.png", classic=True),
    Shot("file-browser-image-scrolled", "~/Projects/raffi/assets/logo.png", ("ctrl+j",) * 3, classic=True),
    Shot("keyword-help", "", ("ctrl+slash",), classic=True),
    Shot("keyword-help-shortcuts", "", ("ctrl+slash", "type:preview"), classic=True),
    Shot("keyword-suggestion", "sn", classic=True),
    Shot("launcher-script-preview", "Check Disk", classic=True, tweak=check_disk),
    Shot("script-filter-tz", "tz 10h00 tomorrow", classic=True),
    Shot("text-snippets", "sn ", classic=True),
    Shot("text-snippets-placeholders", "em follow", classic=True),
    Shot("browser-tabs-setup", "tabs ", classic=True),
    Shot("light-theme", "fi", theme="light", classic=True),
    Shot("theme-catppuccin", "sn ", theme="catppuccin", classic=True),
    Shot("theme-gruvbox", "pr ", theme="gruvbox", classic=True),
    Shot("theme-solarized-light", "pr ", classic=True, tweak=solarized_light, palette="daylight-warm"),
    Shot("theme-tokyonight", "emoji grin", theme="tokyonight", classic=True, settle=8),
]

BY_NAME = {shot.name: shot for shot in SHOTS}
assert len(BY_NAME) == len(SHOTS), "duplicate shot names"
