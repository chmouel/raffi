#!/usr/bin/env python3
"""Raffi script filter for browsing Herdr sessions.

By default it lists workspaces and tabs as an indented tree, with pane
children only for tabs containing multiple panes. The query matches any
of them and keeps the ancestors of each match, so results stay in tree
order. Pass --agents (or export HERDR_SCOPE=agents) to list
only panes hosting a recognized agent, like earlier versions did.
Use --status or HERDR_VIEW=status to group agents by lifecycle status.
These modes are also Raffi views: Ctrl+Tab switches between them, and the
one picked arrives in RAFFI_VIEW, which wins over the flags.

When Raffi runs it with RAFFI_PREVIEW=1, it prints a live preview of the
selected workspace, tab, or pane (RAFFI_VALUE holds its Herdr ID) instead of
the item list.

Actionable items export two variables to actions and preview runs:

- HERDR_KIND: workspace, tab, agent, or pane
- HERDR_FOCUS: the Herdr ID an action should focus. It matches the item's
  arg except for plain shell panes, whose tab is focused instead, because
  Herdr can only focus neighboring panes directly.
"""

import json
import os
import re
import subprocess
import sys
from pathlib import Path

ICON = "utilities-terminal"
ICON_DIR = Path(__file__).resolve().parent / "icons"
ICON_SETS = ("color", "mono-dark", "mono-light")
VIEWS = (
    ("workspace", "Workspaces"),
    ("status", "Status"),
    ("agents", "Agents"),
)
STATUS_GROUPS = (
    ("working", "Working"),
    ("blocked", "Waiting"),
    ("idle", "Idle"),
    ("done", "Done"),
    ("unknown", "Unknown"),
)
AGENT_ICONS = frozenset(
    (
        "pi",
        "claude",
        "codex",
        "gemini",
        "cursor",
        "devin",
        "agy",
        "cline",
        "omp",
        "mastracode",
        "opencode",
        "copilot",
        "kimi",
        "kiro",
        "droid",
        "amp",
        "grok",
        "hermes",
        "kilo",
        "qodercli",
        "qwen",
        "letta",
        "maki",
        "muse",
    )
)
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

# ANSI colours for agent lifecycle states in item titles.
STATUS_COLORS = {
    "working": "\x1b[32m",
    "blocked": "\x1b[31m",
    "done": "\x1b[33m",
    "idle": "\x1b[36m",
}
UNKNOWN_COLOR = "\x1b[90m"

# Herdr injects HERDR_PANE_ID and friends into the panes it manages; these
# variable names do not clash with them.
KIND_VARIABLE = "HERDR_KIND"
FOCUS_VARIABLE = "HERDR_FOCUS"


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


def herdr_list(command, key):
    entries = herdr_result(run_herdr(*command)).get(key)
    if not isinstance(entries, list) or any(
        not isinstance(entry, dict) for entry in entries
    ):
        raise ValueError(f"Herdr response is missing the {key} list")
    return entries


def herdr_object(command, key):
    value = herdr_result(run_herdr(*command)).get(key)
    if not isinstance(value, dict):
        raise ValueError(f"Herdr response is missing the {key} object")
    return value


def get_workspaces():
    return herdr_list(("workspace", "list"), "workspaces")


def get_tabs():
    return herdr_list(("tab", "list"), "tabs")


def get_panes():
    return herdr_list(("pane", "list"), "panes")


def get_agents():
    return herdr_list(("agent", "list"), "agents")


def text_field(entry, key, default=""):
    value = entry.get(key)
    return value if isinstance(value, str) and value else default


def int_field(entry, key, default=0):
    value = entry.get(key)
    return value if isinstance(value, int) and not isinstance(value, bool) else default


def number_of(entry):
    return int_field(entry, "number")


def project_of(cwd):
    return os.path.basename(cwd.rstrip(os.sep)) or "unknown project"


def status_span(status):
    color = STATUS_COLORS.get(status, UNKNOWN_COLOR)
    return f"{color}[{status}]\x1b[0m"


def count(noun, total):
    return f"{total} {noun}{'' if total == 1 else 's'}"


def focused_prefix(entry):
    return "focused · " if entry.get("focused") else ""


def pane_details(pane):
    """(agent or '', status, cwd, project, terminal title) of a pane."""
    cwd = text_field(pane, "cwd")
    return (
        text_field(pane, "agent"),
        text_field(pane, "agent_status", "unknown"),
        cwd,
        project_of(cwd),
        text_field(pane, "terminal_title_stripped"),
    )


def icon_path(name):
    preset = os.environ.get("HERDR_ICON_SET", "color").strip()
    if preset not in ICON_SETS:
        raise ValueError(
            f"Unknown HERDR_ICON_SET {preset!r}; choose {', '.join(ICON_SETS)}"
        )
    path = ICON_DIR / preset / f"{name}.svg"
    if not path.is_file():
        raise ValueError(
            f"Missing Herdr icon {path}; "
            "copy the icons directory beside herdr_filter.py"
        )
    return str(path)


def pane_icon(pane):
    agent = text_field(pane, "agent")
    if not agent:
        return icon_path("shell")
    return icon_path(f"agent-{agent}" if agent in AGENT_ICONS else "agent-generic")


def tab_icon(panes):
    return pane_icon(panes[0]) if len(panes) == 1 else icon_path("session")


def workspace_item(workspace, tabs, pane_total):
    workspace_id = text_field(workspace, "workspace_id")
    label = text_field(workspace, "label") or workspace_id
    status = text_field(workspace, "agent_status", "unknown")
    return {
        "title": f"{label} {status_span(status)}",
        "subtitle": focused_prefix(workspace)
        + " · ".join((count("tab", len(tabs)), count("pane", pane_total))),
        "arg": workspace_id,
        "indent": 0,
        "icon": {"path": icon_path("workspace")},
        "variables": {KIND_VARIABLE: "workspace", FOCUS_VARIABLE: workspace_id},
    }


def tab_item(tab, panes):
    tab_id = text_field(tab, "tab_id")
    label = text_field(tab, "label") or tab_id
    status = text_field(tab, "agent_status", "unknown")
    occupants = (
        ", ".join(text_field(pane, "agent") or "shell" for pane in panes) or "empty"
    )
    return {
        "title": f"{label} {status_span(status)}",
        "subtitle": focused_prefix(tab) + occupants,
        "arg": tab_id,
        "indent": 1,
        "icon": {"path": tab_icon(panes)},
        "variables": {KIND_VARIABLE: "tab", FOCUS_VARIABLE: tab_id},
    }


def pane_item(pane):
    pane_id = text_field(pane, "pane_id")
    agent, status, cwd, project, terminal_title = pane_details(pane)
    if agent:
        title = f"{agent} {status_span(status)} - {project}"
        kind, focus = "agent", pane_id
    else:
        title = f"shell - {project}"
        # Herdr focuses panes only by direction, so bring up the tab instead.
        kind, focus = "pane", text_field(pane, "tab_id") or pane_id
    return {
        "title": title,
        "subtitle": terminal_title or cwd or "Herdr pane",
        "arg": pane_id,
        "indent": 2,
        "icon": {"path": pane_icon(pane)},
        "variables": {KIND_VARIABLE: kind, FOCUS_VARIABLE: focus},
    }


def pane_node(pane):
    agent, status, cwd, project, terminal_title = pane_details(pane)
    return {
        "item": pane_item(pane),
        "searchable": " ".join(
            (agent or "shell", status, project, cwd, terminal_title)
        ),
        "children": [],
    }


def tree_nodes(workspaces, tabs, panes):
    """Workspace nodes with tab and pane children, in workspace order."""
    panes_by_tab = {}
    for pane in panes:
        if text_field(pane, "pane_id"):
            panes_by_tab.setdefault(text_field(pane, "tab_id"), []).append(pane)
    tabs_by_workspace = {}
    for tab in tabs:
        if text_field(tab, "tab_id"):
            tabs_by_workspace.setdefault(text_field(tab, "workspace_id"), []).append(
                tab
            )

    nodes = []
    for workspace in sorted(workspaces, key=number_of):
        workspace_id = text_field(workspace, "workspace_id")
        if not workspace_id:
            continue
        workspace_tabs = sorted(tabs_by_workspace.pop(workspace_id, []), key=number_of)
        tab_nodes = []
        pane_total = 0
        for tab in workspace_tabs:
            tab_id = text_field(tab, "tab_id")
            tab_panes = panes_by_tab.pop(tab_id, [])
            pane_total += len(tab_panes)
            label = text_field(tab, "label") or tab_id
            children = [pane_node(pane) for pane in tab_panes]
            searchable = " ".join(
                (label, tab_id, text_field(tab, "agent_status", "unknown"))
            )
            if len(children) == 1:
                searchable += " " + children[0]["searchable"]
                children = []
            tab_nodes.append(
                {
                    "item": tab_item(tab, tab_panes),
                    "searchable": searchable,
                    "children": children,
                }
            )
        label = text_field(workspace, "label") or workspace_id
        nodes.append(
            {
                "item": workspace_item(workspace, workspace_tabs, pane_total),
                "searchable": " ".join(
                    (
                        label,
                        workspace_id,
                        text_field(workspace, "agent_status", "unknown"),
                    )
                ),
                "children": tab_nodes,
            }
        )
    return nodes


def collect(node, needle):
    """Items of node and the descendants kept by the query, in tree order.

    A matching node keeps all of its descendants; a non-matching one stays
    as an ancestor only while some descendant matches.
    """
    matched = not needle or needle in node["searchable"].casefold()
    kept = []
    for child in node["children"]:
        kept.extend(collect(child, "" if matched else needle))
    if not matched and not kept:
        return []
    return [node["item"], *kept]


def build_tree_items(workspaces, tabs, panes, query):
    needle = query.strip().casefold()
    items = []
    for node in tree_nodes(workspaces, tabs, panes):
        items.extend(collect(node, needle))
    return items


def build_agent_items(agents, query):
    needle = query.strip().casefold()
    items = []

    for agent in agents:
        pane_id = text_field(agent, "pane_id")
        if not pane_id:
            continue

        agent_type, status, cwd, project, terminal_title = pane_details(agent)

        searchable = " ".join((agent_type, status, project, cwd, terminal_title))
        if needle and needle not in searchable.casefold():
            continue

        items.append(
            {
                "title": f"{agent_type} {status_span(status)} - {project}",
                "subtitle": terminal_title or cwd or "Herdr agent session",
                "arg": pane_id,
                "icon": {"path": pane_icon(agent)},
                "variables": {KIND_VARIABLE: "agent", FOCUS_VARIABLE: pane_id},
            }
        )

    return items


def build_status_items(workspaces, tabs, panes, query):
    needle = query.strip().casefold()
    workspace_map = {
        text_field(workspace, "workspace_id"): (index, workspace)
        for index, workspace in enumerate(sorted(workspaces, key=number_of))
        if text_field(workspace, "workspace_id")
    }
    tab_map = {
        text_field(tab, "tab_id"): (index, tab)
        for index, tab in enumerate(sorted(tabs, key=number_of))
        if text_field(tab, "tab_id")
    }
    tab_counts = {}
    for pane in panes:
        if text_field(pane, "pane_id"):
            tab_id = text_field(pane, "tab_id")
            tab_counts[tab_id] = tab_counts.get(tab_id, 0) + 1

    groups = {status: [] for status, _ in STATUS_GROUPS}
    seen = set()
    for index, pane in enumerate(panes):
        pane_id = text_field(pane, "pane_id")
        agent, status, cwd, project, terminal_title = pane_details(pane)
        if not pane_id or not agent or pane_id in seen:
            continue
        seen.add(pane_id)
        tab_id = text_field(pane, "tab_id")
        tab_order, tab = tab_map.get(tab_id, (len(tabs), {}))
        workspace_id = text_field(pane, "workspace_id") or text_field(
            tab, "workspace_id"
        )
        workspace_order, workspace = workspace_map.get(
            workspace_id, (len(workspaces), {})
        )
        workspace_label = text_field(workspace, "label") or workspace_id or project
        tab_label = text_field(tab, "label") or tab_id or terminal_title or project
        group = status if status in groups else "unknown"
        group_label = dict(STATUS_GROUPS)[group]
        searchable = " ".join(
            (
                workspace_label,
                workspace_id,
                tab_label,
                tab_id,
                agent,
                status,
                group_label,
                cwd,
                project,
                terminal_title,
            )
        )
        if needle and needle not in searchable.casefold():
            continue
        item = pane_item(pane)
        title = tab_label
        if tab_counts.get(tab_id, 0) > 1:
            title += f" / {agent} ({pane_id})"
        item["title"] = title
        item["subtitle"] = " · ".join(
            (workspace_label, agent, group_label if group != "unknown" else status)
        )
        item["indent"] = 1
        groups[group].append(((workspace_order, tab_order, index), item))

    items = []
    for status, label in STATUS_GROUPS:
        members = [
            item for _, item in sorted(groups[status], key=lambda entry: entry[0])
        ]
        if not members:
            continue
        symbol = "waiting" if status == "blocked" else status
        items.append(
            {
                "title": label,
                "subtitle": count("agent", len(members)),
                "arg": f"status:{status}",
                "valid": False,
                "indent": 0,
                "icon": {"path": icon_path(f"status-{symbol}")},
                "preview": {
                    "dynamic": False,
                    "title": label,
                    "subtitle": count("agent", len(members)),
                    "icon": icon_path(f"status-{symbol}"),
                    "text": "\n".join(
                        f"{item['title']} · {item['subtitle']}" for item in members
                    ),
                },
            }
        )
        items.extend(members)
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


def terminal_section(pane_id, height):
    output = run_herdr(
        "pane",
        "read",
        pane_id,
        "--source",
        "recent",
        "--lines",
        str(preview_lines(height)),
        "--ansi",
    )
    return {
        "type": "text",
        "heading": "Terminal",
        "content": clean_terminal_output(output) or "(no output yet)",
    }


def build_pane_preview(pane_id, height):
    pane = herdr_object(("pane", "get", pane_id), "pane")
    agent, status, cwd, project, terminal_title = pane_details(pane)

    metadata = [["Pane", pane_id]]
    if agent:
        metadata = [["Agent", agent], ["Status", status]] + metadata
    if cwd:
        metadata.append(["Directory", cwd])
    workspace = text_field(pane, "workspace_id")
    if workspace:
        metadata.append(["Workspace", workspace])
    tab = text_field(pane, "tab_id")
    if tab:
        metadata.append(["Tab", tab])

    title = f"{agent} [{status}] - {project}" if agent else f"shell - {project}"
    return {
        "title": title,
        "subtitle": terminal_title or cwd,
        "badge": "Herdr",
        "icon": pane_icon(pane),
        "sections": [terminal_section(pane_id, height)],
        "metadata": metadata,
        "refresh": PREVIEW_REFRESH_SECONDS,
    }


def build_tab_preview(tab_id, height):
    tab = herdr_object(("tab", "get", tab_id), "tab")
    panes = [pane for pane in get_panes() if text_field(pane, "tab_id") == tab_id]

    lines = []
    for pane in panes:
        agent, status, cwd, project, terminal_title = pane_details(pane)
        lines.append(
            f"{agent or 'shell'} [{status}] - {project}"
            + (f" · {terminal_title}" if terminal_title else "")
        )

    sections = [
        {
            "type": "text",
            "heading": "Panes",
            "content": "\n".join(lines) or "(no panes)",
        }
    ]
    # A single-pane tab is usually opened for its pane: show its terminal.
    if len(panes) == 1:
        sections.append(terminal_section(text_field(panes[0], "pane_id"), height))

    metadata = [
        ["Status", text_field(tab, "agent_status", "unknown")],
        ["Tab", tab_id],
        ["Panes", str(len(panes))],
    ]
    workspace = text_field(tab, "workspace_id")
    if workspace:
        metadata.append(["Workspace", workspace])

    label = text_field(tab, "label") or tab_id
    return {
        "title": label,
        "subtitle": count("pane", len(panes)),
        "badge": "Herdr",
        "icon": tab_icon(panes),
        "sections": sections,
        "metadata": metadata,
        "refresh": PREVIEW_REFRESH_SECONDS,
    }


def build_workspace_preview(workspace_id, height):
    workspace = herdr_object(("workspace", "get", workspace_id), "workspace")
    tabs = sorted(
        (tab for tab in get_tabs() if text_field(tab, "workspace_id") == workspace_id),
        key=number_of,
    )

    lines = []
    for tab in tabs:
        label = text_field(tab, "label") or text_field(tab, "tab_id")
        status = text_field(tab, "agent_status", "unknown")
        marker = "● " if tab.get("focused") else "  "
        lines.append(
            f"{marker}{label} [{status}] - {count('pane', int_field(tab, 'pane_count'))}"
        )

    metadata = [
        ["Status", text_field(workspace, "agent_status", "unknown")],
        ["Tabs", str(len(tabs))],
        ["Panes", str(int_field(workspace, "pane_count"))],
    ]
    active_tab = text_field(workspace, "active_tab_id")
    if active_tab:
        metadata.append(["Active tab", active_tab])

    label = text_field(workspace, "label") or workspace_id
    return {
        "title": label,
        "subtitle": " · ".join(
            (count("tab", len(tabs)), count("pane", int_field(workspace, "pane_count")))
        ),
        "badge": "Herdr",
        "icon": icon_path("workspace"),
        "sections": [
            {
                "type": "text",
                "heading": "Tabs",
                "content": "\n".join(lines) or "(no tabs)",
            }
        ],
        "metadata": metadata,
        "refresh": PREVIEW_REFRESH_SECONDS,
    }


def preview_kind(value):
    """What RAFFI_VALUE points at: the item variable, else the ID shape."""
    kind = os.environ.get(KIND_VARIABLE, "").strip().lower()
    if kind in ("workspace", "tab", "agent", "pane"):
        return kind
    # Public Herdr IDs: workspace w1, tab w1:t1, pane w1:p1.
    if ":p" in value:
        return "pane"
    if ":t" in value:
        return "tab"
    return "workspace"


def build_preview(value, height):
    if value.startswith("status:"):
        status = value.removeprefix("status:")
        label = dict(STATUS_GROUPS).get(status)
        if label is None:
            raise ValueError(f"Unknown status group {status!r}")
        for item in build_status_items(
            get_workspaces(), get_tabs(), get_panes(), os.environ.get("RAFFI_QUERY", "")
        ):
            if item.get("arg") == value:
                return {**item["preview"], "refresh": PREVIEW_REFRESH_SECONDS}
        return {
            "title": label,
            "text": "No agents in this group",
            "refresh": PREVIEW_REFRESH_SECONDS,
        }
    kind = preview_kind(value)
    if kind == "workspace":
        return build_workspace_preview(value, height)
    if kind == "tab":
        return build_tab_preview(value, height)
    return build_pane_preview(value, height)


def preview_main(value, height):
    try:
        icon_path("workspace")
        preview = build_preview(value, height)
    except (
        FileNotFoundError,
        subprocess.CalledProcessError,
        subprocess.TimeoutExpired,
        ValueError,
    ) as error:
        # Show the problem in the pane and keep retrying, the session may be
        # starting up or the server restarting.
        preview = {
            "title": value or "Herdr",
            "badge": "Herdr",
            "icon": ICON,
            "text": f"Cannot read this session: {error}",
            "refresh": PREVIEW_REFRESH_SECONDS,
        }
    print(json.dumps(preview, ensure_ascii=False))
    return 0


def parse_argv(argv):
    """(view mode, query) from raw argv; flags may sit anywhere."""
    mode = os.environ.get("HERDR_VIEW", "workspace").strip().lower()
    if mode not in ("workspace", "status"):
        raise ValueError("HERDR_VIEW must be workspace or status")
    if os.environ.get("HERDR_SCOPE", "").strip().lower() == "agents":
        mode = "agents"
    words = []
    for arg in argv:
        if arg == "--agents":
            mode = "agents"
        elif arg == "--tree":
            mode = "workspace"
        elif arg == "--status":
            mode = "status"
        else:
            words.append(arg)
    view = os.environ.get("RAFFI_VIEW", "")
    if view in dict(VIEWS):
        mode = view
    return mode, " ".join(words).strip()


def main(argv=None):
    if os.environ.get("RAFFI_PREVIEW") == "1":
        return preview_main(
            os.environ.get("RAFFI_VALUE", ""), os.environ.get("RAFFI_PREVIEW_HEIGHT")
        )

    if argv is None:
        argv = sys.argv[1:]
    try:
        mode, query = parse_argv(argv)
        icon_path("workspace")
        if mode == "agents":
            items = build_agent_items(get_agents(), query)
        else:
            build = build_status_items if mode == "status" else build_tree_items
            items = build(get_workspaces(), get_tabs(), get_panes(), query)
    except (
        FileNotFoundError,
        subprocess.CalledProcessError,
        subprocess.TimeoutExpired,
        ValueError,
    ) as error:
        print(f"Herdr script filter: {error}", file=sys.stderr)
        return 1

    views = [{"id": view, "title": title} for view, title in VIEWS]
    output = {"views": views, "view": mode, "items": items}
    print(json.dumps(output, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
