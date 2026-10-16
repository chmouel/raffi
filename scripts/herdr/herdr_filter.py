#!/usr/bin/env python3
"""Raffi script filter for browsing Herdr agent sessions."""

import json
import os
import subprocess
import sys


def get_agents():
    result = subprocess.run(
        ["herdr", "agent", "list"],
        capture_output=True,
        text=True,
        timeout=2,
        check=True,
    )
    payload = json.loads(result.stdout)
    if not isinstance(payload, dict):
        raise ValueError("Herdr returned an unexpected JSON document")

    result = payload.get("result")
    if not isinstance(result, dict):
        raise ValueError("Herdr response is missing the result object")

    agents = result.get("agents")
    if not isinstance(agents, list) or any(
        not isinstance(agent, dict) for agent in agents
    ):
        raise ValueError("Herdr response is missing the agents list")
    return agents


def build_items(agents, query):
    needle = query.strip().casefold()
    items = []

    for agent in agents:
        pane_id = agent.get("pane_id")
        if not isinstance(pane_id, str) or not pane_id:
            continue

        agent_type = agent.get("agent")
        if not isinstance(agent_type, str) or not agent_type:
            agent_type = "?"

        status = agent.get("agent_status")
        if not isinstance(status, str) or not status:
            status = "unknown"

        cwd = agent.get("cwd")
        if not isinstance(cwd, str):
            cwd = ""
        project = os.path.basename(cwd.rstrip(os.sep)) or "unknown project"

        terminal_title = agent.get("terminal_title_stripped")
        if not isinstance(terminal_title, str):
            terminal_title = ""

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


def main(argv=None):
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
