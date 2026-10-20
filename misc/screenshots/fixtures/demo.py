"""Demo home directory and base config shared by every shot.

Everything here is made up: no real accounts, paths or secrets. The fake
CLIs in fake_cli.py answer the commands the addons run.
"""
import json
import os
import shutil
import sqlite3
import subprocess
from datetime import datetime
from pathlib import Path
from zoneinfo import ZoneInfo

from PIL import Image, ImageDraw, ImageFont

FIXTURES = Path(__file__).resolve().parent
FAKE_COMMANDS = [
    "pactl", "niri", "playerctl", "bluetoothctl", "rbw", "copyq", "herdr", "gh", "batz",
    "systemctl", "loginctl", "xdg-open", "hyprctl", "swaymsg", "kitty", "wezterm",
    "pass", "gpg", "gpg2", "gpgconf", "notify-send", "wl-copy", "xclip", "xsel",
    "firefox", "code", "nautilus",
]
# File dates shown in previews.
FIXED_MTIME = datetime(2026, 10, 15, 20, 38, tzinfo=ZoneInfo("Europe/Paris")).timestamp()

SNIPPET_FILES = {
    "bug-report.snippet": """Bug report
keyword: !bug
icon: 🐛
---
## Summary
[What failed, and where?]

## Steps to reproduce
1. [First step]
2. [Action that triggers the failure]

## Expected result
[What should have happened?]

## Actual result
[What happened instead?]
""",
    "follow-up-email.snippet": """Follow-up email
keyword: !followup
icon: 📬
---
Hi,

Following up on:

https://raffi.example.com/pull/142

Could you send your review when you have a moment?
""",
    "incident-handover.snippet": """Incident handover
keyword: !handover
icon: 🚨
---
## Handover | {date:%Y-%m-%d} 09:30
Next update: {date:%Y-%m-%d} 10:00

## Current context
[What is affected, and since when?]
""",
}


def link_fake_commands(bin_dir: Path):
    bin_dir.mkdir(parents=True, exist_ok=True)
    (FIXTURES / "fake_cli.py").chmod(0o755)
    for name in FAKE_COMMANDS:
        (bin_dir / name).symlink_to(FIXTURES / "fake_cli.py")


def _font(size):
    path = subprocess.check_output(["fc-match", "-f", "%{file}", "Noto Sans"], text=True)
    return ImageFont.truetype(path, size)


def _documents(home: Path):
    documents = home / "Documents"
    (documents / "Images").mkdir(parents=True)
    (documents / "Archive").mkdir()
    (documents / "preview.py").write_text(
        'from pathlib import Path\n\n\ndef readable_files(directory: Path):\n'
        '    return sorted(\n        path.name\n        for path in directory.iterdir()\n'
        '        if path.is_file()\n    )\n\n\nfor name in readable_files(Path(".")):\n    print(name)\n'
    )
    (documents / "release-notes.json").write_text(json.dumps({
        "project": "Raffi", "release": "upcoming",
        "features": ["Preview pane", "Session trees", "Quick conversions"], "demo_data": True,
    }, indent=2))
    cover = Image.new("RGB", (900, 1100), "#203349")
    draw = ImageDraw.Draw(cover)
    draw.rectangle((60, 60, 840, 1040), outline="#b7cddd", width=2)
    draw.text((110, 150), "Keyboard\nfirst.", font=_font(70), fill="#f1f5fa", spacing=16)
    draw.text((110, 870), "Raffi demo document\nRich PDF preview", font=_font(32), fill="#b7cddd", spacing=18)
    cover.save(documents / "keyboard-first.pdf", "PDF", resolution=100)
    cover.resize((450, 550)).save(documents / "Images/cover.png")


def _project(home: Path, repo: Path):
    project = home / "Projects/raffi"
    for name in ["assets", "docs", "examples", "scripts", "src", "tests", ".github/workflows"]:
        (project / name).mkdir(parents=True)
    shutil.copyfile(repo / "assets/logo.png", project / "assets/logo.png")
    shutil.copyfile(repo / "Cargo.toml", project / "Cargo.toml")
    (project / "README.md").write_text("# Raffi\n\nA keyboard-first launcher for Wayland.\n")
    (project / ".github/workflows/ci.yml").write_text("name: CI\non: [push]\n")
    (project / "examples/raffi.yaml").write_text("version: 1\n")
    (project / "src/main.rs").write_text('fn main() {\n    println!("raffi");\n}\n')


def _firefox(home: Path, repo: Path):
    profile = home / ".mozilla/firefox/demo"
    profile.mkdir(parents=True)
    (profile / "favicons.sqlite").touch()
    with sqlite3.connect(profile / "places.sqlite") as conn:
        conn.execute("CREATE TABLE moz_places (url TEXT, rev_host TEXT, description TEXT, preview_image_url TEXT)")
        conn.execute("INSERT INTO moz_places VALUES (?, ?, ?, ?)", (
            "https://raffi.example.com/guide", "moc.elpmaxe.iffar.",
            "Launch apps, browse files, switch tabs, and inspect results without leaving the keyboard. Demo browser data.",
            "https://raffi.example.com/logo.png",
        ))
    # Raffi caches link previews under an FNV-1a hash of the image URL.
    image_url = "https://raffi.example.com/logo.png"
    key_hash = 0xcbf29ce484222325
    for byte in image_url.encode():
        key_hash = ((key_hash ^ byte) * 0x100000001b3) & ((1 << 64) - 1)
    previews = home / ".cache/raffi/previews"
    previews.mkdir(parents=True, exist_ok=True)
    logo = Image.open(repo / "assets/logo.png").convert("RGBA")
    logo.thumbnail((420, 340))
    logo.save(previews / f"{key_hash:016x}.png")


def _fix_mtimes(*roots: Path):
    for root in roots:
        for path in [root, *root.rglob("*")]:
            os.utime(path, (FIXED_MTIME, FIXED_MTIME), follow_symlinks=False)


def setup(home: Path, repo: Path) -> dict:
    """Populate an empty `home` and return the base config."""
    for path in [".config", ".cache/raffi", ".local/share/applications", "runtime"]:
        (home / path).mkdir(parents=True, exist_ok=True)
    (home / "runtime").chmod(0o700)
    for name in ["Code/code.example.com", "Community/forum.example.com", "Mail/mail.example.com"]:
        path = home / ".password-store" / f"{name}.gpg"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("Not an encrypted secret. Listing-only screenshot fixture.\n")
    _documents(home)
    _project(home, repo)
    snippets = home / ".local/share/snippets"
    snippets.mkdir(parents=True)
    for name, text in SNIPPET_FILES.items():
        (snippets / name).write_text(text)
    _fix_mtimes(home / "Documents", home / "Projects")
    _firefox(home, repo)
    (home / ".local/share/applications/raffi-demo.desktop").write_text(
        "[Desktop Entry]\nType=Application\nName=Demo browser\nExec=firefox\nIcon=firefox\n"
        "Actions=private;\n\n[Desktop Action private]\nName=Private window\nExec=firefox --private-window\n"
    )
    (home / ".cache/raffi/recent.cache").write_text(
        json.dumps({"source": "text_snippets.Notes", "key": "Release checklist", "last_used": 1792400000}) + "\n"
    )
    return base_config(repo)


def base_config(repo: Path) -> dict:
    return {
        "version": 1,
        "general": {
            "theme": "catppuccin", "font_family": "Noto Sans", "font_size": 22,
            "window_width": 1120, "window_height": 560, "show_preview": True, "max_history": 0,
            # Tips are picked at random on each launch; off keeps shots reproducible.
            "show_tips": False,
            "preview": {"font_size": 17, "mono_font_family": "JetBrains Mono", "width_percent": 52},
            "main_view": ["launchers", "text_snippets.Notes", "windows", "audio"],
        },
        "launchers": {
            "Raffi documentation": {"binary": "firefox", "args": ["https://chmouel.github.io/raffi/"], "icon": "firefox", "description": "Raffi documentation"},
            "Terminal": {"binary": "kitty", "icon": "utilities-terminal", "description": "Terminal"},
            "Code editor": {"binary": "code", "icon": "code", "description": "Code editor"},
            "Files": {"binary": "nautilus", "icon": "system-file-manager", "description": "Files"},
        },
        "addons": {
            "currency": {"enabled": False},
            "clipboard": {"enabled": True},
            "pass": {"enabled": True},
            "bitwarden": {"enabled": True},
            "slack": {"enabled": False},
            "browser_tabs": {"favicon_service": "none"},
            "terminal": {"command": ["kitty"], "open_in": "tab"},
            "file_browser": {"action": "copy"},
            "text_snippets": [{"name": "Notes", "keyword": "note", "action": "copy", "snippets": [
                {"name": "Release checklist", "value": "Capture screenshots\nCheck image descriptions\nReview the draft"},
                {"name": "Review request", "value": "Could you review the keyboard navigation and preview pane?"},
            ]}],
            "script_filters": [
                {"name": "Herdr", "keyword": "herdr", "command": "python3", "args": [str(repo / "scripts/herdr/herdr_filter.py")], "action": "copy", "icon": "utilities-terminal"},
                {"name": "Herdr status", "keyword": "agents", "command": "python3", "args": [str(repo / "scripts/herdr/herdr_filter.py"), "--status"], "action": "copy", "icon": "utilities-terminal"},
                {"name": "Pull requests", "keyword": "pr", "command": "python3", "args": [str(repo / "scripts/github/pr.py")], "action": "copy", "icon": "github"},
            ],
        },
    }
