"""What the GitHub script filters for Raffi share: running `gh`, GraphQL
requests through its cache, error rows and terminal colours.

`pr.py` and `repos.py` import this module, so it must be installed in the
same directory as they are.
"""

import json
import math
import os
import re
import subprocess
from datetime import UTC, datetime

API_HOST = os.environ.get("API_HOST", "github.com")
# Raffi sets it for Ctrl+R and for the server's warming runs: read live state.
REFRESH = os.environ.get("RAFFI_REFRESH") == "1"
ICON = "github"
# Send times as data for Raffi to render ("3h ago") instead of text that goes
# stale in its cache. Needs a Raffi that knows the `time` item field.
TYPED_TIME = os.environ.get("TYPED_TIME") == "1"

ANSI = re.compile(r"\x1b\[[0-9;]*m")
DURATION = re.compile(r"([0-9]+(?:\.[0-9]*)?|\.[0-9]+)(ns|us|µs|μs|ms|[smhd])")
DURATION_UNITS = {
    "ns": 1e-9, "us": 1e-6, "µs": 1e-6, "μs": 1e-6, "ms": 1e-3,
    "s": 1, "m": 60, "h": 3600, "d": 86400,
}
BOLD = "\033[1m"
GREEN = "\033[32m"
RED = "\033[31m"
YELLOW = "\033[33m"
GREY = "\033[90m"
RESET = "\033[0m"


class GhError(Exception):
    pass


def run_gh(*args: str, timeout: float = 30) -> dict:
    """Run gh and parse its JSON output."""
    try:
        result = subprocess.run(
            ["gh", *args], capture_output=True, text=True, timeout=timeout, check=False
        )
    except FileNotFoundError:
        raise GhError("GitHub CLI not found, see https://cli.github.com") from None
    except subprocess.TimeoutExpired:
        raise GhError(f"gh did not answer within {timeout} seconds") from None
    if result.returncode:
        raise GhError(result.stderr.strip() or f"gh exited with {result.returncode}")
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise GhError(f"gh returned invalid JSON: {error}") from None


def graphql(
    query: str, cache: str | None = None, timeout: float = 30, **variables
) -> dict:
    """Run a GraphQL query, cached by gh for `cache` such as "10m".

    RAFFI_REFRESH skips the cache. Variables set to None are left out.
    """
    args = ["api", "graphql", "--hostname", API_HOST]
    if cache and not REFRESH:
        args += ["--cache", cache]
    for name, value in {"query": query, **variables}.items():
        if value is None:
            continue
        # -F sends integers as numbers, -f keeps everything else a string.
        args += ["-F" if isinstance(value, int) else "-f", f"{name}={value}"]
    response = run_gh(*args, timeout=timeout)
    if errors := response.get("errors"):
        raise GhError(errors[0].get("message", "GraphQL error"))
    return response["data"]


def colored(text: str, color: str) -> str:
    return f"{color}{text}{RESET}"


def time_field(timestamp: str | None, label: str) -> dict | None:
    """The `time` item field for an ISO 8601 timestamp, or None without one."""
    return {"at": timestamp, "label": label} if timestamp else None


def time_text(item: dict) -> str:
    """What Raffi will show for an item's `time` field, for matching filters."""
    time = item.get("time")
    if not time:
        return ""
    return f"{time.get('label') or ''} {ago(time['at'])}".strip()


def ago(timestamp: str | None) -> str:
    """How long ago an ISO 8601 timestamp was, such as `3h ago`."""
    if not timestamp:
        return ""
    since = datetime.now(UTC) - datetime.fromisoformat(timestamp)
    seconds = since.total_seconds()
    for unit, length in (
        ("y", 31536000),
        ("mo", 2592000),
        ("d", 86400),
        ("h", 3600),
        ("m", 60),
    ):
        if seconds >= length:
            return f"{int(seconds // length)}{unit} ago"
    return "just now"


def error_item(title: str, message: str) -> dict:
    """A row that shows an error and does nothing when chosen."""
    return {
        "title": title,
        "subtitle": message,
        "valid": False,
        "preview": {"title": title, "text": message, "dynamic": False},
    }


class ErrorRows(list):
    """Rows that report a failure, which Raffi must not keep as the result."""


def error_rows(title: str, message: str) -> ErrorRows:
    return ErrorRows([error_item(title, message)])


def seconds(duration: str, default: float = 600) -> float:
    """A gh-style duration such as 10m or 2h30m, in seconds."""
    duration = duration.strip().removeprefix("+")
    if duration == "0":
        return 0
    if not duration:
        return default
    total = 0.0
    end = 0
    while end < len(duration):
        match = DURATION.match(duration, end)
        if not match:
            return default
        value, unit = match.groups()
        total += float(value) * DURATION_UNITS[unit]
        end = match.end()
    return total if math.isfinite(total) else default


def with_cache(
    output: dict, items: list, duration: str, warm: tuple[str, ...] = ()
) -> dict:
    """Tell Raffi how long `output` stays fresh.

    `warm` lists queries worth keeping fresh in the background besides the
    ones the config warms, which Raffi bounds to a few short ones.

    A gh cache window such as `10m` becomes the hint, so Raffi keeps the list
    as long as `gh` would serve it, within what the user's `cache_max_seconds`
    allows. Error rows ask not to be kept: Raffi then keeps the rows it already
    has and reports the failed refresh instead of replacing them.
    """
    if isinstance(items, ErrorRows):
        return {**output, "cache": {"seconds": 0}, "items": items}

    cache: dict = {}
    window = seconds(duration, default=math.nan)
    if math.isfinite(window):
        cache["seconds"] = max(1, int(window))
    if warm:
        cache["warm"] = list(warm)
    result = {**output, "items": items}
    if cache:
        result["cache"] = cache
    return result
