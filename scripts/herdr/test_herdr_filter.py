import io
import json
import os
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


class HerdrPreviewTests(unittest.TestCase):
    def fake_herdr(self, *args):
        if args[:2] == ("agent", "get"):
            return json.dumps(
                {"result": {"agent": dict(SAMPLE_AGENTS[0], workspace_id="w1")}}
            )
        if args[:2] == ("agent", "read"):
            return "\x1b]0;title\x07\n\x1b[32mdone\x1b[0m    \r\nnext line   \n\n"
        raise AssertionError(args)

    def test_preview_lines_follow_pane_height(self):
        self.assertEqual(herdr_filter.preview_lines("600"), 30)
        self.assertEqual(herdr_filter.preview_lines("50"), 10)
        self.assertEqual(herdr_filter.preview_lines("100000"), 200)
        self.assertEqual(
            herdr_filter.preview_lines(None), herdr_filter.DEFAULT_PREVIEW_LINES
        )

    def test_clean_terminal_output_keeps_colours_only(self):
        cleaned = herdr_filter.clean_terminal_output(
            "\n  \n\x1b[2J\x1b[31mred\x1b[0m   \x1b[0m\r\nplain   \n\x1b[0m  \n"
        )
        self.assertEqual(cleaned, "\x1b[31mred\x1b[0m\nplain")

    def test_build_preview_reads_agent_and_terminal(self):
        with patch.object(
            herdr_filter, "run_herdr", side_effect=self.fake_herdr
        ) as run:
            preview = herdr_filter.build_preview("w1:p1", "400")

        run.assert_any_call(
            "agent", "read", "w1:p1", "--source", "recent", "--lines", "20", "--ansi"
        )
        self.assertEqual(preview["title"], "claude [working] - project-a")
        self.assertEqual(preview["subtitle"], "implement feature X")
        self.assertEqual(preview["refresh"], herdr_filter.PREVIEW_REFRESH_SECONDS)
        self.assertEqual(
            preview["sections"][0]["content"], "\x1b[32mdone\x1b[0m\nnext line"
        )
        self.assertIn(["Status", "working"], preview["metadata"])
        self.assertIn(["Workspace", "w1"], preview["metadata"])

    def test_main_prints_preview_when_raffi_asks(self):
        stdout = io.StringIO()
        env = {"RAFFI_PREVIEW": "1", "RAFFI_VALUE": "w1:p1"}
        with (
            patch.dict(os.environ, env),
            patch.object(herdr_filter, "run_herdr", side_effect=self.fake_herdr),
            redirect_stdout(stdout),
        ):
            result = herdr_filter.main(["ignored query"])

        self.assertEqual(result, 0)
        payload = json.loads(stdout.getvalue())
        self.assertNotIn("items", payload)
        self.assertEqual(payload["badge"], "Herdr")

    def test_preview_failure_is_shown_in_the_pane(self):
        stdout = io.StringIO()
        error = subprocess.CalledProcessError(1, ["herdr"])
        with (
            patch.object(herdr_filter, "run_herdr", side_effect=error),
            redirect_stdout(stdout),
        ):
            result = herdr_filter.preview_main("w1:p9", None)

        self.assertEqual(result, 0)
        payload = json.loads(stdout.getvalue())
        self.assertIn("Cannot read this agent", payload["text"])
        self.assertEqual(payload["refresh"], herdr_filter.PREVIEW_REFRESH_SECONDS)


if __name__ == "__main__":
    unittest.main()
