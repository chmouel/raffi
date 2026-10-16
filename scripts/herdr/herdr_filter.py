#!/usr/bin/env python3
"""Raffi script filter for browsing Herdr agent sessions.

When Raffi runs it with RAFFI_PREVIEW=1, it prints a live preview of the
selected agent (RAFFI_VALUE holds its pane id) instead of the item list.
"""

import json
import os
import re
import subprocess
import sys

ICON = "utilities-terminal"
PREVIEW_REFRESH_SECONDS = 2
# Rough height of one preview text line, used to size the terminal snapshot.
PREVIEW_LINE_HEIGHT_PX = 20
DEFAULT_PREVIEW_LINES = 40

# Raffi renders colour escapes only, so drop cursor movement and OSC titles.
NON_SGR_ESCAPE = re.compile(
    r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-9;?]*[A-Za-ln-z]"
)
TRAILING_BLANK = re.compile(r"(?:\s|\x1b\[[0-9;]*m)+$")
SGR = re.compile(r"\x1b\[[0-9;]*m")


def run_herdr(*args):
    result = subprocess.run(
        ["herdr", *args],
        capture_output=True,
        text=True,
        timeout=2,
        check=True,
    )
    return result.stdout


def herdr_result(stdout):
    payload = json.loads(stdout)
    if not isinstance(payload, dict):
        raise ValueError("Herdr returned an unexpected JSON document")

    result = payload.get("result")
    if not isinstance(result, dict):
        raise ValueError("Herdr response is missing the result object")
    return result


def get_agents():
    result = herdr_result(run_herdr("agent", "list"))

    agents = result.get("agents")
    if not isinstance(agents, list) or any(
        not isinstance(agent, dict) for agent in agents
    ):
        raise ValueError("Herdr response is missing the agents list")
    return agents


def text_field(agent, key, default=""):
    value = agent.get(key)
    return value if isinstance(value, str) and value else default


def agent_fields(agent):
    cwd = text_field(agent, "cwd")
    return (
        text_field(agent, "agent", "?"),
        text_field(agent, "agent_status", "unknown"),
        cwd,
        os.path.basename(cwd.rstrip(os.sep)) or "unknown project",
        text_field(agent, "terminal_title_stripped"),
    )


def build_items(agents, query):
    needle = query.strip().casefold()
    items = []

    for agent in agents:
        pane_id = agent.get("pane_id")
        if not isinstance(pane_id, str) or not pane_id:
            continue

        agent_type, status, cwd, project, terminal_title = agent_fields(agent)

        searchable = " ".join((agent_type, status, project, cwd, terminal_title))
        if needle and needle not in searchable.casefold():
            continue

        items.append(
            {
                "title": f"{agent_type} [{status}] - {project}",
                "subtitle": terminal_title or cwd or "Herdr agent session",
                "arg": pane_id,
            }
        )

    return items


def preview_lines(height):
    try:
        pixels = int(height)
    except (TypeError, ValueError):
        return DEFAULT_PREVIEW_LINES
    return max(10, min(200, pixels // PREVIEW_LINE_HEIGHT_PX))


def clean_terminal_output(text):
    """Keep colours, drop other escapes, padding, and blank edges."""
    text = NON_SGR_ESCAPE.sub("", text.replace("\r", ""))
    lines = [TRAILING_BLANK.sub("", line) for line in text.split("\n")]
    while lines and not SGR.sub("", lines[-1]).strip():
        lines.pop()
    while lines and not SGR.sub("", lines[0]).strip():
        lines.pop(0)
    # Close any colour left open at the end of a trimmed line.
    return "\n".join(f"{line}\x1b[0m" if "\x1b[" in line else line for line in lines)


def build_preview(pane_id, height=None):
    agent = herdr_result(run_herdr("agent", "get", pane_id)).get("agent")
    if not isinstance(agent, dict):
        raise ValueError("Herdr response is missing the agent object")
    agent_type, status, cwd, project, terminal_title = agent_fields(agent)

    output = run_herdr(
        "agent",
        "read",
        pane_id,
        "--source",
        "recent",
        "--lines",
        str(preview_lines(height)),
        "--ansi",
    )

    metadata = [["Agent", agent_type], ["Status", status], ["Pane", pane_id]]
    if cwd:
        metadata.append(["Directory", cwd])
    workspace = text_field(agent, "workspace_id")
    if workspace:
        metadata.append(["Workspace", workspace])

    return {
        "title": f"{agent_type} [{status}] - {project}",
        "subtitle": terminal_title or cwd,
        "badge": "Herdr",
        "icon": ICON,
        "sections": [
            {
                "type": "text",
                "heading": "Terminal",
                "content": clean_terminal_output(output) or "(no output yet)",
            }
        ],
        "metadata": metadata,
        "refresh": PREVIEW_REFRESH_SECONDS,
    }


def preview_main(pane_id, height):
    try:
        preview = build_preview(pane_id, height)
    except (
        FileNotFoundError,
        subprocess.CalledProcessError,
        subprocess.TimeoutExpired,
        ValueError,
    ) as error:
        # Show the problem in the pane and keep retrying, the agent may be
        # starting up or the server restarting.
        preview = {
            "title": pane_id or "Herdr agent",
            "badge": "Herdr",
            "icon": ICON,
            "text": f"Cannot read this agent: {error}",
            "refresh": PREVIEW_REFRESH_SECONDS,
        }
    print(json.dumps(preview, ensure_ascii=False))
    return 0


def main(argv=None):
    if os.environ.get("RAFFI_PREVIEW") == "1":
        return preview_main(
            os.environ.get("RAFFI_VALUE", ""), os.environ.get("RAFFI_PREVIEW_HEIGHT")
        )

    if argv is None:
        argv = sys.argv[1:]
    query = " ".join(argv).strip()

    try:
        items = build_items(get_agents(), query)
    except (
        FileNotFoundError,
        subprocess.CalledProcessError,
        subprocess.TimeoutExpired,
        ValueError,
    ) as error:
        print(f"Herdr script filter: {error}", file=sys.stderr)
        return 1

    print(json.dumps({"items": items}, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
