#!/usr/bin/env python3
"""Listing-only stand-in for a browser connected to Raffi's tabs socket.

Usage: browser_host.py SOCKET_DIR
"""
import json
import socket
import sys
from pathlib import Path

TABS = [
    {"id": i, "title": title, "url": url, "lastAccessed": 100 - i, "pinned": i == 1}
    for i, title, url in [
        (1, "Raffi - keyboard-first launcher", "https://raffi.example.com/guide"),
        (2, "Preview pane implementation", "https://code.example.com/demo/preview"),
        (3, "Community discussion", "https://forum.example.com/keyboard"),
        (4, "Release checklist", "https://notes.example.com/raffi"),
    ]
]


def main():
    directory = Path(sys.argv[1])
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    path = directory / "demo.sock"
    path.unlink(missing_ok=True)
    with socket.socket(socket.AF_UNIX) as server:
        server.bind(str(path))
        path.chmod(0o600)
        server.listen()
        print("ready", flush=True)
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
                    {"listing": {"browser": {"id": "firefox", "name": "Firefox"}, "tabs": TABS}}
                    if request.get("method") == "list"
                    else {"error": "Listing-only demo fixture"}
                )
                connection.sendall(json.dumps(response).encode() + b"\n")


if __name__ == "__main__":
    main()
