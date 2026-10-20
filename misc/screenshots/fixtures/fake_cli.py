#!/usr/bin/env python3
"""Stand-in for the CLIs Raffi's addons call, symlinked under each name.

Every answer is demo data. Commands it does not know are refused, so a shot
can never act on the real desktop.
"""
import json
import sys
from datetime import datetime, timedelta
from pathlib import Path
from zoneinfo import ZoneInfo


def emit(value):
    print(json.dumps(value, ensure_ascii=False))


def reject():
    print(f"Demo fixture refuses command: {Path(sys.argv[0]).name} {sys.argv[1:]}", file=sys.stderr)
    raise SystemExit(1)


def device(name, description, volume, port, kind):
    return {
        "name": name,
        "description": description,
        "mute": False,
        "volume": {"front-left": {"value_percent": f"{volume}%"}},
        "channel_map": "front-left",
        "active_port": port,
        "ports": [{"name": port, "description": port.replace("-", " ").title(), "type": kind}],
    }


WORKSPACES = [
    {"workspace_id": "w1", "label": "raffi", "agent_status": "working", "focused": True, "pane_count": 4, "active_tab_id": "w1:t1"},
    {"workspace_id": "w2", "label": "website", "agent_status": "idle", "pane_count": 1, "active_tab_id": "w2:t1"},
]
TABS = [
    {"tab_id": "w1:t1", "workspace_id": "w1", "label": "Build the preview pane", "agent_status": "working", "number": 1, "pane_count": 2, "focused": True},
    {"tab_id": "w1:t2", "workspace_id": "w1", "label": "Review keyboard navigation", "agent_status": "blocked", "number": 2, "pane_count": 1},
    {"tab_id": "w1:t3", "workspace_id": "w1", "label": "Shell", "number": 3, "pane_count": 1},
    {"tab_id": "w2:t1", "workspace_id": "w2", "label": "Write feature docs", "agent_status": "idle", "number": 1, "pane_count": 1},
]
PANES = [
    {"pane_id": "w1:p1", "tab_id": "w1:t1", "workspace_id": "w1", "agent": "copilot", "agent_status": "working", "cwd": "/home/demo/projects/raffi", "terminal_title_stripped": "Preview implementation", "number": 1},
    {"pane_id": "w1:p2", "tab_id": "w1:t1", "workspace_id": "w1", "cwd": "/home/demo/projects/raffi", "terminal_title_stripped": "Tests", "number": 2},
    {"pane_id": "w1:p3", "tab_id": "w1:t2", "workspace_id": "w1", "agent": "claude", "agent_status": "blocked", "cwd": "/home/demo/projects/raffi", "terminal_title_stripped": "Waiting for review", "number": 1},
    {"pane_id": "w1:p4", "tab_id": "w1:t3", "workspace_id": "w1", "cwd": "/home/demo/projects/raffi", "number": 1},
    {"pane_id": "w2:p1", "tab_id": "w2:t1", "workspace_id": "w2", "agent": "codex", "agent_status": "idle", "cwd": "/home/demo/projects/website", "terminal_title_stripped": "Feature guide", "number": 1},
]


def pull(number, title, state="SUCCESS", review="APPROVED"):
    return {
        "number": number, "title": title, "url": f"https://github.com/raffi-demo/launcher/pull/{number}",
        "isDraft": False, "headRefName": "preview-pane", "baseRefName": "main",
        "reviewDecision": review, "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
        "additions": 148, "deletions": 23, "changedFiles": 4,
        "updatedAt": "2026-10-19T09:00:00Z", "createdAt": "2026-10-18T15:00:00Z",
        "repository": {"nameWithOwner": "raffi-demo/launcher", "owner": {"login": "raffi-demo", "avatarUrl": ""}},
        "author": {"login": "demo"}, "state": "OPEN",
        "reviewThreads": {"nodes": []},
        "commits": {"totalCount": 3, "nodes": [{"commit": {
            "messageHeadline": "Add keyboard-driven previews",
            "statusCheckRollup": {"state": state, "contexts": {"nodes": [
                {"__typename": "CheckRun", "name": name, "status": "COMPLETED", "conclusion": "SUCCESS", "startedAt": "2026-10-19T09:00:00Z", "completedAt": "2026-10-19T09:01:00Z"}
                for name in ["Rust tests", "Clippy", "Documentation"]
            ]}},
        }}]},
        "latestReviews": {"nodes": [{"author": {"login": "demo-reviewer"}, "state": "APPROVED"}]},
        "reviewRequests": {"nodes": []}, "assignees": {"nodes": []}, "labels": {"nodes": [{"name": "enhancement"}]},
        "autoMergeRequest": None, "milestone": None,
        "body": "## Keyboard-driven previews\n\nBrowse results on the left and inspect the selected item on the right.\n\n- Scroll the preview without leaving the keyboard.\n- Keep the preview matched to the current theme.\n\nDemo data for the Raffi feature gallery.",
        "comments": {"totalCount": 1, "nodes": [{"author": {"__typename": "User", "login": "demo-reviewer"}, "createdAt": "2026-10-19T09:00:00Z", "body": "Keyboard navigation looks good. Ready for another look."}]},
        "closingIssuesReferences": {"nodes": []},
        "files": {"totalCount": 2, "nodes": [
            {"path": "src/ui/wayland/preview.rs", "additions": 120, "deletions": 20, "changeType": "MODIFIED"},
            {"path": "website/src/content/docs/features/ui-modes.mdx", "additions": 28, "deletions": 3, "changeType": "MODIFIED"},
        ]},
    }


CITIES = [
    ("New-York", "America/New_York"),
    ("Bangalore", "Asia/Kolkata"),
    ("Nice", "Europe/Paris"),
    ("London", "Europe/London"),
    ("Tokyo", "Asia/Tokyo"),
]


def batz(query):
    """Timezone comparison for a fixed day, in the shape of batz -j."""
    hour = 10
    for word in query:
        if "h" in word and word.split("h")[0].isdigit():
            hour = int(word.split("h")[0])
    base = datetime(2026, 10, 21, hour, tzinfo=ZoneInfo("Europe/Paris"))
    if "tomorrow" in query:
        base += timedelta(days=1)
    rows = []
    for city, zone in CITIES:
        local = base.astimezone(ZoneInfo(zone))
        offset = local.utcoffset()
        minutes = int(offset.total_seconds() // 60)
        sign = "+" if minutes >= 0 else "-"
        utc = f"UTC{sign}{abs(minutes) // 60}" + (f":{abs(minutes) % 60:02d}" if minutes % 60 else "")
        rows.append((city, zone, local, utc, f"{sign}{abs(minutes) // 60:02d}:{abs(minutes) % 60:02d}"))
    width = max(len(city) for city, *_ in rows) + 2
    lines = [f"\x1b[1;36mTimezone comparison: {base:%Hh%M} {base.tzname()}\x1b[0m", ""]
    for index, (city, zone, local, _, offset) in enumerate(rows):
        bullet = "\x1b[32m\u25cf\x1b[0m" if index == 0 else "\u25cb"
        name = f"\x1b[1;32m{city:<{width}}\x1b[0m" if index == 0 else f"\x1b[1m{city:<{width}}\x1b[0m"
        lines.append(f"  {bullet} {name}{local:%H:%M} {local.tzname():<5} (UTC{offset})")
    lines += ["", f"\x1b[33mDST status:\x1b[0m New-York is {rows[0][2]:%Hh%M} {rows[0][2].tzname()}"]
    preview = "\n".join(lines)
    emit({"items": [
        {"title": f"{city} ({zone})", "subtitle": f"{local:%a %d %B %Hh%M} {local.tzname()} ({utc})",
         "arg": f"{local:%a %d %B %Hh%M} {local.tzname()}", "preview": preview}
        for city, zone, local, utc, _ in rows
    ]})


def main():
    name = Path(sys.argv[0]).name
    args = sys.argv[1:]
    if name == "pactl":
        if args == ["get-default-sink"]:
            print("demo.headphones")
        elif args == ["get-default-source"]:
            print("demo.microphone")
        elif args == ["-f", "json", "list", "sinks"]:
            emit([device("demo.headphones", "Studio headphones", 65, "analog-output-headphones", "headphones"),
                  device("demo.speakers", "Desktop speakers", 40, "analog-output-speaker", "speaker")])
        elif args == ["-f", "json", "list", "sources"]:
            emit([device("demo.microphone", "USB microphone", 80, "analog-input-mic", "mic")])
        else:
            reject()
    elif name == "niri":
        if args == ["msg", "--json", "windows"]:
            emit([
                {"id": index, "title": title, "app_id": app, "workspace_id": workspace, "is_focused": index == 1, "focus_timestamp": {"secs": 100 - index, "nanos": 0}}
                for index, title, app, workspace in [
                    (1, "Raffi documentation", "firefox", 1),
                    (2, "preview.rs - Raffi", "code", 1),
                    (3, "Terminal - feature demo", "kitty", 2),
                    (4, "Files - Documents", "org.gnome.Nautilus", 3),
                ]
            ])
        elif args == ["msg", "--json", "workspaces"]:
            emit([{"id": i, "idx": i, "name": n, "is_focused": i == 1, "output": "Demo"} for i, n in [(1, "Build"), (2, "Terminal"), (3, "Files")]])
        else:
            reject()
    elif name == "playerctl":
        if args == ["-l"]:
            print("DemoPlayer")
        elif args[:3] == ["-a", "metadata", "--format"]:
            print("\x1eDemoPlayer\x1fDemoPlayer\x1fPlaying\x1fEvening light\x1fDemo Quartet\x1fStudio sessions")
        else:
            reject()
    elif name == "bluetoothctl":
        if args == ["show"]:
            print("Controller 00:11:22:33:44:55\n\tAlias: Demo desktop\n\tPowered: yes")
        elif args in (["devices", "Paired"], ["paired-devices"]):
            print("Device 00:11:22:33:44:01 Studio headphones\nDevice 00:11:22:33:44:02 Wireless keyboard\nDevice 00:11:22:33:44:03 Travel earbuds")
        elif len(args) == 2 and args[0] == "info":
            idx = int(args[1][-2:])
            label, icon = [("Studio headphones", "audio-headphones"), ("Wireless keyboard", "input-keyboard"), ("Travel earbuds", "audio-headphones")][idx - 1]
            print(f"Device {args[1]}\n\tName: {label}\n\tAlias: {label}\n\tIcon: {icon}\n\tPaired: yes\n\tConnected: {'yes' if idx < 3 else 'no'}\n\tBattery Percentage: 0x50 (80)")
        else:
            reject()
    elif name == "rbw":
        if args == ["unlocked"]:
            pass
        elif args == ["list", "--raw"]:
            emit([{"id": f"demo-{i}", "name": label, "user": "demo@example.com", "folder": "Demo vault", "uris": [url], "type": "Login"}
                  for i, label, url in [(1, "Code hosting", "https://code.example.com"), (2, "Community forum", "https://forum.example.com"), (3, "Mail", "https://mail.example.com")]])
        elif args[:2] == ["search", "--raw"]:
            emit([])
        else:
            reject()
    elif name == "copyq":
        if len(args) < 2 or args[0] != "eval":
            reject()
        script = args[1]
        texts = [
            "Release checklist\n\nCapture the launcher and preview pane.\nReview every screenshot for private content.\nWrite image descriptions before posting.",
            "cargo test --all-features",
            "https://example.com/raffi-demo",
            "Meeting notes: keyboard shortcuts and rich file previews",
        ]
        if "var stable =" in script:
            emit({"tab": "Clipboard", "total": len(texts), "head": "demo-0", "next": len(texts), "stable": True,
                  "items": [{"r": i, "id": f"demo-{i}", "f": ["text/plain"], "k": "text", "n": len(text), "p": text, "t": 0, "c": "2026-10-19 10:30:00"} for i, text in enumerate(texts)]})
        elif "var maxText =" in script:
            row = int(args[args.index("--") + 3])
            text = texts[row]
            emit({"ok": True, "k": "text", "sizes": [["text/plain", len(text)]], "n": len(text), "text": text, "image": "", "sent": -1})
        else:
            reject()
    elif name == "herdr":
        groups = {"workspace": WORKSPACES, "tab": TABS, "pane": PANES}
        if len(args) == 2 and args[0] in groups and args[1] == "list":
            emit({"result": {args[0] + "s": groups[args[0]]}})
        elif len(args) == 3 and args[0] in groups and args[1] == "get":
            emit({"result": {args[0]: next(item for item in groups[args[0]] if item[args[0] + "_id"] == args[2])}})
        elif args[:2] == ["pane", "read"]:
            print("\x1b[36mDemo session: keyboard previews\x1b[0m\n\nInspecting selected launcher details...\nAdding preview scroll shortcuts.\n\n\x1b[32mPASS\x1b[0m  selection follows keyboard focus\n\x1b[32mPASS\x1b[0m  theme palette matches preview\n\nReady for review.")
        else:
            reject()
    elif name == "batz":
        if args[:1] != ["-j"]:
            reject()
        batz(" ".join(args[1:]).split())
    elif name == "gh":
        if args[:2] != ["api", "graphql"]:
            reject()
        query = next((a[6:] for a in args if a.startswith("query=")), "")
        pulls = [pull(42, "Add keyboard-driven previews"), pull(43, "Browse sessions as connected trees", "PENDING", "REVIEW_REQUIRED"), pull(44, "Add rich file previews")]
        if "pullRequest(number:" in query:
            number = int(next(a[7:] for a in args if a.startswith("number=")))
            emit({"data": {"repository": {"pullRequest": next(p for p in pulls if p["number"] == number)}}})
        elif "pullRequests(states:" in query:
            emit({"data": {"repository": {"pullRequests": {"nodes": pulls}}}})
        else:
            emit({"data": {"search": {"nodes": pulls}}})
    else:
        reject()


if __name__ == "__main__":
    main()
