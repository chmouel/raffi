"""What the GitHub script filters for Raffi share: running `gh`, GraphQL
requests through its cache, error rows and terminal colours.

`pr.py` and `repos.py` import this module, so it must be installed in the
same directory as they are.
"""

import json
import os
import re
import subprocess
from datetime import UTC, datetime

API_HOST = os.environ.get("API_HOST", "github.com")
# Raffi sets it for Ctrl+R and for the server's warming runs: read live state.
REFRESH = os.environ.get("RAFFI_REFRESH") == "1"
ICON = "github"

ANSI = re.compile(r"\x1b\[[0-9;]*m")
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
