#!/usr/bin/env python3
"""
DuckDuckGo autocomplete search script filter for Raffi.

Usage: search.py <engine> [query]
  engine  - one of the keys in ENGINES dict below
  query   - search query (passed automatically by Raffi)

Outputs Alfred-compatible JSON for Raffi script filter consumption.

Preview mode: when Raffi sets RAFFI_PREVIEW=1, the script prints a preview of
the selected suggestion (RAFFI_TITLE) built from the DuckDuckGo Instant Answer
API, with its image when there is one.

Debug mode: Set DEBUG=1 environment variable to enable debug output to stderr.
"""

import base64
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
DDG_IA_URL = "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1"
DDG_BASE_URL = "https://duckduckgo.com"
USER_AGENT = "raffi-search/1.0"
# Raffi gives the preview command 3 seconds in total.
PREVIEW_TIMEOUT = 1.2

DEBUG = os.getenv("DEBUG", "").lower() in ("1", "true", "yes")


def debug(msg: str) -> None:
    """Print debug message to stderr if DEBUG is enabled."""
    if DEBUG:
        print(f"[DEBUG] {msg}", file=sys.stderr)


def build_url(engine: str, query: str) -> str:
    encoded = urllib.parse.quote_plus(query)
    return ENGINES[engine].replace("{query}", encoded)


def fetch(url: str, timeout: float) -> bytes:
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return resp.read()


def fetch_suggestions(query: str) -> list[str]:
    url = DDG_AC_URL.format(urllib.parse.quote_plus(query))
    debug(f"Fetching suggestions from: {url}")
    data = json.loads(fetch(url, 2).decode())
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


def fetch_instant_answer(query: str) -> dict:
    url = DDG_IA_URL.format(urllib.parse.quote_plus(query))
    debug(f"Fetching instant answer from: {url}")
    data = json.loads(fetch(url, PREVIEW_TIMEOUT).decode())
    return data if isinstance(data, dict) else {}


def fetch_image(path: str) -> str | None:
    if not path:
        return None
    url = path if path.startswith("http") else DDG_BASE_URL + path
    try:
        return base64.b64encode(fetch(url, PREVIEW_TIMEOUT)).decode()
    except Exception as e:
        debug(f"Error fetching image {url}: {e}")
        return None


def build_preview(engine: str, title: str, url: str) -> dict:
    engine_label = engine.capitalize()
    preview = {
        "title": title,
        "subtitle": f"Enter opens {engine_label}",
        "badge": engine_label,
        "metadata": [["Opens", url]],
    }

    try:
        answer = fetch_instant_answer(title)
    except Exception as e:
        debug(f"Error fetching instant answer: {e}")
        answer = {}

    text = next(
        (
            answer[key]
            for key in ("AbstractText", "Answer", "Definition")
            if isinstance(answer.get(key), str) and answer[key].strip()
        ),
        "",
    )
    if not text:
        preview["text"] = f'No instant answer for "{title}".'
        return preview

    sections = []
    image = fetch_image(answer.get("Image") or "")
    if image:
        sections.append({"type": "image", "base64": image})
    sections.append({"type": "text", "content": text})
    preview["sections"] = sections

    if answer.get("Heading"):
        preview["title"] = answer["Heading"]
    source = answer.get("AbstractSource") or answer.get("DefinitionSource")
    if source:
        preview["subtitle"] = f"From {source}, Enter opens {engine_label}"
    source_url = answer.get("AbstractURL") or answer.get("DefinitionURL")
    if source_url:
        preview["metadata"].append(["Source", source_url])
    return preview


def preview_main(engine: str) -> None:
    title = os.getenv("RAFFI_TITLE", "")
    url = os.getenv("RAFFI_VALUE") or build_url(engine, title)
    print(json.dumps(build_preview(engine, title, url)))


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

    if os.getenv("RAFFI_PREVIEW") == "1":
        preview_main(engine)
        return

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
