import io
import json
import subprocess
import unittest
from contextlib import redirect_stderr, redirect_stdout
from unittest.mock import patch

import herdr_filter


SAMPLE_AGENTS = [
    {
        "agent": "claude",
        "agent_status": "working",
        "cwd": "/home/user/project-a",
        "pane_id": "w1:p1",
        "terminal_title_stripped": "implement feature X",
    },
    {
        "agent": "pi",
        "agent_status": "blocked",
        "cwd": "/home/user/project-b",
        "pane_id": "w1:p2",
        "terminal_title_stripped": "review PR",
    },
    {
        "agent": "codex",
        "agent_status": "idle",
        "cwd": "/home/user/project-c",
        "pane_id": "w1:p3",
        "terminal_title_stripped": "",
    },
]


class HerdrFilterTests(unittest.TestCase):
    def test_get_agents_calls_herdr_and_parses_response(self):
        response = {"result": {"agents": SAMPLE_AGENTS}}
        completed = subprocess.CompletedProcess(
            ["herdr", "agent", "list"], 0, json.dumps(response), ""
        )

        with patch.object(
            herdr_filter.subprocess, "run", return_value=completed
        ) as run:
            self.assertEqual(herdr_filter.get_agents(), SAMPLE_AGENTS)

        run.assert_called_once_with(
            ["herdr", "agent", "list"],
            capture_output=True,
            text=True,
            timeout=2,
            check=True,
        )

    def test_build_items_preserves_focus_id_and_session_details(self):
        items = herdr_filter.build_items(SAMPLE_AGENTS, "")

        self.assertEqual(len(items), 3)
        self.assertEqual(items[0]["title"], "claude [working] - project-a")
        self.assertEqual(items[0]["subtitle"], "implement feature X")
        self.assertEqual(items[0]["arg"], "w1:p1")

    def test_query_matches_agent_status_project_and_terminal_title(self):
        cases = {
            "CLAUDE": "w1:p1",
            "blocked": "w1:p2",
            "project-c": "w1:p3",
            "review PR": "w1:p2",
        }
        for query, pane_id in cases.items():
            with self.subTest(query=query):
                items = herdr_filter.build_items(SAMPLE_AGENTS, query)
                self.assertEqual([item["arg"] for item in items], [pane_id])

    def test_items_without_pane_id_are_not_selectable(self):
        items = herdr_filter.build_items(
            [{"agent": "claude", "agent_status": "working"}], ""
        )

        self.assertEqual(items, [])

    def test_empty_agent_list_outputs_empty_items(self):
        stdout = io.StringIO()
        with patch.object(herdr_filter, "get_agents", return_value=[]):
            with redirect_stdout(stdout):
                result = herdr_filter.main([])

        self.assertEqual(result, 0)
        self.assertEqual(json.loads(stdout.getvalue()), {"items": []})

    def test_command_failure_is_reported(self):
        stderr = io.StringIO()
        with patch.object(
            herdr_filter, "get_agents", side_effect=FileNotFoundError("herdr")
        ):
            with redirect_stderr(stderr):
                result = herdr_filter.main([])

        self.assertEqual(result, 1)
        self.assertIn("Herdr script filter:", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
