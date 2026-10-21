#!/usr/bin/env python3
"""Listing-only stand-ins for two browsers connected to Raffi's tabs socket.

Usage: browser_host.py SOCKET_DIR
"""
import json
import socket
import sys
import threading
import time
from pathlib import Path

NOW_MS = time.time() * 1000


def tabs(rows, minutes_ago):
    """Tabs last used one minute apart, the first `minutes_ago` minutes ago."""
    return [
        {
            "id": i,
            "title": title,
            "url": url,
            "lastAccessed": NOW_MS - (minutes_ago + i) * 60_000,
            "pinned": pinned,
        }
        for i, (title, url, pinned) in enumerate(rows, start=1)
    ]


BROWSERS = {
    "demo.sock": (
        {"id": "firefox", "name": "Firefox"},
        tabs(
            [
                ("Raffi - keyboard-first launcher", "https://raffi.example.com/guide", True),
                ("Preview pane implementation", "https://code.example.com/demo/preview", False),
                ("Community discussion", "https://forum.example.com/keyboard", False),
                ("Release checklist", "https://notes.example.com/raffi", False),
            ],
            0,
        ),
    ),
    "chromium.sock": (
        {"id": "chromium", "name": "Chromium"},
        tabs(
            [
                ("Team calendar", "https://calendar.example.com/week", True),
                ("Rust release notes", "https://blog.example.com/rust", False),
                ("Wayland protocols", "https://wayland.example.com/protocols", False),
            ],
            30,
        ),
    ),
}


def serve(server, browser, listing):
    while True:
        connection, _ = server.accept()
        with connection:
            data = b""
            while not data.endswith(b"\n"):
                chunk = connection.recv(8192)
                if not chunk:
                    break
                data += chunk
            if not data:
                continue
            request = json.loads(data)
            response = (
                {"listing": {"browser": browser, "tabs": listing}}
                if request.get("method") == "list"
                else {"error": "Listing-only demo fixture"}
            )
            connection.sendall(json.dumps(response).encode() + b"\n")


def main():
    directory = Path(sys.argv[1])
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    threads = []
    for name, (browser, listing) in BROWSERS.items():
        path = directory / name
        path.unlink(missing_ok=True)
        server = socket.socket(socket.AF_UNIX)
        server.bind(str(path))
        path.chmod(0o600)
        server.listen()
        threads.append(threading.Thread(target=serve, args=(server, browser, listing), daemon=True))
    for thread in threads:
        thread.start()
    print("ready", flush=True)
    for thread in threads:
        thread.join()


if __name__ == "__main__":
    main()
