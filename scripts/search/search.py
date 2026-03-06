#!/usr/bin/env python3
"""
DuckDuckGo autocomplete search script filter for Raffi.

Usage: search.py <engine> [query]
  engine  - one of the keys in ENGINES dict below
  query   - search query (passed automatically by Raffi)

Outputs Alfred-compatible JSON for Raffi script filter consumption.

Debug mode: Set DEBUG=1 environment variable to enable debug output to stderr.
"""

import json
import os
import sys
import urllib.parse
import urllib.request

# Configure search engines here. Add or remove engines as desired.
ENGINES = {
    "google": "https://www.google.com/search?q={query}",
    "duckduckgo": "https://duckduckgo.com/?q={query}",
    "bing": "https://www.bing.com/search?q={query}",
    "brave": "https://search.brave.com/search?q={query}",
}

DDG_AC_URL = "https://duckduckgo.com/ac/?q={}&type=list"

DEBUG = os.getenv("DEBUG", "").lower() in ("1", "true", "yes")


def debug(msg: str) -> None:
    """Print debug message to stderr if DEBUG is enabled."""
    if DEBUG:
        print(f"[DEBUG] {msg}", file=sys.stderr)


def build_url(engine: str, query: str) -> str:
    encoded = urllib.parse.quote_plus(query)
    return ENGINES[engine].replace("{query}", encoded)


def fetch_suggestions(query: str) -> list[str]:
    url = DDG_AC_URL.format(urllib.parse.quote_plus(query))
    debug(f"Fetching suggestions from: {url}")
    req = urllib.request.Request(url, headers={"User-Agent": "raffi-search/1.0"})
    with urllib.request.urlopen(req, timeout=2) as resp:
        data = json.loads(resp.read().decode())
    # DDG returns ["query", ["sug1", "sug2", ...]]
    if isinstance(data, list) and len(data) >= 2 and isinstance(data[1], list):
        debug(f"Got {len(data[1])} suggestions")
        return data[1]
    debug("No suggestions found in response")
    return []


def make_item(title: str, engine: str) -> dict:
    url = build_url(engine, title)
    engine_label = engine.capitalize()
    return {
        "title": title,
        "subtitle": f'Search {engine_label} for "{title}"',
        "arg": url,
    }


def main() -> None:
    if len(sys.argv) < 2:
        print(json.dumps({"items": [], "error": "Usage: search.py <engine> [query]"}))
        sys.exit(1)

    engine = sys.argv[1].lower()
    debug(f"Engine: {engine}")
    if engine not in ENGINES:
        known = ", ".join(sorted(ENGINES.keys()))
        print(
            json.dumps(
                {"items": [], "error": f"Unknown engine '{engine}'. Known: {known}"}
            )
        )
        sys.exit(1)

    query = sys.argv[2] if len(sys.argv) > 2 else ""
    debug(f"Query: '{query}'")

    if not query.strip():
        print(json.dumps({"items": []}))
        return

    items = [make_item(query, engine)]

    try:
        suggestions = fetch_suggestions(query)
        for suggestion in suggestions:
            if suggestion != query:
                items.append(make_item(suggestion, engine))
    except Exception as e:
        # On any error (timeout, network issue), fall back to raw query only
        debug(f"Error fetching suggestions: {e}")

    debug(f"Returning {len(items)} items")
    print(json.dumps({"items": items}))


if __name__ == "__main__":
    main()
