"""Demo home directory and base config shared by every shot.

Everything here is made up: no real accounts, paths or secrets. The fake
CLIs in fake_cli.py answer the commands the addons run.
"""
import json
import os
import shutil
import sqlite3
import subprocess
import time
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
        conn.execute("CREATE TABLE moz_places "
                     "(id INTEGER PRIMARY KEY, url TEXT, rev_host TEXT, description TEXT, preview_image_url TEXT, "
                     "title TEXT, visit_count INTEGER DEFAULT 0, typed INTEGER DEFAULT 0, "
                     "hidden INTEGER DEFAULT 0, last_visit_date INTEGER DEFAULT 0)")
        conn.execute("INSERT INTO moz_places (url, rev_host, description, preview_image_url) VALUES (?, ?, ?, ?)", (
            "https://raffi.example.com/guide", "moc.elpmaxe.iffar.",
            "Launch apps, browse files, switch tabs, and inspect results without leaving the keyboard. Demo browser data.",
            "https://raffi.example.com/logo.png",
        ))
        _firefox_bookmarks(conn)
        _firefox_history(conn)
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


# (id, type, parent, title, guid, url): type 1 is a bookmark, 2 a folder.
FIREFOX_BOOKMARKS = [
    (1, 2, 0, "", "root________", None),
    (2, 2, 1, "menu", "menu________", None),
    (3, 2, 1, "toolbar", "toolbar_____", None),
    (5, 2, 1, "unfiled", "unfiled_____", None),
    (10, 2, 3, "Work", "work_folder1", None),
    (11, 2, 3, "Reading", "read_folder1", None),
    (12, 1, 3, "Raffi guide", "bm_guide0001", "https://raffi.example.com/guide"),
    (13, 1, 3, "Mail", "bm_mail00001", "https://mail.example.com/inbox"),
    (14, 1, 3, "Calendar", "bm_calendar1", "https://calendar.example.com/week"),
    (15, 1, 10, "Pull requests", "bm_pulls0001", "https://code.example.com/pulls"),
    (16, 1, 10, "Release checklist", "bm_release01", "https://wiki.example.com/release"),
    (17, 1, 11, "Rust standard library", "bm_rust00001", "https://doc.rust-lang.org/std/"),
    (18, 1, 2, "Raffi on GitHub", "bm_github001", "https://github.com/chmouel/raffi"),
    (19, 1, 5, "Sourdough recipe", "bm_bread0001", "https://recipes.example.org/sourdough"),
    (20, 1, 2, "Nix code search", "bm_nixsrch01", "https://github.com/search?q=%22%s%22+path%3A*.nix&type=code"),
]
# (keyword, bookmark id)
FIREFOX_KEYWORDS = [("nixsearch", 20)]


def _firefox_bookmarks(conn):
    conn.execute("CREATE TABLE moz_bookmarks "
                 "(id INTEGER PRIMARY KEY, type INTEGER, fk INTEGER, parent INTEGER, position INTEGER, "
                 "title TEXT, dateAdded INTEGER, guid TEXT)")
    added = 1789000000 * 1_000_000
    for position, (id_, kind, parent, title, guid, url) in enumerate(FIREFOX_BOOKMARKS):
        place = None
        if url:
            place = conn.execute("SELECT id FROM moz_places WHERE url = ?", (url,)).fetchone()
            place = place[0] if place else conn.execute(
                "INSERT INTO moz_places (url) VALUES (?)", (url,)).lastrowid
        conn.execute("INSERT INTO moz_bookmarks VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                     (id_, kind, place, parent, position, title, added + id_ * 86_400_000_000, guid))
    conn.execute("CREATE TABLE moz_keywords "
                 "(id INTEGER PRIMARY KEY, keyword TEXT, place_id INTEGER, post_data TEXT)")
    for keyword, bookmark in FIREFOX_KEYWORDS:
        conn.execute("INSERT INTO moz_keywords (keyword, place_id) "
                     "SELECT ?, fk FROM moz_bookmarks WHERE id = ?", (keyword, bookmark))


def _firefox_history(conn):
    now = int(time.time())
    rows = [
        ("https://news.ycombinator.com/", "Hacker News", 120, 12, now - 30 * 60),
        ("https://raffi.example.com/guide", "Raffi guide", 42, 3, now - 2 * 60 * 60),
        ("https://raffi.example.com/guide/configuration", "Raffi guide", 18, 0, now - 150 * 60),
        ("https://github.com/chmouel/raffi", "Raffi on GitHub", 15, 2, now - 3 * 60 * 60),
        ("https://doc.rust-lang.org/std/", "Rust standard library", 8, 1, now - 26 * 60 * 60),
        ("https://en.wikipedia.org/wiki/Rust_(programming_language)", "Rust (programming language)", 9, 0, now - 2 * 86400),
        ("https://recipes.example.org/sourdough", "Sourdough recipe", 4, 0, now - 5 * 86400),
    ]
    for link, title, visits, typed, visited in rows:
        updated = conn.execute(
            "UPDATE moz_places SET title = ?, visit_count = ?, typed = ?, last_visit_date = ? WHERE url = ?",
            (title, visits, typed, visited * 1_000_000, link),
        ).rowcount
        if not updated:
            conn.execute(
                "INSERT INTO moz_places (url, title, visit_count, typed, last_visit_date) VALUES (?, ?, ?, ?, ?)",
                (link, title, visits, typed, visited * 1_000_000),
            )


def _brave(home: Path):
    root = home / ".config/BraveSoftware/Brave-Browser"
    profile = root / "Default"
    profile.mkdir(parents=True)
    (root / "Local State").write_text(json.dumps({"profile": {"info_cache": {"Default": {"name": "Personal"}}}}))
    _brave_history(profile)

    def url(name, link):
        return {"type": "url", "name": name, "url": link, "date_added": "13400000000000000"}

    bar = {"type": "folder", "name": "Bookmarks bar", "children": [
        {"type": "folder", "name": "News", "children": [url("Example News", "https://news.example.com/")]},
        url("Forum", "https://forum.example.com/latest"),
    ]}
    (profile / "Bookmarks").write_text(json.dumps({"version": 1, "roots": {
        "bookmark_bar": bar,
        "other": {"type": "folder", "name": "Other bookmarks", "children": []},
        "synced": {"type": "folder", "name": "Mobile bookmarks", "children": []},
    }}))


def _brave_history(profile: Path):
    epoch = 11_644_473_600_000_000
    now = int(time.time())
    rows = [
        ("https://github.com/chmouel/raffi", "Raffi on GitHub", 7, 1, now - 20 * 60),
        ("https://forum.example.com/latest", "Forum", 6, 0, now - 60 * 60),
        ("https://news.example.com/", "Example News", 30, 2, now - 6 * 60 * 60),
    ]
    with sqlite3.connect(profile / "History") as conn:
        conn.execute("CREATE TABLE urls (id INTEGER PRIMARY KEY, url TEXT, title TEXT, "
                     "visit_count INTEGER, typed_count INTEGER, last_visit_time INTEGER, hidden INTEGER DEFAULT 0)")
        for index, (link, title, visits, typed, visited) in enumerate(rows, 1):
            conn.execute("INSERT INTO urls VALUES (?, ?, ?, ?, ?, ?, 0)",
                         (index, link, title, visits, typed, epoch + visited * 1_000_000))


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
    _brave(home)
    (home / ".local/share/applications/raffi-demo.desktop").write_text(
        "[Desktop Entry]\nType=Application\nName=Demo browser\nExec=firefox\nIcon=firefox\n"
        "Actions=private;\n\n[Desktop Action private]\nName=Private window\nExec=firefox --private-window\n"
    )
    (home / ".cache/raffi/recent.cache").write_text(
        json.dumps({"source": "text_snippets.Notes", "key": "Release checklist", "last_used": 1792400000}) + "\n"
    )
    slack = home / ".cache/raffi/slack"
    slack.mkdir()
    slack_users = []
    for uid, login, name, title in [
        ("U1", "alice", "Alice Adams", "Engineering"),
        ("U2", "bob", "Bob Bennett", "Design"),
        ("U3", "carol", "Carol Chen", "Product"),
    ]:
        slack_users.append({
            "id": uid, "name": login, "real_name": name, "display_name": login,
            "title": title, "status_text": "", "status_emoji": "",
            "tz_label": "", "tz_offset": 0, "avatar": "",
        })
    (slack / "directory.json").write_text(json.dumps({
        "version": 3, "fetched_at": int(time.time()),
        "workspace": {"team_id": "T1", "user_id": "U0", "url": "https://demo.slack.com/", "name": "Demo"},
        "users": slack_users, "conversations": [],
    }))
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
