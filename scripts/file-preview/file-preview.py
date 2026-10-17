#!/usr/bin/env python3
"""File previewer for the Raffi file browser.

Usage: file-preview.py <path>

Raffi runs the configured preview command with the selected file or directory
as its last argument and RAFFI_PREVIEW=1 set. It also passes the pane size in
pixels (RAFFI_PREVIEW_WIDTH, RAFFI_PREVIEW_HEIGHT) and in character cells
(RAFFI_PREVIEW_COLUMNS, RAFFI_PREVIEW_LINES).

The script prints a JSON preview object (badge, ANSI text, images, sections
and metadata rows). When it has no preview for a file it prints nothing and
exits with status 1, and Raffi shows its built-in preview instead.

Every external tool (bat, eza, jq, pdftoppm, ffmpeg, ...) is optional. Without
one, the script falls back to the standard library or gives up on that type.
Generated thumbnails and slow previews are cached in
$XDG_CACHE_HOME/raffi/file-preview.

Colours follow the Raffi theme (RAFFI_THEME, RAFFI_THEME_MODE): bat, glow and
gum get a matching theme, and the user's own LS_COLORS and EZA_COLORS are
ignored. Set FILE_PREVIEW_BAT_THEME to force a bat theme.

Debug mode: set DEBUG=1 to log to stderr.
"""

import bz2
import contextlib
import csv
import functools
import gzip
import hashlib
import html
import io
import json
import lzma
import math
import mimetypes
import os
import posixpath
import re
import shutil
import signal
import sqlite3
import stat
import struct
import subprocess
import sys
import tarfile
import time
import traceback
import unicodedata
import urllib.parse
import xml.etree.ElementTree as ET
import zipfile
import zlib
from collections.abc import Callable, Iterable
from dataclasses import dataclass
from pathlib import Path

DEBUG = os.getenv("DEBUG", "").lower() in ("1", "true", "yes")

# Raffi kills the preview command after 3 seconds. Every tool call is clamped
# to what is left of this budget so the script always answers in time.
BUDGET = 2.6
TOOL_TIMEOUT = 2.0
DEADLINE = math.inf

CACHE_DIR = (
    Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache")
    / "raffi"
    / "file-preview"
)
CACHE_MAX_BYTES = 200 * 1024 * 1024
CACHE_MAX_FILES = 2000
CACHE_IMAGE_EXTS = (".jpg", ".png", ".gif", ".webp")

HEAD_BYTES = 512 * 1024
PARSE_LIMIT = 4 * 1024 * 1024
COUNT_LIMIT = 32 * 1024 * 1024
MAX_LINE_CHARS = 1000
HEX_BYTES = 512

IGNORED_DIRS = (".git", "node_modules", "__pycache__")

DIM = "2"
BOLD = "1"
DIR_COLOR = "1;94"
LINK_COLOR = "36"
SIZE_COLOR = "32"
KEY_COLOR = "1;94"
STRING_COLOR = "32"
NUMBER_COLOR = "35"
LITERAL_COLOR = "33"

Preview = dict | str | None


def debug(msg: str) -> None:
    """Print debug message to stderr if DEBUG is enabled."""
    if DEBUG:
        print(f"[DEBUG] {msg}", file=sys.stderr)


def remaining() -> float:
    return DEADLINE - time.monotonic()


def run(
    cmd: list[str],
    timeout: float = TOOL_TIMEOUT,
    input: str | bytes | None = None,
    env: dict[str, str | None] | None = None,
) -> str | None:
    """Run a tool and return its stdout, or None when it is missing or fails.

    `env` is added to the environment; a None value removes that variable.
    """
    if shutil.which(cmd[0]) is None:
        debug(f"{cmd[0]}: not installed")
        return None
    timeout = min(timeout, remaining())
    if timeout < 0.05:
        debug(f"{cmd[0]}: no time left")
        return None
    data = input.encode() if isinstance(input, str) else input
    try:
        proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE if data is not None else subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=merge_env(env) if env else None,
            start_new_session=True,
        )
    except OSError as e:
        debug(f"{cmd[0]}: {e}")
        return None
    try:
        out, _ = proc.communicate(data, timeout=timeout)
    except subprocess.TimeoutExpired:
        # Kill the whole process group: some tools fork helpers.
        with contextlib.suppress(OSError):
            os.killpg(proc.pid, signal.SIGKILL)
        proc.wait()
        debug(f"{cmd[0]}: timed out after {timeout:.2f}s")
        return None
    if proc.returncode != 0:
        debug(f"{cmd[0]}: exit status {proc.returncode}")
        return None
    return out.decode("utf-8", errors="replace")


def merge_env(env: dict[str, str | None]) -> dict[str, str]:
    merged = {**os.environ, **env}
    return {k: v for k, v in merged.items() if v is not None}


@dataclass(frozen=True)
class Pane:
    columns: int = 80
    lines: int = 24
    width: int = 800
    height: int = 600

    @property
    def max_lines(self) -> int:
        return max(self.lines * 4, 200)

    @classmethod
    def from_env(cls) -> "Pane":
        term = shutil.get_terminal_size()
        return cls(
            columns=env_int("RAFFI_PREVIEW_COLUMNS", term.columns),
            lines=env_int("RAFFI_PREVIEW_LINES", term.lines),
            width=env_int("RAFFI_PREVIEW_WIDTH", 800),
            height=env_int("RAFFI_PREVIEW_HEIGHT", 600),
        )


def env_int(name: str, default: int) -> int:
    try:
        value = int(os.environ.get(name, ""))
    except ValueError:
        return default
    return value if value > 0 else default


# --- Theme ------------------------------------------------------------------

# Raffi passes its theme in RAFFI_THEME and RAFFI_THEME_MODE. Inside Raffi the
# tools get a matching theme and the user's own colour settings are dropped, so
# the preview matches the launcher. Basic ANSI colours are drawn with the Raffi
# theme's palette, which is why "ansi" is the fallback bat theme. Run from a
# terminal, without RAFFI_THEME, the user's settings apply as usual.

BAT_THEMES = {
    "catppuccin-mocha": "Catppuccin Mocha",
    "catppuccin-latte": "Catppuccin Latte",
    "nord": "Nord",
    "dracula": "Dracula",
    "gruvbox": "gruvbox-dark",
}
GLAMOUR_STYLES = {"dracula": "dracula", "tokyo-night": "tokyo-night"}
USER_COLOR_ENV = ("EZA_COLORS", "LS_COLORS", "LSCOLORS")
THEME_ENV = ("RAFFI_THEME", "RAFFI_THEME_MODE", "FILE_PREVIEW_BAT_THEME")


def raffi_theme() -> str | None:
    return os.environ.get("RAFFI_THEME") or None


@functools.cache
def bat_themes() -> frozenset[str]:
    return frozenset(split_lines(run(["bat", "--list-themes", "--color=never"]) or ""))


def bat_theme() -> str | None:
    """The bat --theme to use, or None to keep the user's bat config."""
    if forced := os.environ.get("FILE_PREVIEW_BAT_THEME"):
        return forced
    theme = raffi_theme()
    if theme is None:
        return None
    wanted = BAT_THEMES.get(theme, "ansi")
    # Older bat releases lack some themes; Catppuccin arrived in 0.25.
    return wanted if wanted == "ansi" or wanted in bat_themes() else "ansi"


def glamour_style() -> str:
    """Markdown style for glow and gum format."""
    theme = raffi_theme()
    if theme in GLAMOUR_STYLES:
        return GLAMOUR_STYLES[theme]
    return "light" if os.environ.get("RAFFI_THEME_MODE") == "light" else "dark"


def plain_color_env() -> dict[str, str | None]:
    """Inside Raffi, drop the user's ls colours so the theme palette applies."""
    return dict.fromkeys(USER_COLOR_ENV) if raffi_theme() else {}


# --- Text helpers -----------------------------------------------------------


def sgr(text: str, code: str) -> str:
    return f"\x1b[{code}m{text}\x1b[0m" if text else text


ESCAPES = re.compile(
    r"\x1b\[[0-?]*[ -/]*[@-~]"  # CSI
    r"|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)?"  # OSC, e.g. hyperlinks
    r"|\x1b[P^_X][^\x1b]*(?:\x1b\\)?"  # DCS, PM, APC, SOS
    r"|\x1b.?"
    r"|[\x00-\x08\x0b-\x1f\x7f-\x9f]"
)
SGR_SEQUENCE = re.compile(r"\x1b\[[0-9;]*m")


def clean(text: str) -> str:
    """Drop every escape sequence and control character except SGR colours."""
    return ESCAPES.sub(
        lambda m: m.group() if SGR_SEQUENCE.fullmatch(m.group()) else "", text
    )


def visible_width(text: str) -> int:
    if text.isascii():
        return len(text)
    return sum(
        0
        if unicodedata.combining(c)
        else 2
        if unicodedata.east_asian_width(c) in "WF"
        else 1
        for c in text
    )


def fit(text: str, width: int) -> str:
    """Cut text to a display width, ending with an ellipsis when cut."""
    if visible_width(text) <= width:
        return text
    out, used = [], 0
    for c in text:
        w = visible_width(c)
        if used + w > width - 1:
            break
        out.append(c)
        used += w
    return "".join(out) + "…"


def pad(text: str, width: int, right: bool = False) -> str:
    gap = " " * max(width - visible_width(text), 0)
    return gap + text if right else text + gap


def plural(count: int, word: str) -> str:
    return f"{count:,} {word}{'' if count == 1 else 's'}"


def human_size(size: float, short: bool = False) -> str:
    units = (
        ("B", "K", "M", "G", "T", "P")
        if short
        else ("B", "KiB", "MiB", "GiB", "TiB", "PiB")
    )
    sep = "" if short else " "
    for unit in units:
        if size < 1024 or unit == units[-1]:
            if unit == "B":
                return f"{int(size)}{sep}{'' if short else 'B'}".rstrip()
            return f"{size:.1f}{sep}{unit}" if size < 10 else f"{size:.0f}{sep}{unit}"
        size /= 1024
    return str(size)


def clock(seconds: float) -> str:
    seconds = round(seconds)
    hours, rest = divmod(seconds, 3600)
    minutes, secs = divmod(rest, 60)
    return f"{hours}:{minutes:02}:{secs:02}" if hours else f"{minutes}:{secs:02}"


def more_marker(count: int, noun: str = "lines", exact: bool = True) -> str:
    amount = f"{count:,}" if exact else f"~{count:,}"
    return sgr(f"… {amount} more {noun}", DIM)


def cap_lines(lines: list[str], pane: Pane, noun: str = "lines") -> str:
    """Join lines, keeping at most pane.max_lines and marking the rest."""
    shown = lines[: pane.max_lines]
    text = "\n".join(shown)
    if len(lines) > len(shown):
        text += "\n" + more_marker(len(lines) - len(shown), noun)
    return text


def squeeze_blank(lines: Iterable[str]) -> list[str]:
    """Strip trailing spaces, collapse blank runs and trim blank edges."""
    out: list[str] = []
    for line in lines:
        line = line.rstrip()
        if line or (out and out[-1]):
            out.append(line)
    while out and not out[-1]:
        out.pop()
    return out


def read_head(path: Path, limit: int = HEAD_BYTES) -> bytes:
    with open(path, "rb") as f:
        return f.read(limit)


def decode_text(data: bytes, truncated: bool = False) -> str | None:
    """Decode file contents, or return None when they look binary."""
    if data.startswith((b"\xff\xfe", b"\xfe\xff")):
        if truncated:
            data = data[: len(data) // 2 * 2]
        return data.decode("utf-16", errors="replace").removeprefix("\ufeff")
    data = data.removeprefix(b"\xef\xbb\xbf")
    if b"\0" in data[:8192]:
        return None
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError as e:
        # A multi-byte character cut by the read limit is not an error.
        if truncated and e.start >= len(data) - 3:
            with contextlib.suppress(UnicodeDecodeError):
                return data[: e.start].decode("utf-8")
    sample = data[:8192]
    controls = sum(b < 32 and b not in b"\t\n\r\f\x1b" for b in sample)
    if controls > len(sample) // 100:
        return None
    return data.decode("cp1252", errors="replace")


def count_lines(path: Path, size: int) -> tuple[int, bool]:
    """Count lines exactly for reasonable files, estimate for huge ones."""
    if size <= COUNT_LIMIT:
        count, last = 0, b"\n"
        with open(path, "rb") as f:
            while chunk := f.read(1 << 20):
                count += chunk.count(b"\n")
                last = chunk[-1:]
        return count + (last != b"\n"), True
    head = read_head(path)
    return int(size * head.count(b"\n") / max(len(head), 1)), False


def parse_fields(text: str | None, sep: str = ":") -> dict[str, str]:
    """Parse "Key: value" lines, keeping the first value of each key."""
    fields: dict[str, str] = {}
    for line in (text or "").splitlines():
        key, found, value = line.partition(sep)
        if found and key.strip() and key.strip() not in fields:
            fields[key.strip()] = value.strip()
    return fields


def compose(
    badge: str,
    rows: Iterable[tuple[str, str] | list[str]] = (),
    image: str | None = None,
    text: str | None = None,
    blocks: Iterable[tuple[str, str]] = (),
) -> dict:
    """Build a Raffi preview object.

    Text blocks with headings become sections, shown under the image.
    """
    preview: dict = {"badge": badge}
    blocks = [(heading, content) for heading, content in blocks if content.strip()]
    if blocks:
        sections = [{"type": "image", "path": image}] if image else []
        for heading, content in blocks:
            section = {"type": "text", "content": content}
            if heading:
                section["heading"] = heading
            sections.append(section)
        preview["sections"] = sections
    else:
        if image:
            preview["image"] = image
        if text and text.strip():
            preview["text"] = text
    metadata = [[str(key), str(value)] for key, value in rows if value]
    if metadata:
        preview["metadata"] = metadata
    return preview


# --- Thumbnail cache --------------------------------------------------------


def cache_key(path: Path, *parts: object) -> str:
    st = path.stat()
    fields = [os.path.realpath(path), st.st_mtime_ns, st.st_size, *parts]
    raw = "\0".join(map(str, fields))
    return hashlib.sha256(raw.encode("utf-8", "surrogateescape")).hexdigest()


def sniff_image_ext(head: bytes) -> str | None:
    if head.startswith(b"\x89PNG\r\n\x1a\n"):
        return ".png"
    if head.startswith(b"\xff\xd8\xff"):
        return ".jpg"
    if head[:6] in (b"GIF87a", b"GIF89a"):
        return ".gif"
    if head[:4] == b"RIFF" and head[8:12] == b"WEBP":
        return ".webp"
    return None


def touch(path: Path) -> None:
    with contextlib.suppress(OSError):
        os.utime(path)


def prune_cache() -> None:
    """Drop the least recently used files once the cache grows too big."""
    try:
        entries = []
        for entry in os.scandir(CACHE_DIR):
            if entry.is_file(follow_symlinks=False):
                st = entry.stat(follow_symlinks=False)
                entries.append((st.st_mtime, st.st_size, entry.path))
    except OSError:
        return
    total = sum(size for _, size, _ in entries)
    if total <= CACHE_MAX_BYTES and len(entries) <= CACHE_MAX_FILES:
        return
    entries.sort()
    while entries and (
        total > CACHE_MAX_BYTES * 0.8 or len(entries) > CACHE_MAX_FILES * 0.8
    ):
        _, size, name = entries.pop(0)
        with contextlib.suppress(OSError):
            os.unlink(name)
        total -= size
    debug(f"cache pruned to {len(entries)} files")


def cached_image(
    path: Path, pane: Pane, ext: str, make: Callable[[Path], bool]
) -> str | None:
    """Return a cached image generated from path, making it when missing.

    make(out) writes the image to out (whose name ends with ext) and returns
    whether it succeeded. The real format is sniffed afterwards, so a tool
    writing a PNG to a .jpg name still gives a usable file.
    """
    key = cache_key(path, pane.width)
    for candidate in CACHE_IMAGE_EXTS:
        hit = CACHE_DIR / f"{key}{candidate}"
        if hit.is_file():
            touch(hit)
            debug(f"cache hit {hit}")
            return str(hit)
    try:
        CACHE_DIR.mkdir(parents=True, exist_ok=True)
    except OSError as e:
        debug(f"cache: {e}")
        return None
    tmp = CACHE_DIR / f".{key}.{os.getpid()}{ext}"
    try:
        if not make(tmp) or not tmp.is_file():
            return None
        real_ext = sniff_image_ext(read_head(tmp, 16))
        if real_ext is None:
            debug(f"cache: {tmp} is not an image")
            return None
        final = CACHE_DIR / f"{key}{real_ext}"
        os.replace(tmp, final)
    except OSError as e:
        debug(f"cache: {e}")
        return None
    finally:
        with contextlib.suppress(OSError):
            tmp.unlink()
    prune_cache()
    return str(final)


# --- Text and source code ---------------------------------------------------

LANGUAGES = {
    "c": "C", "h": "C", "cc": "C++", "cpp": "C++", "cxx": "C++", "hpp": "C++",
    "cs": "C#", "css": "CSS", "scss": "SCSS", "dart": "Dart", "diff": "Diff",
    "patch": "Diff", "el": "Emacs Lisp", "ex": "Elixir", "exs": "Elixir",
    "erl": "Erlang", "fish": "Fish", "go": "Go", "hs": "Haskell", "ini": "INI",
    "conf": "Config", "cfg": "Config", "java": "Java", "js": "JavaScript",
    "mjs": "JavaScript", "cjs": "JavaScript", "jsx": "JavaScript",
    "kt": "Kotlin", "lua": "Lua", "log": "Log", "nix": "Nix", "php": "PHP",
    "pl": "Perl", "proto": "Protobuf", "py": "Python", "pyi": "Python",
    "r": "R", "rb": "Ruby", "rs": "Rust", "rst": "reST", "sh": "Shell",
    "bash": "Shell", "zsh": "Shell", "sql": "SQL", "swift": "Swift",
    "tex": "LaTeX", "tf": "Terraform", "toml": "TOML", "ts": "TypeScript",
    "tsx": "TypeScript", "txt": "Text", "vim": "Vim", "yaml": "YAML",
    "yml": "YAML", "zig": "Zig", "md": "Markdown", "markdown": "Markdown",
    "html": "HTML", "htm": "HTML", "xml": "XML", "json": "JSON", "csv": "CSV",
}  # fmt: skip
FILE_NAMES = {
    "makefile": "Makefile", "gnumakefile": "Makefile", "dockerfile": "Dockerfile",
    "containerfile": "Dockerfile", "justfile": "Just", "cmakelists.txt": "CMake",
    "pkgbuild": "Shell", "vagrantfile": "Ruby", "gemfile": "Ruby",
    ".bashrc": "Shell", ".bash_profile": "Shell", ".profile": "Shell",
    ".zshrc": "Shell", ".zshenv": "Shell", ".zprofile": "Shell",
    ".gitconfig": "Git config", ".gitignore": "Git ignore",
}  # fmt: skip
INTERPRETERS = {
    "sh": "Shell", "bash": "Shell", "zsh": "Shell", "dash": "Shell",
    "fish": "Fish", "python": "Python", "node": "JavaScript", "ruby": "Ruby",
    "perl": "Perl", "lua": "Lua", "php": "PHP", "deno": "TypeScript",
}  # fmt: skip
SHEBANG = re.compile(r"#!\s*\S*/(?:env\s+(?:-\S+\s+)*)?([A-Za-z]+)")


def language(name: str, text: str = "") -> str:
    lower = name.lower()
    if lower in FILE_NAMES:
        return FILE_NAMES[lower]
    ext = lower.rpartition(".")[2] if "." in lower.lstrip(".") else ""
    if ext in LANGUAGES:
        return LANGUAGES[ext]
    if match := SHEBANG.match(text):
        return INTERPRETERS.get(match.group(1), "Script")
    return "Text"


def highlight(lines: list[str], name: str, pane: Pane, lang: str | None = None) -> str:
    """Syntax highlight lines with bat, or number them when bat is missing."""
    lines = [
        line if len(line) <= MAX_LINE_CHARS else line[:MAX_LINE_CHARS] + "…"
        for line in lines
    ]
    cmd = [
        "bat",
        "--color=always",
        "--paging=never",
        "--style=numbers",
        "--decorations=always",
        "--wrap=character",
        f"--terminal-width={pane.columns}",
        f"--file-name={name}",
    ]
    if theme := bat_theme():
        cmd.append(f"--theme={theme}")
    if lang:
        cmd.append(f"--language={lang}")
    out = run(cmd, input="\n".join(lines) + "\n")
    if out:
        return out.rstrip("\n")
    width = len(str(len(lines)))
    return "\n".join(
        f"{sgr(str(n).rjust(width), DIM)}  {line.expandtabs(4)}"
        for n, line in enumerate(lines, 1)
    )


def split_lines(text: str) -> list[str]:
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    return [line.rstrip("\r") for line in lines]


def text_result(
    text: str,
    name: str,
    pane: Pane,
    *,
    total: int | None = None,
    exact: bool = True,
    badge: str | None = None,
    lang: str | None = None,
    rows: Iterable[tuple[str, str]] = (),
) -> dict:
    lines = split_lines(text)
    total = len(lines) if total is None else total
    shown = lines[: pane.max_lines]
    body = highlight(shown, name, pane, lang)
    if total > len(shown):
        body += "\n" + more_marker(total - len(shown), exact=exact)
    count = f"{total:,}" if exact else f"~{total:,}"
    badge = badge or language(name, text)
    return compose(badge, [*rows, ("Lines", count)], text=body)


def read_text_file(path: Path) -> tuple[str, bool] | None:
    """Read the start of a file as text: (text, whether it was cut)."""
    data = read_head(path)
    truncated = path.stat().st_size > len(data)
    text = decode_text(data, truncated)
    if text is None:
        return None
    if truncated and "\n" in text:
        text = text[: text.rindex("\n") + 1]
    return text, truncated


def text_preview(path: Path, pane: Pane) -> Preview:
    read = read_text_file(path)
    if read is None:
        return binary_preview(path, pane)
    total, exact = count_lines(path, path.stat().st_size)
    return text_result(read[0], path.name, pane, total=total, exact=exact)


# --- JSON -------------------------------------------------------------------

JSON_TOKEN = re.compile(
    r'("(?:[^"\\]|\\.)*")(\s*:)?'
    r"|(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)"
    r"|\b(true|false|null)\b"
)
# null:false:true:numbers:strings:arrays:objects:object keys
JQ_COLORS = "0;33:0;33:0;33:0;35:0;32:1;39:1;39:1;94"


def colorize_json_line(line: str) -> str:
    def paint(m: re.Match) -> str:
        if m.group(1):
            color = KEY_COLOR if m.group(2) else STRING_COLOR
            return sgr(m.group(1), color) + (m.group(2) or "")
        if m.group(3):
            return sgr(m.group(3), NUMBER_COLOR)
        return sgr(m.group(4), LITERAL_COLOR)

    return JSON_TOKEN.sub(paint, line)


def json_summary(values: list, lines_mode: bool) -> list[tuple[str, str]]:
    if lines_mode:
        return [("Records", f"{len(values):,}")]
    value = values[0]
    rows = []
    if isinstance(value, dict):
        rows.append(("Structure", f"Object, {plural(len(value), 'key')}"))
        if value.get("type") == "FeatureCollection" and isinstance(
            value.get("features"), list
        ):
            rows.append(("Features", f"{len(value['features']):,}"))
    elif isinstance(value, list):
        rows.append(("Structure", f"Array, {plural(len(value), 'item')}"))
    return rows


def render_json(text: str, name: str, pane: Pane, lines_mode: bool = False) -> dict:
    """Pretty print JSON (or JSON Lines), or show it raw when it is invalid."""
    try:
        if lines_mode:
            values = [json.loads(line) for line in text.splitlines() if line.strip()]
        else:
            values = [json.loads(text)]
    except ValueError as e:
        debug(f"invalid JSON: {e}")
        return text_result(text, name, pane, badge="JSON")
    badge = "JSONL" if lines_mode else "JSON"
    rows = json_summary(values, lines_mode)

    pretty = run(["jq", "-C", "."], input=text, env={"JQ_COLORS": JQ_COLORS})
    if pretty:
        return compose(badge, rows, text=cap_lines(split_lines(pretty), pane))
    dumped = "\n".join(json.dumps(v, indent=2, ensure_ascii=False) for v in values)
    lines = dumped.split("\n")
    shown = lines[: pane.max_lines]
    body = None
    if shutil.which("bat"):
        body = highlight(shown, "data.json", pane, "json")
    if body is None or not body.strip():
        body = "\n".join(colorize_json_line(line) for line in shown)
    if len(lines) > len(shown):
        body += "\n" + more_marker(len(lines) - len(shown))
    return compose(badge, rows, text=body)


def json_preview(path: Path, pane: Pane) -> Preview:
    if path.stat().st_size > PARSE_LIMIT:
        return text_preview(path, pane)
    text = decode_text(path.read_bytes())
    if text is None:
        return binary_preview(path, pane)
    lines_mode = path.suffix.lower() in (".jsonl", ".ndjson")
    return render_json(text, path.name, pane, lines_mode)


# --- Tables (CSV, TSV, spreadsheets) ----------------------------------------

NUMBER = re.compile(r"[-+]?[\d,]*\.?\d+(?:[eE][-+]?\d+)?%?")
SEPARATOR = " │ "


def render_table(rows: list[list[str]], pane: Pane) -> tuple[str, int]:
    """Align rows in columns fitting the pane; first row is the header.

    Returns the text and how many columns are shown.
    """
    if not rows:
        return "", 0
    ncols = max(len(row) for row in rows)
    rows = [
        [" ".join(cell.split()) for cell in row] + [""] * (ncols - len(row))
        for row in rows
    ]
    widths = [min(max(visible_width(row[i]) for row in rows), 40) for i in range(ncols)]
    numeric = [
        any(row[i] for row in rows[1:])
        and all(NUMBER.fullmatch(row[i]) for row in rows[1:] if row[i])
        for i in range(ncols)
    ]

    def total(ws: list[int]) -> int:
        return sum(ws) + len(SEPARATOR) * (len(ws) - 1)

    while total(widths) > pane.columns:
        widest = max(range(len(widths)), key=widths.__getitem__)
        if widths[widest] <= 8:
            break
        widths[widest] -= 1
    shown = len(widths)
    while shown > 1 and total(widths[:shown]) + 2 > pane.columns:
        shown -= 1
    widths = widths[:shown]

    separator = sgr(SEPARATOR, DIM)
    lines = []
    for index, row in enumerate(rows):
        cells = []
        for i in range(shown):
            cell, right = fit(row[i], widths[i]), numeric[i] and index > 0
            # No trailing padding unless a column marker follows.
            if right or i < shown - 1 or shown < ncols:
                cell = pad(cell, widths[i], right)
            cells.append(sgr(cell, BOLD) if index == 0 else cell)
        line = separator.join(cells)
        if shown < ncols:
            line = line + sgr(" …", DIM)
        lines.append(line)
        if index == 0:
            lines.append(sgr("─┼─".join("─" * w for w in widths), DIM))
    return "\n".join(lines), shown


def table_rows(columns: int, shown: int, total_rows: str) -> list[tuple[str, str]]:
    count = f"{columns:,}" if shown == columns else f"{columns:,} ({shown} shown)"
    return [("Rows", total_rows), ("Columns", count)]


def csv_preview(path: Path, pane: Pane) -> Preview:
    read = read_text_file(path)
    if read is None:
        return binary_preview(path, pane)
    text, _ = read
    tsv = path.suffix.lower() == ".tsv"
    try:
        dialect = csv.excel_tab if tsv else csv.Sniffer().sniff(text[:8192], ",;\t|")
    except csv.Error:
        dialect = csv.excel
    try:
        reader = csv.reader(io.StringIO(text), dialect)
        rows = [row for _, row in zip(range(pane.max_lines + 1), reader) if row]
    except csv.Error as e:
        debug(f"csv: {e}")
        return text_preview(path, pane)
    if not rows:
        return None
    body, shown = render_table(rows, pane)
    total, exact = count_lines(path, path.stat().st_size)
    data_rows = max(total - 1, 0)
    if data_rows > len(rows) - 1:
        body += "\n" + more_marker(data_rows - (len(rows) - 1), "rows", exact)
    delimiter = {",": "Comma", ";": "Semicolon", "\t": "Tab", "|": "Pipe"}
    ncols = max(len(row) for row in rows)
    return compose(
        "TSV" if tsv else "CSV",
        [
            *table_rows(ncols, shown, f"{data_rows:,}" if exact else f"~{data_rows:,}"),
            ("Delimiter", delimiter.get(dialect.delimiter, dialect.delimiter)),
        ],
        text=body,
    )


# --- Markup ------------------------------------------------------------------


def markdown_preview(path: Path, pane: Pane) -> Preview:
    read = read_text_file(path)
    if read is None:
        return None
    text, _ = read
    rendered = run(
        ["glow", "-s", glamour_style(), "-w", str(pane.columns), "-"],
        input=text,
        env={"CLICOLOR_FORCE": "1"},
    ) or run(
        ["gum", "format", "--type=markdown", f"--theme={glamour_style()}"],
        input=text,
        env={"CLICOLOR_FORCE": "1"},
    )
    if rendered and rendered.strip():
        lines = squeeze_blank(split_lines(rendered))
        return compose("Markdown", text=cap_lines(lines, pane))
    return text_result(text, path.name, pane, badge="Markdown")


TITLE = re.compile(rb"<title[^>]*>(.*?)</title>", re.IGNORECASE | re.DOTALL)


def html_preview(path: Path, pane: Pane) -> Preview:
    head = read_head(path, 64 * 1024)
    rows = []
    if match := TITLE.search(head):
        title = " ".join(
            html.unescape(match.group(1).decode("utf-8", "replace")).split()
        )
        rows.append(("Title", title))
    dump = run(
        [
            "w3m",
            "-dump",
            "-cols",
            str(pane.columns),
            "-T",
            "text/html",
            "-O",
            "UTF-8",
            str(path),
        ]
    ) or run(
        [
            "lynx",
            "-dump",
            "-nolist",
            f"-width={pane.columns}",
            "-display_charset=UTF-8",
            str(path),
        ]
    )
    if dump is None and shutil.which("html2text"):
        dump = run(["html2text", "--body-width", str(pane.columns), str(path)])
    if dump and dump.strip():
        return compose(
            "HTML", rows, text=cap_lines(squeeze_blank(split_lines(dump)), pane)
        )
    preview = text_preview(path, pane)
    if isinstance(preview, dict):
        preview["badge"] = "HTML"
    return preview


ROOT_ELEMENT = re.compile(r"^<([A-Za-z_][\w:.-]*)", re.MULTILINE)


def xml_preview(path: Path, pane: Pane) -> Preview:
    formatted = None
    if path.stat().st_size <= PARSE_LIMIT:
        formatted = run(
            ["xmllint", "--format", "--nonet", "--encode", "UTF-8", str(path)]
        )
    if not formatted:
        preview = text_preview(path, pane)
        if isinstance(preview, dict):
            preview["badge"] = "XML"
        return preview
    rows = []
    if match := ROOT_ELEMENT.search(formatted):
        rows.append(("Root", f"<{match.group(1)}>"))
    return text_result(formatted, path.name, pane, badge="XML", lang="xml", rows=rows)


# --- Images -----------------------------------------------------------------

NATIVE_IMAGES = {"png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "svg"}
CONVERTED_IMAGES = {
    "heic", "heif", "avif", "jxl", "tif", "tiff", "psd", "xcf", "tga", "ppm",
    "pgm", "pbm", "pnm", "exr", "dds", "cr2", "cr3", "nef", "arw", "dng",
    "orf", "rw2", "raf", "srw", "pef",
}  # fmt: skip
PHOTO_FORMATS = {
    "heic", "heif", "avif", "jxl", "cr2", "cr3", "nef", "arw", "dng", "orf",
    "rw2", "raf", "srw", "pef",
}  # fmt: skip
PNG_COLORS = {0: "Gray", 2: "RGB", 3: "Indexed", 4: "Gray + alpha", 6: "RGBA"}
EXIF_TAGS = {
    0x010F: "Make", 0x0110: "Model", 0x0112: "Orientation", 0x0132: "DateTime",
    0x829A: "ExposureTime", 0x829D: "FNumber", 0x8769: "ExifIFD",
    0x8827: "ISO", 0x9003: "DateTimeOriginal", 0x920A: "FocalLength",
    0xA434: "LensModel",
}  # fmt: skip
SVG_TAG = re.compile(r"<svg\b[^>]*>", re.DOTALL)
SVG_SIZE = re.compile(
    r"""(?<![-\w])(width|height)\s*=\s*["']\s*([\d.]+)\s*(?:px)?\s*["']"""
)
SVG_VIEWBOX = re.compile(
    r"""viewBox\s*=\s*["']\s*[-\d.]+[\s,]+[-\d.]+[\s,]+([\d.]+)[\s,]+([\d.]+)"""
)


def parse_exif(tiff: bytes) -> dict:
    """Read the few EXIF tags worth showing from a TIFF structure."""
    endian = {b"II": "<", b"MM": ">"}.get(tiff[:2])
    if endian is None:
        return {}
    out: dict = {}

    def read_ifd(offset: int) -> None:
        (count,) = struct.unpack_from(endian + "H", tiff, offset)
        for i in range(min(count, 512)):
            entry = offset + 2 + 12 * i
            tag, kind, n = struct.unpack_from(endian + "HHI", tiff, entry)
            if tag not in EXIF_TAGS:
                continue
            size = {2: 1, 3: 2, 4: 4, 5: 8, 10: 8}.get(kind, 0) * n
            start = entry + 8
            if size > 4:
                (start,) = struct.unpack_from(endian + "I", tiff, entry + 8)
            raw = tiff[start : start + size]
            if kind == 2:
                value = raw.split(b"\0")[0].decode("utf-8", "replace").strip()
            elif kind == 3:
                (value,) = struct.unpack_from(endian + "H", raw)
            elif kind == 4:
                (value,) = struct.unpack_from(endian + "I", raw)
            elif kind in (5, 10):
                num, den = struct.unpack_from(
                    endian + ("II" if kind == 5 else "ii"), raw
                )
                value = num / den if den else 0
            else:
                continue
            out[EXIF_TAGS[tag]] = value

    with contextlib.suppress(struct.error, IndexError, ValueError):
        read_ifd(struct.unpack_from(endian + "I", tiff, 4)[0])
        if isinstance(out.get("ExifIFD"), int):
            read_ifd(out.pop("ExifIFD"))
    out.pop("ExifIFD", None)
    return out


def jpeg_info(f: io.BufferedReader) -> dict:
    info: dict = {"format": "JPEG"}
    f.seek(2)
    while True:
        byte = f.read(1)
        while byte == b"\xff":
            byte = f.read(1)
        if not byte:
            return info
        marker = byte[0]
        if marker in (0x01, 0xD8) or 0xD0 <= marker <= 0xD7:
            continue
        length_bytes = f.read(2)
        if len(length_bytes) < 2 or marker == 0xDA:
            return info
        (length,) = struct.unpack(">H", length_bytes)
        if marker == 0xE1 and "exif" not in info:
            segment = f.read(length - 2)
            if segment.startswith(b"Exif\0\0"):
                info["exif"] = parse_exif(segment[6:])
            continue
        if 0xC0 <= marker <= 0xCF and marker not in (0xC4, 0xC8, 0xCC):
            _, height, width = struct.unpack(">BHH", f.read(5))
            info["width"], info["height"] = width, height
            return info
        f.seek(length - 2, os.SEEK_CUR)


def image_info(path: Path) -> dict:
    """Read format and dimensions from the image header, without tools."""
    try:
        with open(path, "rb") as f:
            head = f.read(64)
            if head.startswith(b"\xff\xd8"):
                return jpeg_info(f)
            if path.suffix.lower() == ".svg" or b"<svg" in head or b"<?xml" in head:
                f.seek(0)
                return svg_info(f.read(16384).decode("utf-8", "replace"))
    except (OSError, struct.error) as e:
        debug(f"image header: {e}")
        return {}
    try:
        if head.startswith(b"\x89PNG\r\n\x1a\n") and head[12:16] == b"IHDR":
            width, height, depth, color = struct.unpack(">IIBB", head[16:26])
            mode = PNG_COLORS.get(color, "")
            return {
                "format": "PNG",
                "width": width,
                "height": height,
                "detail": f"{mode}, {depth}-bit",
            }
        if head[:6] in (b"GIF87a", b"GIF89a"):
            width, height = struct.unpack("<HH", head[6:10])
            return {"format": "GIF", "width": width, "height": height}
        if head[:4] == b"RIFF" and head[8:12] == b"WEBP":
            chunk = head[12:16]
            if chunk == b"VP8 ":
                width, height = struct.unpack("<HH", head[26:30])
                return {
                    "format": "WebP",
                    "width": width & 0x3FFF,
                    "height": height & 0x3FFF,
                }
            if chunk == b"VP8L":
                (bits,) = struct.unpack("<I", head[21:25])
                return {
                    "format": "WebP",
                    "width": (bits & 0x3FFF) + 1,
                    "height": ((bits >> 14) & 0x3FFF) + 1,
                }
            if chunk == b"VP8X":
                width = int.from_bytes(head[24:27], "little") + 1
                height = int.from_bytes(head[27:30], "little") + 1
                return {"format": "WebP", "width": width, "height": height}
        if head[:2] == b"BM":
            width, height = struct.unpack("<ii", head[18:26])
            return {"format": "BMP", "width": width, "height": abs(height)}
        if head[:4] == b"\0\0\1\0":
            (count,) = struct.unpack("<H", head[4:6])
            with open(path, "rb") as f:
                entries = f.read(6 + 16 * count)[6:]
            sizes = [
                (entries[i] or 256, entries[i + 1] or 256)
                for i in range(0, len(entries) - 1, 16)
            ]
            if sizes:
                width, height = max(sizes)
                return {
                    "format": "ICO",
                    "width": width,
                    "height": height,
                    "detail": plural(len(sizes), "size"),
                }
    except (OSError, struct.error, IndexError) as e:
        debug(f"image header: {e}")
    return {}


def svg_info(text: str) -> dict:
    info: dict = {"format": "SVG"}
    tag = SVG_TAG.search(text)
    if not tag:
        return info
    sizes = {name: float(value) for name, value in SVG_SIZE.findall(tag.group())}
    if ("width" not in sizes or "height" not in sizes) and (
        box := SVG_VIEWBOX.search(tag.group())
    ):
        sizes = {"width": float(box.group(1)), "height": float(box.group(2))}
    if "width" in sizes and "height" in sizes:
        info["width"], info["height"] = round(sizes["width"]), round(sizes["height"])
    return info


def tool_image_info(path: Path) -> dict:
    out = run(
        ["magick", "identify", "-ping", "-format", "%m %w %h\n", f"{path}[0]"]
    ) or run(["identify", "-ping", "-format", "%m %w %h\n", f"{path}[0]"])
    parts = (out or "").split()
    if len(parts) >= 3 and parts[1].isdigit() and parts[2].isdigit():
        return {"format": parts[0], "width": int(parts[1]), "height": int(parts[2])}
    return {}


def exiftool_info(path: Path) -> dict:
    tags = [
        "Make",
        "Model",
        "LensModel",
        "DateTimeOriginal",
        "FNumber",
        "ExposureTime",
        "ISO",
        "FocalLength",
        "Orientation",
    ]
    out = run(["exiftool", "-j", "-n", *(f"-{tag}" for tag in tags), str(path)])
    try:
        data = json.loads(out or "[]")[0]
    except (ValueError, IndexError, KeyError):
        return {}
    return {key: data[key] for key in tags if key in data}


def exif_rows(exif: dict) -> list[tuple[str, str]]:
    rows = []
    make, model = str(exif.get("Make", "")).strip(), str(exif.get("Model", "")).strip()
    if make and model.lower().startswith(make.split()[0].lower()):
        make = ""
    rows.append(("Camera", " ".join(filter(None, [make, model]))))
    rows.append(("Lens", str(exif.get("LensModel", "")).strip()))
    taken = str(exif.get("DateTimeOriginal") or exif.get("DateTime") or "")
    if re.match(r"\d{4}:\d\d:\d\d", taken):
        taken = taken[:10].replace(":", "-") + taken[10:]
    rows.append(("Taken", taken))
    exposure = []
    with contextlib.suppress(TypeError, ValueError, ZeroDivisionError):
        if exif.get("FNumber"):
            exposure.append(f"f/{float(exif['FNumber']):g}")
        if exif.get("ExposureTime"):
            seconds = float(exif["ExposureTime"])
            exposure.append(
                f"1/{round(1 / seconds)} s" if seconds < 1 else f"{seconds:g} s"
            )
        if exif.get("ISO"):
            exposure.append(f"ISO {int(exif['ISO'])}")
        if exif.get("FocalLength"):
            exposure.append(f"{float(exif['FocalLength']):g} mm")
    rows.append(("Exposure", " · ".join(exposure)))
    return rows


def dimension_rows(info: dict) -> list[tuple[str, str]]:
    width, height = info.get("width"), info.get("height")
    if width and height and int(info.get("exif", {}).get("Orientation", 1) or 1) >= 5:
        width, height = height, width
    fmt = ", ".join(filter(None, [info.get("format", ""), info.get("detail", "")]))
    return [
        ("Dimensions", f"{width} × {height}" if width and height else ""),
        ("Format", fmt),
    ]


def image_preview(path: Path, pane: Pane) -> Preview:
    ext = path.suffix.lower().lstrip(".")
    if ext not in NATIVE_IMAGES:
        return converted_image_preview(path, pane)
    info = image_info(path) or tool_image_info(path)
    rows = dimension_rows(info) + exif_rows(info.get("exif", {}))
    return compose("Image", rows, image=str(path))


def converted_image_preview(path: Path, pane: Pane) -> Preview:
    """Convert formats Raffi cannot draw (HEIC, RAW, PSD, ...) to a thumbnail."""
    ext = path.suffix.lower().lstrip(".")
    out_ext = ".jpg" if ext in PHOTO_FORMATS else ".png"
    size = f"{pane.width}x{pane.height}>"

    def make(out: Path) -> bool:
        target = f"{out}[Q=85,strip]" if out_ext == ".jpg" else str(out)
        if (
            run(["vipsthumbnail", str(path), "-s", size, "-o", target], timeout=2.5)
            is not None
            and out.is_file()
        ):
            return True
        for tool in (["magick"], ["convert"]):
            cmd = [
                *tool,
                f"{path}[0]",
                "-auto-orient",
                "-thumbnail",
                size,
                "-quality",
                "85",
                str(out),
            ]
            if run(cmd, timeout=2.5) is not None and out.is_file():
                return True
        return False

    image = cached_image(path, pane, out_ext, make)
    if image is None:
        return None
    info = tool_image_info(path) if remaining() > 0.5 else {}
    exif = exiftool_info(path) if ext in PHOTO_FORMATS and remaining() > 0.5 else {}
    rows = dimension_rows({**info, "format": ext.upper(), "exif": exif}) + exif_rows(
        exif
    )
    return compose("Image", rows, image=image)


# --- PDF --------------------------------------------------------------------

PAGE_SIZE = re.compile(r"([\d.]+) x ([\d.]+) pts(?: \(([^)]+)\))?")


def pdf_preview(path: Path, pane: Pane) -> Preview:
    info = parse_fields(run(["pdfinfo", "-enc", "UTF-8", str(path)]))

    def make(out: Path) -> bool:
        cmd = [
            "pdftoppm", "-f", "1", "-l", "1", "-singlefile",
            "-scale-to-x", str(pane.width), "-scale-to-y", "-1",
            "-jpeg", "-jpegopt", "quality=85", str(path), str(out.with_suffix("")),
        ]  # fmt: skip
        return run(cmd, timeout=2.5) is not None

    image = cached_image(path, pane, ".jpg", make)
    text = run(["pdftotext", "-l", "1", "-enc", "UTF-8", str(path), "-"], timeout=1.0)
    page_text = "\n".join(squeeze_blank(split_lines(text or ""))[:40])
    if not (image or page_text or info):
        return None

    size = ""
    if match := PAGE_SIZE.match(info.get("Page size", "")):
        width, height = (round(float(v) * 25.4 / 72) for v in match.group(1, 2))
        size = f"{width} × {height} mm" + (
            f" ({match.group(3)})" if match.group(3) else ""
        )
    encrypted = info.get("Encrypted", "")
    rows = [
        ("Pages", info.get("Pages", "")),
        ("Title", info.get("Title", "")),
        ("Author", info.get("Author", "")),
        ("Creator", info.get("Creator") or info.get("Producer", "")),
        ("Page size", size),
        ("Version", info.get("PDF version", "")),
        ("Encrypted", "yes" if encrypted.startswith("yes") else ""),
    ]
    return compose(
        "PDF",
        rows,
        image=image,
        blocks=[("Page 1 text", page_text)] if image else [],
        text=page_text,
    )


# --- Video and audio --------------------------------------------------------

VIDEO_EXTS = {
    "mp4", "m4v", "mkv", "webm", "mov", "avi", "wmv", "flv", "mpg", "mpeg",
    "m2ts", "mts", "3gp", "ogv", "vob",
}  # fmt: skip
AUDIO_EXTS = {
    "mp3", "flac", "ogg", "oga", "opus", "m4a", "m4b", "wav", "aac", "wma",
    "aiff", "aif", "ape", "wv", "mka", "alac",
}  # fmt: skip
CODECS = {
    "h264": "H.264", "hevc": "HEVC", "av1": "AV1", "vp9": "VP9", "vp8": "VP8",
    "mpeg4": "MPEG-4", "mpeg2video": "MPEG-2", "mpeg1video": "MPEG-1",
    "prores": "ProRes", "theora": "Theora", "aac": "AAC", "mp3": "MP3",
    "opus": "Opus", "vorbis": "Vorbis", "flac": "FLAC", "ac3": "AC-3",
    "eac3": "E-AC-3", "dts": "DTS", "truehd": "TrueHD", "alac": "ALAC",
    "wmav2": "WMA", "subrip": "SRT", "ass": "ASS", "mov_text": "Text",
    "hdmv_pgs_subtitle": "PGS", "webvtt": "WebVTT",
}  # fmt: skip
CHANNELS = {1: "mono", 2: "stereo", 6: "5.1", 8: "7.1"}


def probe(path: Path) -> dict:
    out = run(
        [
            "ffprobe",
            "-v",
            "error",
            "-show_format",
            "-show_streams",
            "-of",
            "json",
            str(path),
        ]
    )
    try:
        data = json.loads(out or "{}")
    except ValueError:
        return {}
    return data if isinstance(data, dict) else {}


def streams(info: dict, kind: str) -> list[dict]:
    return [s for s in info.get("streams", []) if s.get("codec_type") == kind]


def codec_name(stream: dict) -> str:
    name = stream.get("codec_name", "")
    if name.startswith("pcm_"):
        return "PCM"
    return CODECS.get(name, name.upper())


def tags(info: dict) -> dict[str, str]:
    """Format and stream tags merged, keys lowercased."""
    merged: dict[str, str] = {}
    for source in [info.get("format", {}), *info.get("streams", [])]:
        for key, value in (source.get("tags") or {}).items():
            merged.setdefault(key.lower(), str(value).strip())
    return merged


def bitrate(value: str | int | None) -> str:
    with contextlib.suppress(TypeError, ValueError):
        bps = int(value)
        return f"{bps / 1e6:.1f} Mb/s" if bps >= 1e6 else f"{bps // 1000} kb/s"
    return ""


def duration(info: dict) -> float:
    with contextlib.suppress(TypeError, ValueError):
        return float(info.get("format", {}).get("duration"))
    return 0.0


def frame_rate(value: str) -> str:
    with contextlib.suppress(ValueError, ZeroDivisionError):
        num, _, den = value.partition("/")
        fps = float(num) / float(den or 1)
        if fps > 0:
            return f"{fps:.2f}".rstrip("0").rstrip(".") + " fps"
    return ""


def audio_label(stream: dict) -> str:
    channels = stream.get("channels")
    parts = [
        codec_name(stream),
        CHANNELS.get(channels, f"{channels} ch" if channels else ""),
    ]
    with contextlib.suppress(TypeError, ValueError):
        parts.append(f"{int(stream['sample_rate']) / 1000:g} kHz")
    lang = (stream.get("tags") or {}).get("language", "")
    label = " · ".join(filter(None, parts))
    return f"{label} ({lang})" if lang and lang != "und" else label


def video_rows(info: dict) -> list[tuple[str, str]]:
    video = next(
        (
            s
            for s in streams(info, "video")
            if not s.get("disposition", {}).get("attached_pic")
        ),
        {},
    )
    audio = streams(info, "audio")
    subtitles = streams(info, "subtitle")
    hdr = {"smpte2084": "HDR10", "arib-std-b67": "HLG"}.get(
        video.get("color_transfer", ""), ""
    )
    video_label = (
        " · ".join(
            filter(
                None,
                [
                    " ".join(
                        filter(None, [codec_name(video), video.get("profile", "")])
                    ),
                    frame_rate(video.get("avg_frame_rate", "")),
                    hdr,
                ],
            )
        )
        if video
        else ""
    )
    audio_text = ", ".join(audio_label(s) for s in audio[:3]) + (
        f" (+{len(audio) - 3})" if len(audio) > 3 else ""
    )
    langs = [(s.get("tags") or {}).get("language", "") for s in subtitles]
    subs = ", ".join(lang for lang in langs[:5] if lang) or (
        str(len(subtitles)) if subtitles else ""
    )
    seconds = duration(info)
    return [
        ("Title", tags(info).get("title", "")),
        ("Duration", clock(seconds) if seconds else ""),
        (
            "Resolution",
            f"{video['width']} × {video['height']}" if video.get("width") else "",
        ),
        ("Video", video_label),
        ("Audio", audio_text),
        ("Subtitles", subs),
        ("Bitrate", bitrate(info.get("format", {}).get("bit_rate"))),
    ]


def video_preview(path: Path, pane: Pane) -> Preview:
    info = probe(path)
    seconds = duration(info)

    def make(out: Path) -> bool:
        cmd = [
            "ffmpegthumbnailer",
            "-i",
            str(path),
            "-o",
            str(out),
            "-s",
            str(pane.width),
            "-t",
            "10%",
            "-q",
            "8",
            "-c",
            "jpeg",
        ]
        if run(cmd, timeout=2.0) is not None and out.is_file() and out.stat().st_size:
            return True
        seek = seconds * 0.1 if seconds >= 2 else 0
        cmd = [
            "ffmpeg", "-nostdin", "-v", "error", "-ss", f"{seek:.2f}", "-i", str(path),
            "-frames:v", "1", "-vf", f"scale='min({pane.width},iw)':-2", "-q:v", "3", "-y", str(out),
        ]  # fmt: skip
        return run(cmd, timeout=2.5) is not None

    image = cached_image(path, pane, ".jpg", make)
    rows = video_rows(info) if info else []
    if image or any(value for _, value in rows):
        return compose("Video", rows, image=image)
    text = run(["mediainfo", str(path)])
    return compose("Video", text=text) if text else None


def audio_preview(path: Path, pane: Pane) -> Preview:
    info = probe(path)
    if not info:
        text = run(["mediainfo", str(path)])
        return compose("Audio", text=text) if text else None
    meta = tags(info)
    stream = (streams(info, "audio") or [{}])[0]
    depth = stream.get("bits_per_raw_sample") or stream.get("bits_per_sample")
    fmt = audio_label(stream) + (
        f" · {depth}-bit" if depth and str(depth) != "0" else ""
    )
    seconds = duration(info)
    rows = [
        ("Title", meta.get("title", "")),
        ("Artist", meta.get("artist") or meta.get("album_artist", "")),
        ("Album", meta.get("album", "")),
        ("Track", meta.get("track", "")),
        ("Year", (meta.get("date") or meta.get("year", ""))[:4]),
        ("Genre", meta.get("genre", "")),
        ("Duration", clock(seconds) if seconds else ""),
        ("Format", fmt),
        ("Bitrate", bitrate(info.get("format", {}).get("bit_rate"))),
    ]
    cover = None
    if streams(info, "video"):
        cmd = [
            "ffmpeg", "-nostdin", "-v", "error", "-i", str(path), "-map", "0:v:0", "-frames:v", "1",
            "-vf", f"scale='min({pane.width},iw)':-2", "-q:v", "3", "-y",
        ]  # fmt: skip
        cover = cached_image(
            path, pane, ".jpg", lambda out: run([*cmd, str(out)]) is not None
        )
    return compose("Audio", rows, image=cover)


# --- Fonts ------------------------------------------------------------------

FONT_SAMPLE = (
    r"Aa Bb Cc Dd Ee Ff\nThe quick brown fox\njumps over the lazy dog\n0123456789 !?&@"
)


def charset_ranges(charset: str) -> list[tuple[int, int]]:
    """Parse fontconfig's "20-7e a0-ff ..." charset notation."""
    ranges = []
    for token in charset.split():
        first, _, last = token.partition("-")
        with contextlib.suppress(ValueError):
            ranges.append((int(first, 16), int(last or first, 16)))
    return ranges


def font_preview(path: Path, pane: Pane) -> Preview:
    out = run(
        [
            "fc-scan",
            "--format",
            "%{family[0]}\t%{style[0]}\t%{fontformat}\t%{fontversion}\t%{charset}\n",
            str(path),
        ]
    )
    faces = [line.split("\t") for line in (out or "").splitlines() if line.strip()]
    rows = []
    covers_latin = True
    if faces:
        family, style, fmt, version, charset = (faces[0] + [""] * 5)[:5]
        with contextlib.suppress(ValueError):
            version = f"{int(version) / 65536:.3f}".rstrip("0").rstrip(".")
        ranges = charset_ranges(charset)
        # Icon fonts have no Latin letters: a text sample would be all boxes.
        covers_latin = all(
            any(lo <= ord(c) <= hi for lo, hi in ranges) for c in "Aaz09"
        )
        rows = [
            ("Family", family),
            ("Style", style),
            ("Format", fmt),
            ("Version", version),
            (
                "Characters",
                f"{sum(hi - lo + 1 for lo, hi in ranges):,}" if ranges else "",
            ),
        ]
        if len(faces) > 1:
            rows.append(("Faces", str(len(faces))))

    def make(out: Path) -> bool:
        cmd = [
            "magick", "-background", "white", "-fill", "#202020", "-font", str(path),
            "-pointsize", "56", f"label:{FONT_SAMPLE}", "-bordercolor", "white",
            "-border", "24", "-resize", f"{pane.width}x>", str(out),
        ]  # fmt: skip
        return run(cmd, timeout=2.0) is not None

    image = cached_image(path, pane, ".png", make) if covers_latin else None
    if not image and not rows:
        return None
    return compose("Font", rows, image=image)


# --- Archives ---------------------------------------------------------------


@dataclass
class Entry:
    name: str
    size: int = -1
    is_dir: bool = False
    link: str = ""


def listing_line(entry: Entry) -> str:
    name = entry.name.removeprefix("./")
    if entry.is_dir:
        return f"{'':>6}  {sgr(name.rstrip('/') + '/', DIR_COLOR)}"
    size = human_size(entry.size, short=True) if entry.size >= 0 else ""
    if entry.link:
        name = sgr(name, LINK_COLOR) + sgr(f" → {entry.link}", DIM)
    return f"{sgr(f'{size:>6}', SIZE_COLOR)}  {name}"


def archive_result(
    entries: list[Entry],
    pane: Pane,
    *,
    badge: str = "Archive",
    complete: bool = True,
    packed: int | None = None,
    rows: Iterable[tuple[str, str]] = (),
    blocks: Iterable[tuple[str, str]] = (),
) -> dict:
    entries = [e for e in entries if e.name.removeprefix("./").strip("/.")]
    files = [e for e in entries if not e.is_dir]
    folders = len(entries) - len(files)
    if complete:
        listing = cap_lines([listing_line(e) for e in entries], pane, "entries")
    else:
        listing = "\n".join(listing_line(e) for e in entries[: pane.max_lines])
        listing += "\n" + sgr("… more entries, listing stopped early", DIM)
    if not entries:
        listing = sgr("Empty archive", DIM)
    count = plural(len(files), "file") + (
        f", {plural(folders, 'folder')}" if folders else ""
    )
    if not complete:
        count = f"More than {len(entries):,}"
    total = sum(e.size for e in files if e.size > 0)
    known_sizes = any(e.size >= 0 for e in files)
    meta = [
        *rows,
        ("Entries", count),
        ("Uncompressed", human_size(total) if known_sizes and complete else ""),
    ]
    if packed and total and complete:
        meta.append(("Compressed", f"{packed / total:.0%} of original"))
    blocks = list(blocks)
    if blocks:
        return compose(badge, meta, blocks=[*blocks, ("Contents", listing)])
    return compose(badge, meta, text=listing)


def zip_entries(path: Path) -> tuple[list[Entry], int, bool]:
    with zipfile.ZipFile(path) as z:
        infos = z.infolist()
    entries = [Entry(i.filename, i.file_size, i.is_dir()) for i in infos]
    packed = sum(i.compress_size for i in infos)
    encrypted = any(i.flag_bits & 1 for i in infos)
    return entries, packed, encrypted


def zip_preview(path: Path, pane: Pane) -> Preview:
    try:
        entries, packed, encrypted = zip_entries(path)
    except (zipfile.BadZipFile, OSError) as e:
        debug(f"zip: {e}")
        return tool_archive_preview(path, pane)
    return archive_result(
        entries, pane, packed=packed, rows=[("Encrypted", "yes" if encrypted else "")]
    )


def tar_entries(fileobj_or_path: Path | io.BytesIO) -> tuple[list[Entry], bool]:
    """List a tar archive, stopping early when time runs out."""
    entries: list[Entry] = []
    kwargs = (
        {"fileobj": fileobj_or_path}
        if isinstance(fileobj_or_path, io.BytesIO)
        else {"name": fileobj_or_path}
    )
    with tarfile.open(mode="r:*", **kwargs) as tf:
        for index, member in enumerate(tf):
            if index % 256 == 0 and (remaining() < 1.0 or index >= 50000):
                return entries, False
            entries.append(
                Entry(
                    member.name,
                    member.size if member.isfile() else -1,
                    member.isdir(),
                    member.linkname if member.issym() else "",
                )
            )
    return entries, True


def tar_preview(path: Path, pane: Pane) -> Preview:
    try:
        entries, complete = tar_entries(path)
    except (tarfile.TarError, OSError, EOFError, lzma.LZMAError, zlib.error) as e:
        debug(f"tar: {e}")
        return tool_archive_preview(path, pane)
    return archive_result(entries, pane, complete=complete, packed=path.stat().st_size)


BSDTAR_LINE = re.compile(
    r"^([-dlcbps])\S{9}\S*\s+\d+\s+\S+\s+\S+\s+(\d+)\s+\S+\s+\d+\s+\S+\s+(.*)$"
)


def sevenzip_entries(path: Path) -> list[Entry] | None:
    out = run(["7z", "l", "-slt", "-ba", "-sccUTF-8", str(path)])
    entries = []
    for block in (out or "").split("\n\n"):
        fields = parse_fields(block, " = ")
        if "Path" not in fields:
            continue
        is_dir = (
            fields.get("Folder") == "+"
            or "D" in fields.get("Attributes", "").split(" ")[0]
        )
        size = int(fields["Size"]) if fields.get("Size", "").isdigit() else -1
        entries.append(Entry(fields["Path"], size, is_dir))
    return entries or None


def bsdtar_entries(path: Path) -> list[Entry] | None:
    entries = []
    for line in (run(["bsdtar", "-tvf", str(path)]) or "").splitlines():
        if match := BSDTAR_LINE.match(line):
            kind, size, name = match.groups()
            link = ""
            if kind == "l" and " -> " in name:
                name, link = name.split(" -> ", 1)
            entries.append(
                Entry(name, int(size) if kind == "-" else -1, kind == "d", link)
            )
    return entries or None


def unrar_entries(path: Path) -> list[Entry] | None:
    out = run(["unrar", "lb", str(path)])
    return [Entry(name) for name in out.splitlines() if name] if out else None


def tool_entries(
    path: Path,
    listers: Iterable[Callable[[Path], list[Entry] | None]] = (
        sevenzip_entries,
        bsdtar_entries,
        unrar_entries,
    ),
) -> list[Entry] | None:
    for lister in listers:
        if entries := lister(path):
            return entries
    return None


def tool_archive_preview(path: Path, pane: Pane) -> Preview:
    entries = tool_entries(path)
    if entries is None:
        return None
    return archive_result(entries, pane, packed=path.stat().st_size)


def read_ar(
    path: Path, wanted: Callable[[str], bool], limit: int = 64 << 20
) -> dict[str, bytes]:
    """Read the members of an ar archive (a .deb) whose names are wanted."""
    members = {}
    with open(path, "rb") as f:
        if f.read(8) != b"!<arch>\n":
            raise ValueError("not an ar archive")
        while header := f.read(60):
            if len(header) < 60:
                break
            name = header[:16].decode("ascii", "replace").strip().rstrip("/")
            size = int(header[48:58].decode().strip() or 0)
            if wanted(name) and size <= limit:
                members[name] = f.read(size)
                f.seek(size % 2, os.SEEK_CUR)
            else:
                f.seek(size + size % 2, os.SEEK_CUR)
    return members


def deb_preview(path: Path, pane: Pane) -> Preview:
    try:
        members = read_ar(
            path, lambda name: name.startswith(("control.tar", "data.tar"))
        )
        control_data = next(
            v for k, v in members.items() if k.startswith("control.tar")
        )
        with tarfile.open(fileobj=io.BytesIO(control_data)) as tf:
            member = next(m for m in tf if m.name.lstrip("./") == "control")
            control = tf.extractfile(member).read().decode("utf-8", "replace")  # type: ignore[union-attr]
    except (ValueError, StopIteration, tarfile.TarError, OSError, AttributeError) as e:
        debug(f"deb: {e}")
        return tool_archive_preview(path, pane)

    fields = parse_fields(control)
    description = []
    lines = control.splitlines()
    for i, line in enumerate(lines):
        if line.startswith("Description:"):
            for cont in lines[i + 1 :]:
                if not cont.startswith(" "):
                    break
                description.append("" if cont.strip() == "." else cont.strip())
    rows = [
        (key, fields.get(key, ""))
        for key in (
            "Package",
            "Version",
            "Architecture",
            "Maintainer",
            "Section",
            "Homepage",
        )
    ]
    rows.append(("Summary", fields.get("Description", "")))
    entries, complete = [], True
    data = next((v for k, v in members.items() if k.startswith("data.tar")), None)
    if data:
        with contextlib.suppress(tarfile.TarError, EOFError, lzma.LZMAError):
            entries, complete = tar_entries(io.BytesIO(data))
            entries = [e for e in entries if e.name not in (".", "./")]
    return archive_result(
        entries, pane, badge="DEB", complete=complete, rows=rows,
        blocks=[("Description", "\n".join(description))],
    )  # fmt: skip


RPM_TAGS = {
    1000: "Name", 1001: "Version", 1002: "Release", 1004: "Summary",
    1005: "Description", 1014: "License", 1020: "URL", 1022: "Architecture",
}  # fmt: skip


def rpm_header(path: Path) -> dict[str, str]:
    """Read the string tags of an RPM package header."""
    with open(path, "rb") as f:
        if f.read(4) != b"\xed\xab\xee\xdb":
            raise ValueError("not an RPM package")
        f.seek(96)
        for signature in (True, False):
            magic, count, size = struct.unpack(">4s4xII", f.read(16))
            if magic[:3] != b"\x8e\xad\xe8":
                raise ValueError("bad RPM header")
            index = f.read(16 * count)
            store = f.read(size)
            if signature:
                f.seek(-size % 8, os.SEEK_CUR)
    fields = {}
    for offset in range(0, len(index), 16):
        tag, kind, start, _ = struct.unpack(">IIII", index[offset : offset + 16])
        # 6: string, 8: string array, 9: i18n string array (first is "C").
        if tag in RPM_TAGS and kind in (6, 8, 9):
            end = store.find(b"\0", start)
            fields[RPM_TAGS[tag]] = store[start:end].decode("utf-8", "replace")
    return fields


def rpm_preview(path: Path, pane: Pane) -> Preview:
    try:
        fields = rpm_header(path)
    except (ValueError, OSError, struct.error) as e:
        debug(f"rpm: {e}")
        return tool_archive_preview(path, pane)
    rows = [
        (key, fields.get(key, ""))
        for key in (
            "Name",
            "Version",
            "Release",
            "Architecture",
            "License",
            "URL",
            "Summary",
        )
    ]
    description = " ".join(fields.get("Description", "").split())
    if description == fields.get("Summary"):
        description = ""
    entries = bsdtar_entries(path)
    if entries is None:
        files = run(["rpm", "-qlp", str(path)]) or ""
        entries = [Entry(name) for name in files.splitlines() if name.startswith("/")]
    if not entries:
        return compose("RPM", rows, blocks=[("Description", description)])
    return archive_result(
        entries, pane, badge="RPM", rows=rows, blocks=[("Description", description)]
    )


# --- Single compressed files ------------------------------------------------


def zstd_open(path: Path) -> io.BufferedIOBase:
    from compression import zstd  # Python 3.14+

    return zstd.open(path)


OPENERS: dict[str, tuple[str, Callable]] = {
    "gz": ("gzip", gzip.open),
    "bz2": ("bzip2", bz2.open),
    "xz": ("xz", lzma.open),
    "lzma": ("lzma", lzma.open),
    "zst": ("zstd", zstd_open),
}


def compressed_preview(path: Path, pane: Pane) -> Preview:
    """Show the decompressed start of a .gz/.bz2/.xz/.zst file."""
    ext = path.suffix.lower().lstrip(".")
    name, opener = OPENERS.get(ext, ("gzip", gzip.open))
    try:
        with opener(path) as f:
            data = f.read(HEAD_BYTES + 1)
    except Exception as e:  # noqa: BLE001  each codec raises its own error types
        debug(f"{name}: {e}")
        if ext == "zst":
            out = run(["zstd", "-dc", str(path)])
            data = (out or "").encode()[: HEAD_BYTES + 1]
        else:
            return binary_preview(path, pane)
        if not data:
            return binary_preview(path, pane)
    if data[257:262] == b"ustar":
        return tar_preview(path, pane)

    truncated = len(data) > HEAD_BYTES
    data = data[:HEAD_BYTES]
    inner = path.name[: -len(path.suffix)] if path.suffix else path.name
    rows = [("Compression", name)]
    if ext == "gz" and not truncated:
        rows.append(("Uncompressed", human_size(len(data))))
    text = decode_text(data, truncated)
    if text is None:
        preview = binary_preview(path, pane, data)
        if isinstance(preview, dict):
            preview.setdefault("metadata", []).insert(0, list(rows[0]))
        return preview
    if truncated and "\n" in text:
        text = text[: text.rindex("\n") + 1]
    inner_ext = inner.lower().rpartition(".")[2]
    if inner_ext in ("json", "geojson", "jsonl", "ndjson") and not truncated:
        preview = render_json(text, inner, pane, inner_ext in ("jsonl", "ndjson"))
    else:
        preview = text_result(
            text, inner, pane, exact=not truncated, total=len(split_lines(text))
        )
    preview.setdefault("metadata", []).insert(0, list(rows[0]))
    return preview


# --- Office documents -------------------------------------------------------

W_NS = "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}"
A_NS = "{http://schemas.openxmlformats.org/drawingml/2006/main}"
S_NS = "{http://schemas.openxmlformats.org/spreadsheetml/2006/main}"
R_NS = "{http://schemas.openxmlformats.org/officeDocument/2006/relationships}"
TEXT_NS = "{urn:oasis:names:tc:opendocument:xmlns:text:1.0}"
DC_NS = "{http://purl.org/dc/elements/1.1/}"
META_NS = "{urn:oasis:names:tc:opendocument:xmlns:meta:1.0}"
EP_NS = "{http://schemas.openxmlformats.org/officeDocument/2006/extended-properties}"


def docx_lines(z: zipfile.ZipFile) -> list[str]:
    root = ET.fromstring(z.read("word/document.xml"))
    lines = []
    for p in root.iter(W_NS + "p"):
        parts = []
        for node in p.iter():
            if node.tag == W_NS + "t":
                parts.append(node.text or "")
            elif node.tag == W_NS + "tab":
                parts.append("    ")
            elif node.tag in (W_NS + "br", W_NS + "cr"):
                parts.append("\n")
        text = "".join(parts)
        style = p.find(f"{W_NS}pPr/{W_NS}pStyle")
        heading = style is not None and re.match(
            r"(Heading|Title)", style.get(W_NS + "val", ""), re.IGNORECASE
        )
        if heading and text.strip():
            lines += ["", sgr(text, BOLD)]
        else:
            lines += text.split("\n")
    return lines


def pptx_lines(z: zipfile.ZipFile) -> list[str]:
    slides = sorted(
        (n for n in z.namelist() if re.fullmatch(r"ppt/slides/slide\d+\.xml", n)),
        key=lambda n: int(re.search(r"\d+", n.rpartition("/")[2]).group()),  # type: ignore[union-attr]
    )
    lines = []
    for number, name in enumerate(slides, 1):
        lines += ["", sgr(f"Slide {number}", DIM)]
        root = ET.fromstring(z.read(name))
        for p in root.iter(A_NS + "p"):
            text = "".join(t.text or "" for t in p.iter(A_NS + "t"))
            if text.strip():
                lines.append(text)
    return lines


def odf_lines(z: zipfile.ZipFile) -> list[str]:
    root = ET.fromstring(z.read("content.xml"))
    lines = []
    for node in root.iter():
        if node.tag == TEXT_NS + "h":
            lines += ["", sgr("".join(node.itertext()), BOLD)]
        elif node.tag == TEXT_NS + "p":
            lines.append("".join(node.itertext()))
    return lines


def column_index(ref: str) -> int:
    index = 0
    for c in ref:
        if not c.isalpha():
            break
        index = index * 26 + ord(c.upper()) - 64
    return index - 1


def xlsx_rows(z: zipfile.ZipFile, limit: int) -> tuple[list[list[str]], list[str], str]:
    """First sheet rows, sheet names and the sheet dimension (e.g. A1:F20)."""
    shared = []
    if "xl/sharedStrings.xml" in z.namelist():
        root = ET.fromstring(z.read("xl/sharedStrings.xml"))
        shared = [
            "".join(t.text or "" for t in si.iter(S_NS + "t"))
            for si in root.iter(S_NS + "si")
        ]
    workbook = ET.fromstring(z.read("xl/workbook.xml"))
    sheets = workbook.findall(f"{S_NS}sheets/{S_NS}sheet")
    names = [s.get("name", "") for s in sheets]
    target = "xl/worksheets/sheet1.xml"
    if sheets and "xl/_rels/workbook.xml.rels" in z.namelist():
        rel_id = sheets[0].get(R_NS + "id")
        rels = ET.fromstring(z.read("xl/_rels/workbook.xml.rels"))
        for rel in rels:
            if rel.get("Id") == rel_id:
                target = posixpath.normpath(
                    posixpath.join(
                        "xl", rel.get("Target", "").lstrip("/").removeprefix("xl/")
                    )
                )
    rows: list[list[str]] = []
    dimension = ""
    with z.open(target) as f:
        for _, node in ET.iterparse(f):
            if node.tag == S_NS + "dimension":
                dimension = node.get("ref", "")
            elif node.tag == S_NS + "row":
                row: list[str] = []
                for cell in node.findall(S_NS + "c"):
                    col = column_index(cell.get("r", "")) if cell.get("r") else len(row)
                    if col >= 64:
                        continue
                    kind = cell.get("t")
                    value = cell.findtext(S_NS + "v") or ""
                    if kind == "s" and value.isdigit() and int(value) < len(shared):
                        value = shared[int(value)]
                    elif kind == "inlineStr":
                        value = "".join(cell.itertext())
                    elif kind == "b":
                        value = "TRUE" if value == "1" else "FALSE"
                    row += [""] * (col - len(row) + 1)
                    row[col] = value
                rows.append(row)
                node.clear()
                if len(rows) > limit:
                    break
    return rows, names, dimension


def office_properties(z: zipfile.ZipFile) -> list[tuple[str, str]]:
    names = z.namelist()
    rows = []
    if "docProps/core.xml" in names:
        core = ET.fromstring(z.read("docProps/core.xml"))
        rows += [
            ("Title", core.findtext(DC_NS + "title") or ""),
            ("Author", core.findtext(DC_NS + "creator") or ""),
        ]
    if "docProps/app.xml" in names:
        app = ET.fromstring(z.read("docProps/app.xml"))
        rows += [
            (key, app.findtext(EP_NS + key) or "")
            for key in ("Pages", "Words", "Slides")
        ]
    if "meta.xml" in names:
        meta = ET.fromstring(z.read("meta.xml"))
        rows += [
            ("Title", next((e.text or "" for e in meta.iter(DC_NS + "title")), "")),
            (
                "Author",
                next(
                    (e.text or "" for e in meta.iter(META_NS + "initial-creator")), ""
                ),
            ),
        ]
        stats = next(meta.iter(META_NS + "document-statistic"), None)
        if stats is not None:
            rows += [
                ("Pages", stats.get(META_NS + "page-count", "")),
                ("Words", stats.get(META_NS + "word-count", "")),
            ]
    return rows


def office_preview(path: Path, pane: Pane) -> Preview:
    ext = path.suffix.lower().lstrip(".")
    badge = ext.upper()
    try:
        with zipfile.ZipFile(path) as z:
            rows = office_properties(z)
            if ext == "xlsx":
                table, sheets, dimension = xlsx_rows(z, pane.max_lines)
                body, shown = render_table(table, pane)
                ncols = max((len(r) for r in table), default=0)
                size_rows = table_rows(ncols, shown, "")
                if match := re.fullmatch(r"[A-Z]+(\d+):([A-Z]+)(\d+)", dimension):
                    size_rows = table_rows(
                        column_index(match.group(2)) + 1,
                        shown,
                        f"{int(match.group(3)) - 1:,}",
                    )
                sheet_list = ", ".join(sheets[:5]) + (
                    f" (+{len(sheets) - 5})" if len(sheets) > 5 else ""
                )
                return compose(
                    badge,
                    [*rows, *size_rows, ("Sheets", sheet_list)],
                    blocks=[(sheets[0] if sheets else "", body)],
                )
            pandoc = (
                run(["pandoc", "-t", "plain", "--wrap=none", str(path)])
                if ext in ("docx", "odt")
                else None
            )
            if pandoc:
                lines = split_lines(pandoc)
            elif ext == "docx":
                lines = docx_lines(z)
            elif ext == "pptx":
                lines = pptx_lines(z)
            else:
                lines = odf_lines(z)
    except (zipfile.BadZipFile, KeyError, ET.ParseError, OSError, ValueError) as e:
        debug(f"office: {e}")
        return zip_preview(path, pane)
    lines = squeeze_blank(lines)
    if not lines:
        return zip_preview(path, pane)
    return compose(badge, rows, text=cap_lines(lines, pane))


# --- Ebooks -----------------------------------------------------------------

OPF_NS = "{http://www.idpf.org/2007/opf}"
CONTAINER_NS = "{urn:oasis:names:tc:opendocument:xmlns:container}"
LANGUAGE_NAMES = {
    "en": "English", "fr": "French", "de": "German", "es": "Spanish",
    "it": "Italian", "pt": "Portuguese", "nl": "Dutch", "ja": "Japanese",
    "zh": "Chinese", "ru": "Russian", "eng": "English", "fre": "French",
    "fra": "French", "ger": "German", "deu": "German", "spa": "Spanish",
}  # fmt: skip
IMAGE_EXTS_IN_ARCHIVE = (".jpg", ".jpeg", ".png", ".gif", ".webp")


def strip_html(text: str, limit: int = 1500) -> str:
    text = re.sub(r"(?i)<br\s*/?>|</p>|</div>", "\n", text)
    text = html.unescape(re.sub(r"<[^>]+>", "", text))
    paragraphs = [" ".join(p.split()) for p in text.split("\n")]
    text = "\n\n".join(p for p in paragraphs if p)
    return text if len(text) <= limit else text[:limit].rsplit(" ", 1)[0] + "…"


def language_name(code: str) -> str:
    return LANGUAGE_NAMES.get(code.lower().split("-")[0], code)


def write_bytes_image(data: bytes) -> Callable[[Path], bool]:
    def make(out: Path) -> bool:
        out.write_bytes(data)
        return True

    return make


def epub_preview(path: Path, pane: Pane) -> Preview:
    with zipfile.ZipFile(path) as z:
        container = ET.fromstring(z.read("META-INF/container.xml"))
        rootfile = container.find(f".//{CONTAINER_NS}rootfile")
        if rootfile is None:
            return None
        opf_path = rootfile.get("full-path", "")
        opf = ET.fromstring(z.read(opf_path))

        def dc(name: str) -> list[str]:
            return [
                " ".join((e.text or "").split())
                for e in opf.iter(DC_NS + name)
                if (e.text or "").strip()
            ]

        metas = {
            m.get("name"): m.get("content", "")
            for m in opf.iter(OPF_NS + "meta")
            if m.get("name")
        }
        collection = next(
            (
                m.text or ""
                for m in opf.iter(OPF_NS + "meta")
                if m.get("property") == "belongs-to-collection"
            ),
            "",
        )
        series = metas.get("calibre:series") or collection
        if series and metas.get("calibre:series_index"):
            series += f" #{metas['calibre:series_index'].removesuffix('.0')}"

        manifest = list(opf.iter(OPF_NS + "item"))
        cover_item = next(
            (
                i
                for i in manifest
                if "cover-image" in (i.get("properties") or "").split()
            ),
            None,
        )
        if cover_item is None and metas.get("cover"):
            cover_item = next(
                (i for i in manifest if i.get("id") == metas["cover"]), None
            )
        if cover_item is None:
            cover_item = next(
                (
                    i
                    for i in manifest
                    if "cover" in (i.get("id") or "").lower()
                    and (i.get("media-type") or "").startswith("image/")
                ),
                None,
            )
        cover = None
        if cover_item is not None:
            href = urllib.parse.unquote(cover_item.get("href", ""))
            name = posixpath.normpath(posixpath.join(posixpath.dirname(opf_path), href))
            with contextlib.suppress(KeyError):
                cover = cached_image(
                    path, pane, ".jpg", write_bytes_image(z.read(name))
                )

    dates = dc("date")
    rows = [
        ("Title", (dc("title") or [""])[0]),
        ("Author", ", ".join(dc("creator"))),
        ("Series", series),
        ("Publisher", (dc("publisher") or [""])[0]),
        ("Published", dates[0][:10] if dates else ""),
        ("Language", ", ".join(language_name(code) for code in dc("language"))),
        ("Subjects", ", ".join(dc("subject")[:4])),
    ]
    description = strip_html((dc("description") or [""])[0])
    return compose("EPUB", rows, image=cover, blocks=[("Description", description)])


def comic_preview(path: Path, pane: Pane) -> Preview:
    with zipfile.ZipFile(path) as z:
        pages = sorted(
            (
                n
                for n in z.namelist()
                if n.lower().endswith(IMAGE_EXTS_IN_ARCHIVE)
                and not n.startswith("__MACOSX")
            ),
            key=lambda n: [
                int(t) if t.isdigit() else t.lower() for t in re.split(r"(\d+)", n)
            ],
        )
        if not pages:
            return zip_preview(path, pane)
        cover = cached_image(path, pane, ".jpg", write_bytes_image(z.read(pages[0])))
    return compose("Comic", [("Pages", str(len(pages)))], image=cover)


def ebook_meta_preview(path: Path, pane: Pane) -> Preview:
    """Cover and metadata from calibre's ebook-meta (MOBI, AZW3, FB2, ...)."""
    if not shutil.which("ebook-meta"):
        return None
    output = None

    def make(out: Path) -> bool:
        nonlocal output
        output = run(["ebook-meta", str(path), "--get-cover", str(out)], timeout=2.5)
        return output is not None

    cover = cached_image(path, pane, ".jpg", make)
    if output is None:
        output = run(["ebook-meta", str(path)], timeout=2.5)
    if output is None:
        return None
    head, _, comments = output.partition("\nComments")
    fields = parse_fields(head)
    rows = [
        ("Title", fields.get("Title", "")),
        ("Author", re.sub(r"\s*\[[^]]*\]", "", fields.get("Author(s)", ""))),
        ("Series", fields.get("Series", "")),
        ("Publisher", fields.get("Publisher", "")),
        ("Published", fields.get("Published", "")[:10]),
        (
            "Language",
            ", ".join(
                language_name(c.strip())
                for c in fields.get("Languages", "").split(",")
                if c.strip()
            ),
        ),
    ]
    description = strip_html(comments.partition(":")[2])
    return compose(
        path.suffix.lstrip(".").upper() or "Ebook",
        rows,
        image=cover,
        blocks=[("Description", description)],
    )


def ebook_preview(path: Path, pane: Pane) -> Preview:
    ext = path.suffix.lower()
    try:
        if ext == ".epub":
            if preview := epub_preview(path, pane):
                return preview
        elif ext == ".cbz":
            return comic_preview(path, pane)
    except (zipfile.BadZipFile, KeyError, ET.ParseError, OSError) as e:
        debug(f"ebook: {e}")
    return ebook_meta_preview(path, pane)


# --- SQLite -----------------------------------------------------------------

SQLITE_MAGIC = b"SQLite format 3\0"


def wrap_tokens(
    tokens: list[tuple[str, str]], width: int, indent: str = "  "
) -> list[str]:
    """Wrap (plain, coloured) tokens joined by ", " to a width."""
    lines, current, used = [], [], len(indent)
    for plain, colored in tokens:
        size = len(plain) + 2
        if current and used + size > width:
            lines.append(indent + ", ".join(current) + ",")
            current, used = [], len(indent)
        current.append(colored)
        used += size
    if current:
        lines.append(indent + ", ".join(current))
    return lines


def sqlite_preview(path: Path, pane: Pane) -> Preview:
    if read_head(path, 16) != SQLITE_MAGIC:
        return content_preview(path, pane)
    uri = f"file:{urllib.parse.quote(str(path))}?mode=ro&immutable=1"
    try:
        con = sqlite3.connect(uri, uri=True, timeout=0.5)
    except sqlite3.Error as e:
        debug(f"sqlite: {e}")
        return None
    try:
        objects = con.execute(
            "SELECT type, name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY name"
        ).fetchall()
        tables = [name for kind, name in objects if kind == "table"]
        views = [name for kind, name in objects if kind == "view"]
        count_deadline = time.monotonic() + max(min(1.0, remaining() - 0.8), 0.1)
        lines = []
        for name in tables[: pane.max_lines // 3]:
            quoted = '"' + name.replace('"', '""') + '"'
            columns = con.execute(f"PRAGMA table_info({quoted})").fetchall()
            # count(*) scans the table: stop counting when time runs out.
            con.set_progress_handler(lambda: time.monotonic() > count_deadline, 10000)
            try:
                rows = plural(
                    con.execute(f"SELECT count(*) FROM {quoted}").fetchone()[0], "row"
                )
            except sqlite3.Error:
                rows = "? rows"
            finally:
                con.set_progress_handler(None, 0)
            lines.append(f"{sgr(name, BOLD)}  {sgr(rows, DIM)}")
            tokens = [
                (
                    f"{col[1]} {col[2]}{' PK' if col[5] else ''}".strip(),
                    col[1]
                    + (
                        " " + sgr(f"{col[2]}{' PK' if col[5] else ''}".strip(), DIM)
                        if col[2] or col[5]
                        else ""
                    ),
                )
                for col in columns
            ]
            lines += wrap_tokens(tokens, pane.columns)
        if len(tables) > pane.max_lines // 3:
            lines.append(more_marker(len(tables) - pane.max_lines // 3, "tables"))
        if views:
            lines += ["", sgr("Views", BOLD)]
            lines += wrap_tokens([(v, v) for v in views], pane.columns)
    except sqlite3.Error as e:
        debug(f"sqlite: {e}")
        return None
    finally:
        con.close()
    counts = {
        kind: sum(1 for k, _ in objects if k == kind)
        for kind in ("table", "view", "index", "trigger")
    }
    rows_meta = [
        (f"{kind.capitalize()}s" if kind != "index" else "Indexes", str(n))
        for kind, n in counts.items()
        if n
    ]
    return compose("SQLite", rows_meta, text="\n".join(lines) or sgr("No tables", DIM))


# --- Binary files -----------------------------------------------------------


def byte_style(byte: int) -> tuple[str, str]:
    """Colour and ASCII column glyph of a byte, after hexyl."""
    if byte == 0:
        return DIM, "⋄"
    if byte in (0x09, 0x0A, 0x0D, 0x20):
        return "32", "_" if byte != 0x20 else " "
    if 0x20 < byte < 0x7F:
        return "36", chr(byte)
    if byte < 0x80:
        return "35", "•"
    return "33", "×"


def colored_run(pieces: list[tuple[str, str]], joiner: str = "") -> str:
    """Join (colour, text) pieces, opening one SGR per run of a colour."""
    runs, run_color, run_text = [], "", []
    for color, text in pieces:
        if color != run_color and run_text:
            runs.append(sgr(joiner.join(run_text), run_color))
            run_text = []
        run_color = color
        run_text.append(text)
    if run_text:
        runs.append(sgr(joiner.join(run_text), run_color))
    return joiner.join(runs)


def hexdump(data: bytes, columns: int) -> str:
    per_row = 16 if columns >= 76 else 8 if columns >= 44 else 4
    # Wide rows are split in two groups of 8 bytes, like xxd and hexyl.
    group = min(per_row, 8)
    lines, previous, squeezed = [], None, False
    for offset in range(0, len(data), per_row):
        row = data[offset : offset + per_row]
        if row == previous and offset + per_row < len(data):
            if not squeezed:
                lines.append(sgr("*", DIM))
                squeezed = True
            continue
        previous, squeezed = row, False
        styles = [byte_style(b) for b in row]
        chunks = [range(i, min(i + group, len(row))) for i in range(0, per_row, group)]
        hex_part = "  ".join(
            colored_run([(styles[j][0], f"{row[j]:02x}") for j in chunk], " ")
            for chunk in chunks
        )
        used = sum(max(len(chunk) * 3 - 1, 0) for chunk in chunks)
        hex_part += " " * (len(chunks) * (group * 3 - 1) - used)
        ascii_part = colored_run(styles)
        lines.append(f"{sgr(f'{offset:08x}', DIM)}  {hex_part}  {ascii_part}")
    return "\n".join(lines)


def binary_preview(path: Path, pane: Pane, data: bytes | None = None) -> Preview:
    description = (run(["file", "-Lb", str(path)]) or "").strip()
    if data is None:
        dump = run(
            [
                "hexyl",
                f"--length={HEX_BYTES}",
                "--border=none",
                "--color=always",
                f"--terminal-width={pane.columns}",
                str(path),
            ]
        )
        data = read_head(path, HEX_BYTES)
    else:
        dump = None
    dump = dump or hexdump(data[:HEX_BYTES], pane.columns)
    if not dump.strip():
        return None
    heading = f"First {min(len(data), HEX_BYTES):,} bytes"
    return compose("Binary", blocks=[("Type", description), (heading, dump)])


# --- Directories ------------------------------------------------------------


def python_tree(path: Path, limit: int, depth: int = 2) -> list[str]:
    lines: list[str] = []

    def walk(directory: str, prefix: str, level: int) -> bool:
        try:
            with os.scandir(directory) as it:
                entries = [e for e in it if e.name not in IGNORED_DIRS]
        except OSError:
            return True
        entries.sort(key=lambda e: (not e.is_dir(), e.name.lower()))
        for index, entry in enumerate(entries):
            if len(lines) >= limit:
                return False
            last = index == len(entries) - 1
            is_dir = entry.is_dir(follow_symlinks=False)
            name = entry.name
            if is_dir:
                name = sgr(name, DIR_COLOR)
            elif entry.is_symlink():
                name = sgr(name, LINK_COLOR)
            lines.append(sgr(prefix + ("└── " if last else "├── "), DIM) + name)
            if (
                is_dir
                and level < depth
                and not walk(
                    entry.path, prefix + ("    " if last else "│   "), level + 1
                )
            ):
                return False
        return True

    if not walk(str(path), "", 1):
        lines.append(sgr("… more entries", DIM))
    return lines


def directory_preview(path: Path, pane: Pane) -> Preview:
    folders = files = 0
    with contextlib.suppress(OSError), os.scandir(path) as it:
        for entry in it:
            with contextlib.suppress(OSError):
                if entry.is_dir():
                    folders += 1
                else:
                    files += 1
    ignore = "|".join(IGNORED_DIRS)
    tree = run(
        ["eza", "--tree", "--level=2", "--all", "--color=always", "--icons=never",
         "--group-directories-first", f"--ignore-glob={ignore}", str(path)],
        env=plain_color_env(),
    )  # fmt: skip
    if tree is None:
        globs = [arg for name in IGNORED_DIRS for arg in ("--ignore-glob", name)]
        tree = run(
            ["lsd", "--tree", "--depth=2", "--almost-all", "--color=always", "--icon=never",
             "--group-directories-first", *globs, str(path)],
            env=plain_color_env(),
        )  # fmt: skip
    if tree is not None:
        lines = cap_lines(split_lines(tree)[1:], pane, "entries")
    else:
        lines = "\n".join(python_tree(path, pane.max_lines))
    count = ", ".join(
        plural(n, noun) for n, noun in ((folders, "folder"), (files, "file")) if n
    )
    return compose("Folder", [("Items", count)], text=lines or sgr("Empty folder", DIM))


# --- Preview cache ----------------------------------------------------------

# Previews of regular files slower than this are cached as JSON (big tar.xz
# listings, ebook-meta, ...). Directories are not: their mtime does not change
# when nested entries do.
SLOW_PREVIEW = 0.25


def preview_cache_file(path: Path, pane: Pane) -> Path:
    theme = [os.environ.get(name, "") for name in THEME_ENV]
    key = cache_key(path, pane.width, pane.columns, pane.lines, *theme)
    return CACHE_DIR / f"{key}.json"


def load_preview(cache_file: Path) -> dict | None:
    try:
        preview = json.loads(cache_file.read_text())
    except (OSError, ValueError):
        return None
    images = [preview.get("image")] + [
        s.get("path") for s in preview.get("sections", []) if s.get("type") == "image"
    ]
    if not all(Path(image).is_file() for image in images if image):
        return None
    touch(cache_file)
    return preview


def store_preview(cache_file: Path, preview: dict) -> None:
    try:
        CACHE_DIR.mkdir(parents=True, exist_ok=True)
        tmp = cache_file.with_name(f".{cache_file.name}.{os.getpid()}")
        tmp.write_text(json.dumps(preview, ensure_ascii=False))
        os.replace(tmp, cache_file)
    except OSError as e:
        debug(f"cache: {e}")
        return
    prune_cache()


# --- Dispatch ---------------------------------------------------------------


Handler = Callable[[Path, Pane], Preview]


def ts_preview(path: Path, pane: Pane) -> Preview:
    """.ts is TypeScript or an MPEG transport stream (0x47 every 188 bytes)."""
    head = read_head(path, 189)
    if head[:1] == b"\x47" and head[188:189] in (b"", b"\x47"):
        return video_preview(path, pane)
    return text_preview(path, pane)


EXTENSIONS: dict[str, Handler] = {
    **dict.fromkeys(LANGUAGES, text_preview),
    **dict.fromkeys(NATIVE_IMAGES | CONVERTED_IMAGES, image_preview),
    **dict.fromkeys(VIDEO_EXTS, video_preview),
    "ts": ts_preview,
    **dict.fromkeys(AUDIO_EXTS, audio_preview),
    "pdf": pdf_preview,
    **dict.fromkeys(
        ("epub", "mobi", "azw", "azw3", "fb2", "cbz", "lit", "pdb"), ebook_preview
    ),
    **dict.fromkeys(("md", "markdown", "mdx"), markdown_preview),
    **dict.fromkeys(
        ("json", "geojson", "jsonl", "ndjson", "har", "ipynb"), json_preview
    ),
    **dict.fromkeys(("csv", "tsv"), csv_preview),
    **dict.fromkeys(("html", "htm", "xhtml"), html_preview),
    **dict.fromkeys(
        ("xml", "xsd", "xsl", "xslt", "plist", "rss", "atom", "kml", "gpx"), xml_preview
    ),
    **dict.fromkeys(
        (
            "tar",
            "tgz",
            "tbz",
            "tbz2",
            "txz",
            "tzst",
            "tar.gz",
            "tar.bz2",
            "tar.xz",
            "tar.zst",
            "tar.lzma",
        ),
        tar_preview,
    ),
    **dict.fromkeys(
        (
            "zip",
            "jar",
            "war",
            "apk",
            "aar",
            "whl",
            "xpi",
            "ipa",
            "nupkg",
            "vsix",
            "crx",
        ),
        zip_preview,
    ),
    **dict.fromkeys(
        ("7z", "rar", "iso", "cab", "cpio", "xar", "lzh", "arj", "dmg", "msi", "cbr"),
        tool_archive_preview,
    ),
    **dict.fromkeys(OPENERS, compressed_preview),
    "deb": deb_preview,
    "rpm": rpm_preview,
    **dict.fromkeys(("docx", "pptx", "xlsx", "odt", "odp"), office_preview),
    **dict.fromkeys(("ttf", "otf", "ttc", "otc", "woff", "woff2", "pfb"), font_preview),
    **dict.fromkeys(("sqlite", "sqlite3", "db", "db3"), sqlite_preview),
}

MIMES: list[tuple[str, Handler]] = [
    ("application/pdf", pdf_preview),
    ("application/epub+zip", ebook_preview),
    ("application/x-mobipocket-ebook", ebook_preview),
    ("application/json", json_preview),
    ("application/geo+json", json_preview),
    ("text/html", html_preview),
    ("application/xhtml+xml", html_preview),
    ("text/xml", xml_preview),
    ("application/xml", xml_preview),
    ("text/csv", csv_preview),
    ("text/tab-separated-values", csv_preview),
    ("text/markdown", markdown_preview),
    ("application/vnd.sqlite3", sqlite_preview),
    ("application/x-sqlite3", sqlite_preview),
    ("application/zip", zip_preview),
    ("application/java-archive", zip_preview),
    ("application/x-tar", tar_preview),
    ("application/gzip", compressed_preview),
    ("application/x-bzip2", compressed_preview),
    ("application/x-xz", compressed_preview),
    ("application/zstd", compressed_preview),
    ("application/x-7z-compressed", tool_archive_preview),
    ("application/vnd.rar", tool_archive_preview),
    ("application/x-rar", tool_archive_preview),
    ("application/x-iso9660-image", tool_archive_preview),
    ("application/x-cpio", tool_archive_preview),
    ("application/vnd.debian.binary-package", deb_preview),
    ("application/x-rpm", rpm_preview),
    ("font/", font_preview),
    ("application/font-sfnt", font_preview),
    ("application/vnd.ms-opentype", font_preview),
    ("image/", image_preview),
    ("video/", video_preview),
    ("audio/", audio_preview),
    ("text/", text_preview),
    ("application/javascript", text_preview),
    ("application/x-sh", text_preview),
    ("application/x-shellscript", text_preview),
    ("application/toml", text_preview),
    ("application/yaml", text_preview),
    ("application/x-yaml", text_preview),
    ("application/x-ndjson", text_preview),
    ("application/sql", text_preview),
    ("application/x-perl", text_preview),
    ("application/x-ruby", text_preview),
    ("application/x-php", text_preview),
    ("application/x-awk", text_preview),
    ("application/x-subrip", text_preview),
    ("application/pgp-keys", text_preview),
    ("application/x-pem-file", text_preview),
    ("application/mbox", text_preview),
]


def handler_for_name(name: str) -> Handler | None:
    parts = name.lower().split(".")[1:]
    if len(parts) >= 2 and (handler := EXTENSIONS.get(".".join(parts[-2:]))):
        return handler
    return EXTENSIONS.get(parts[-1]) if parts else None


def handler_for_mime(mime: str) -> Handler | None:
    for prefix, handler in MIMES:
        if mime == prefix or (prefix.endswith("/") and mime.startswith(prefix)):
            return handler
    return None


def mime_type(path: Path) -> str:
    out = run(["file", "-Lb", "--mime-type", str(path)], timeout=1.0)
    if out and out.strip():
        return out.strip()
    return mimetypes.guess_type(path.name)[0] or ""


def content_preview(path: Path, pane: Pane) -> Preview:
    """Guess from the first bytes when name and MIME type say nothing."""
    head = read_head(path, 8192)
    if head.startswith(SQLITE_MAGIC):
        return sqlite_preview(path, pane)
    if head.startswith(b"%PDF-"):
        return pdf_preview(path, pane)
    if head.startswith(b"PK\x03\x04"):
        return zip_preview(path, pane)
    if head.startswith(b"\x1f\x8b"):
        return compressed_preview(path, pane)
    if head[257:262] == b"ustar":
        return tar_preview(path, pane)
    if sniff_image_ext(head):
        return converted_image_preview(path, pane)
    if decode_text(head, truncated=True) is None:
        return binary_preview(path, pane)
    return text_preview(path, pane)


def no_preview(path: Path, pane: Pane) -> Preview:
    return None


def choose_handler(path: Path, st: os.stat_result) -> Handler:
    if stat.S_ISDIR(st.st_mode):
        return directory_preview
    if st.st_size == 0:
        return no_preview
    if handler := handler_for_name(path.name):
        return handler
    mime = mime_type(path)
    debug(f"mime type: {mime}")
    return handler_for_mime(mime) or content_preview


# --- Output -----------------------------------------------------------------

PREVIEW_KEYS = (
    "title",
    "subtitle",
    "badge",
    "icon",
    "text",
    "image",
    "sections",
    "metadata",
)


def clean_preview(preview: dict) -> dict:
    """Keep only the keys Raffi reads and strip non-SGR escapes from text."""
    out = {key: preview[key] for key in PREVIEW_KEYS if key in preview}
    if "text" in out:
        out["text"] = clean(out["text"])
    if "sections" in out:
        out["sections"] = [
            {**s, "content": clean(s["content"])} if s.get("type") == "text" else s
            for s in out["sections"]
        ]
    if "metadata" in out:
        out["metadata"] = [[clean(str(k)), clean(str(v))] for k, v in out["metadata"]]
    return out


def emit(preview: Preview) -> int:
    """Print a preview; return the exit status (1 means no preview)."""
    if isinstance(preview, dict):
        preview = clean_preview(preview)
        if not any(key in preview for key in ("text", "image", "sections", "metadata")):
            return 1
        output = json.dumps(preview, ensure_ascii=False)
    elif isinstance(preview, str) and preview.strip():
        output = clean(preview)
    else:
        return 1
    sys.stdout.buffer.write(output.encode("utf-8", "replace") + b"\n")
    sys.stdout.flush()
    return 0


def main(argv: list[str] | None = None) -> int:
    global DEADLINE
    DEADLINE = time.monotonic() + BUDGET
    args = sys.argv[1:] if argv is None else argv
    if len(args) != 1:
        print("usage: file-preview.py <path>", file=sys.stderr)
        return 1
    path = Path(os.path.abspath(os.path.expanduser(args[0])))
    try:
        # stat() only: opening a FIFO would block until a writer shows up.
        st = path.stat()
    except OSError as e:
        print(f"file-preview: {args[0]}: {e.strerror}", file=sys.stderr)
        return 1
    if not (stat.S_ISREG(st.st_mode) or stat.S_ISDIR(st.st_mode)):
        debug("special file, leaving it to Raffi")
        return 1
    pane = Pane.from_env()
    handler = choose_handler(path, st)
    debug(f"{path}: {handler.__name__} {pane}")
    cache_file = None
    if stat.S_ISREG(st.st_mode):
        cache_file = preview_cache_file(path, pane)
        if cached := load_preview(cache_file):
            debug(f"cache hit {cache_file}")
            return emit(cached)
    start = time.monotonic()
    try:
        preview = handler(path, pane)
    except Exception:  # noqa: BLE001  a broken file must not break the preview pane
        if DEBUG:
            traceback.print_exc()
        return 1
    slow = time.monotonic() - start > SLOW_PREVIEW
    if cache_file and slow and isinstance(preview, dict):
        store_preview(cache_file, preview)
    return emit(preview)


if __name__ == "__main__":
    raise SystemExit(main())
