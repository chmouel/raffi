import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest.mock import patch

import generate_icons
import herdr_filter


SAMPLE_WORKSPACES = [
    {
        "workspace_id": "w1",
        "label": "alpha",
        "number": 1,
        "agent_status": "working",
        "focused": True,
        "tab_count": 2,
        "pane_count": 3,
        "active_tab_id": "w1:t1",
    },
    {
        "workspace_id": "w2",
        "label": "beta",
        "number": 2,
        "agent_status": "idle",
        "focused": False,
        "tab_count": 1,
        "pane_count": 1,
        "active_tab_id": "w2:t9",
    },
]

SAMPLE_TABS = [
    {
        "tab_id": "w1:t1",
        "workspace_id": "w1",
        "label": "Feature",
        "number": 10,
        "pane_count": 2,
        "agent_status": "working",
        "focused": True,
    },
    {
        "tab_id": "w1:t2",
        "workspace_id": "w1",
        "label": "Shell stuff",
        "number": 20,
        "pane_count": 1,
        "agent_status": "unknown",
        "focused": False,
    },
    {
        "tab_id": "w2:t9",
        "workspace_id": "w2",
        "label": "Solo",
        "number": 1,
        "pane_count": 1,
        "agent_status": "idle",
        "focused": False,
    },
]

SAMPLE_PANES = [
    {
        "pane_id": "w1:p1",
        "workspace_id": "w1",
        "tab_id": "w1:t1",
        "agent": "claude",
        "agent_status": "working",
        "cwd": "/home/user/project-a",
        "terminal_title_stripped": "implement feature X",
        "focused": True,
    },
    {
        "pane_id": "w1:p2",
        "workspace_id": "w1",
        "tab_id": "w1:t1",
        "agent": "pi",
        "agent_status": "blocked",
        "cwd": "/home/user/project-b",
        "terminal_title_stripped": "review PR",
    },
    {
        "pane_id": "w1:p3",
        "workspace_id": "w1",
        "tab_id": "w1:t2",
        "agent_status": "unknown",
        "cwd": "/home/user/project-c",
        "terminal_title_stripped": "chmouel@host: ~/project-c",
    },
    {
        "pane_id": "w2:p9",
        "workspace_id": "w2",
        "tab_id": "w2:t9",
        "agent": "codex",
        "agent_status": "idle",
        "cwd": "/home/user/project-d",
        "terminal_title_stripped": "",
    },
]

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


def tree_items(query=""):
    return herdr_filter.build_tree_items(
        SAMPLE_WORKSPACES, SAMPLE_TABS, SAMPLE_PANES, query
    )


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

    def test_tree_lists_workspaces_tabs_and_panes_indented(self):
        items = tree_items()

        self.assertEqual(
            [(item["arg"], item["indent"]) for item in items],
            [
                ("w1", 0),
                ("w1:t1", 1),
                ("w1:p1", 2),
                ("w1:p2", 2),
                ("w1:t2", 1),
                ("w2", 0),
                ("w2:t9", 1),
            ],
        )
        self.assertEqual(items[0]["title"], "alpha \x1b[32m[working]\x1b[0m")
        self.assertEqual(items[0]["subtitle"], "focused · 2 tabs · 3 panes")
        self.assertEqual(items[1]["title"], "Feature \x1b[32m[working]\x1b[0m")
        self.assertEqual(items[1]["subtitle"], "focused · claude, pi")
        self.assertEqual(items[2]["title"], "claude \x1b[32m[working]\x1b[0m - project-a")
        self.assertEqual(items[2]["subtitle"], "implement feature X")
        self.assertEqual(items[4]["title"], "Shell stuff \x1b[90m[unknown]\x1b[0m")
        self.assertEqual(items[4]["subtitle"], "shell")
        self.assertEqual(items[6]["title"], "Solo \x1b[36m[idle]\x1b[0m")
        self.assertEqual(items[6]["subtitle"], "codex")

    def test_tree_items_carry_kind_and_focus_variables(self):
        items = {item["arg"]: item for item in tree_items()}

        self.assertEqual(
            items["w1"]["variables"],
            {"HERDR_KIND": "workspace", "HERDR_FOCUS": "w1"},
        )
        self.assertEqual(
            items["w1:t1"]["variables"], {"HERDR_KIND": "tab", "HERDR_FOCUS": "w1:t1"}
        )
        self.assertEqual(
            items["w1:p1"]["variables"], {"HERDR_KIND": "agent", "HERDR_FOCUS": "w1:p1"}
        )
        self.assertEqual(
            items["w1:t2"]["variables"], {"HERDR_KIND": "tab", "HERDR_FOCUS": "w1:t2"}
        )
        self.assertEqual(
            items["w2:t9"]["variables"], {"HERDR_KIND": "tab", "HERDR_FOCUS": "w2:t9"}
        )

    def test_tree_query_keeps_ancestors_and_prunes_siblings(self):
        items = tree_items("review")

        self.assertEqual([item["arg"] for item in items], ["w1", "w1:t1", "w1:p2"])

    def test_tree_query_matching_workspace_keeps_whole_subtree(self):
        items = tree_items("beta")

        self.assertEqual(
            [item["arg"] for item in items], ["w2", "w2:t9"]
        )

    def test_tree_query_matching_single_pane_tab_keeps_only_tab(self):
        items = tree_items("shell stuff")

        self.assertEqual(
            [item["arg"] for item in items], ["w1", "w1:t2"]
        )

    def test_single_pane_details_still_match_the_tab(self):
        cases = {
            "codex": ["w2", "w2:t9"],
            "/home/user/project-d": ["w2", "w2:t9"],
            "shell": ["w1", "w1:t2"],
            "project-c": ["w1", "w1:t2"],
            "chmouel@host": ["w1", "w1:t2"],
        }
        for query, expected in cases.items():
            with self.subTest(query=query):
                self.assertEqual([item["arg"] for item in tree_items(query)], expected)
        panes = [
            {**pane, "agent_status": "blocked"} if pane["pane_id"] == "w2:p9" else pane
            for pane in SAMPLE_PANES
        ]
        items = herdr_filter.build_tree_items(
            SAMPLE_WORKSPACES, SAMPLE_TABS, panes, "blocked"
        )
        self.assertEqual(
            [item["arg"] for item in items],
            ["w1", "w1:t1", "w1:p2", "w2", "w2:t9"],
        )

    def test_multi_pane_tab_keeps_shell_children_and_tab_matches_keep_all(self):
        panes = [
            *SAMPLE_PANES,
            {"pane_id": "w1:p4", "tab_id": "w1:t2", "cwd": "/tmp/extra"},
        ]
        items = herdr_filter.build_tree_items(
            SAMPLE_WORKSPACES, SAMPLE_TABS, panes, "shell stuff"
        )
        self.assertEqual(
            [item["arg"] for item in items], ["w1", "w1:t2", "w1:p3", "w1:p4"]
        )
        self.assertEqual(
            items[2]["variables"], {"HERDR_KIND": "pane", "HERDR_FOCUS": "w1:t2"}
        )
        self.assertEqual(items[2]["subtitle"], "chmouel@host: ~/project-c")

    def test_empty_tab_stays_visible_and_invalid_panes_do_not_prevent_collapse(self):
        panes = [pane for pane in SAMPLE_PANES if pane["tab_id"] != "w1:t2"]
        panes.append({"tab_id": "w2:t9"})
        items = herdr_filter.build_tree_items(
            SAMPLE_WORKSPACES, SAMPLE_TABS, panes, ""
        )
        self.assertEqual(
            [item["arg"] for item in items],
            ["w1", "w1:t1", "w1:p1", "w1:p2", "w1:t2", "w2", "w2:t9"],
        )
        self.assertEqual(items[4]["subtitle"], "empty")

    def test_tree_entries_without_ids_are_skipped(self):
        items = herdr_filter.build_tree_items(
            [{"label": "ghost"}], [{"label": "lost tab"}], [{"cwd": "/tmp"}], ""
        )

        self.assertEqual(items, [])

    def test_agents_mode_lists_only_agents_with_variables(self):
        items = herdr_filter.build_agent_items(SAMPLE_AGENTS, "")

        self.assertEqual(len(items), 3)
        self.assertEqual(items[0]["title"], "claude \x1b[32m[working]\x1b[0m - project-a")
        self.assertEqual(items[0]["subtitle"], "implement feature X")
        self.assertEqual(items[0]["arg"], "w1:p1")
        self.assertEqual(items[0]["variables"], {"HERDR_KIND": "agent", "HERDR_FOCUS": "w1:p1"})
        self.assertNotIn("indent", items[0])

    def test_agents_query_matches_status_project_and_terminal_title(self):
        cases = {
            "CLAUDE": "w1:p1",
            "blocked": "w1:p2",
            "project-c": "w1:p3",
            "review PR": "w1:p2",
        }
        for query, pane_id in cases.items():
            with self.subTest(query=query):
                items = herdr_filter.build_agent_items(SAMPLE_AGENTS, query)
                self.assertEqual([item["arg"] for item in items], [pane_id])

    def test_items_without_pane_id_are_not_selectable(self):
        items = herdr_filter.build_agent_items(
            [{"agent": "claude", "agent_status": "working"}], ""
        )

        self.assertEqual(items, [])

    def test_parse_argv_separates_flags_from_query(self):
        self.assertEqual(herdr_filter.parse_argv(["--agents"]), ("agents", ""))
        self.assertEqual(herdr_filter.parse_argv(["--agents", "foo"]), ("agents", "foo"))
        self.assertEqual(herdr_filter.parse_argv(["foo", "bar"]), ("workspace", "foo bar"))
        self.assertEqual(herdr_filter.parse_argv(["--tree", "x"]), ("workspace", "x"))

        with patch.dict(os.environ, {"HERDR_SCOPE": "agents"}):
            self.assertEqual(herdr_filter.parse_argv(["x"]), ("agents", "x"))
            self.assertEqual(herdr_filter.parse_argv(["--tree", "x"]), ("workspace", "x"))

    def test_main_lists_the_tree_by_default(self):
        stdout = io.StringIO()
        with (
            patch.object(herdr_filter, "get_workspaces", return_value=SAMPLE_WORKSPACES),
            patch.object(herdr_filter, "get_tabs", return_value=SAMPLE_TABS),
            patch.object(herdr_filter, "get_panes", return_value=SAMPLE_PANES),
            patch.object(herdr_filter, "get_agents") as get_agents,
            redirect_stdout(stdout),
        ):
            result = herdr_filter.main([])

        self.assertEqual(result, 0)
        items = json.loads(stdout.getvalue())["items"]
        self.assertEqual([item["arg"] for item in items][0], "w1")
        self.assertEqual(items[2]["indent"], 2)
        get_agents.assert_not_called()

    def test_main_with_agents_flag_lists_agents(self):
        stdout = io.StringIO()
        with (
            patch.object(herdr_filter, "get_agents", return_value=SAMPLE_AGENTS),
            patch.object(herdr_filter, "get_workspaces") as get_workspaces,
            redirect_stdout(stdout),
        ):
            result = herdr_filter.main(["--agents"])

        self.assertEqual(result, 0)
        items = json.loads(stdout.getvalue())["items"]
        self.assertEqual([item["arg"] for item in items], ["w1:p1", "w1:p2", "w1:p3"])
        get_workspaces.assert_not_called()

    def test_empty_agent_list_outputs_empty_items(self):
        stdout = io.StringIO()
        with (
            patch.object(herdr_filter, "get_agents", return_value=[]),
            redirect_stdout(stdout),
        ):
            result = herdr_filter.main(["--agents"])

        self.assertEqual(result, 0)
        self.assertEqual(json.loads(stdout.getvalue()), {"items": []})

    def test_command_failure_is_reported(self):
        stderr = io.StringIO()
        with patch.object(
            herdr_filter, "get_workspaces", side_effect=FileNotFoundError("herdr")
        ):
            with redirect_stderr(stderr):
                result = herdr_filter.main([])

        self.assertEqual(result, 1)
        self.assertIn("Herdr script filter:", stderr.getvalue())


class HerdrIconTests(unittest.TestCase):
    def test_presets_cover_semantic_rows_and_recognized_agents(self):
        for preset in herdr_filter.ICON_SETS:
            with self.subTest(preset=preset), patch.dict(
                os.environ, {"HERDR_ICON_SET": preset}
            ):
                items = {item["arg"]: item for item in tree_items()}
                expected = {
                    "w1": "workspace.svg",
                    "w1:t1": "session.svg",
                    "w1:p1": "agent-claude.svg",
                    "w1:p2": "agent-pi.svg",
                    "w1:t2": "shell.svg",
                    "w2:t9": "agent-codex.svg",
                }
                for item_id, name in expected.items():
                    self.assertEqual(
                        items[item_id]["icon"]["path"],
                        str(herdr_filter.ICON_DIR / preset / name),
                    )
                self.assertEqual(
                    herdr_filter.tab_item(SAMPLE_TABS[0], [])["icon"]["path"],
                    herdr_filter.icon_path("session"),
                )
                for agent in herdr_filter.AGENT_ICONS:
                    self.assertTrue(Path(herdr_filter.pane_icon({"agent": agent})).is_file())

    def test_default_icons_and_unknown_agents_are_independent_of_cwd(self):
        env = {key: value for key, value in os.environ.items() if key != "HERDR_ICON_SET"}
        with patch.dict(os.environ, env, clear=True), tempfile.TemporaryDirectory() as tmp:
            result = subprocess.run(
                [
                    sys.executable, "-c",
                    "import sys; sys.path.insert(0, sys.argv[1]); "
                    "import herdr_filter; print(herdr_filter.pane_icon({'agent':'../../evil'}))",
                    str(Path(herdr_filter.__file__).resolve().parent),
                ],
                cwd=tmp, capture_output=True, text=True, check=True,
            )
            self.assertEqual(
                result.stdout.strip(),
                str(herdr_filter.ICON_DIR / "color" / "agent-generic.svg"),
            )
            self.assertEqual(
                herdr_filter.build_agent_items([{"agent": "future-agent", "pane_id": "p"}], "")[0]["icon"]["path"],
                result.stdout.strip(),
            )

    def test_invalid_preset_and_missing_assets_report_actionable_errors(self):
        cases = [
            ({"HERDR_ICON_SET": "../source"}, None, "Unknown HERDR_ICON_SET"),
            ({"HERDR_ICON_SET": "color"}, Path("/nonexistent/herdr-icons"), "copy the icons directory"),
        ]
        for env, directory, expected in cases:
            with (
                self.subTest(env=env, directory=directory),
                patch.dict(os.environ, {**env, "RAFFI_PREVIEW": "0"}),
                patch.object(herdr_filter, "ICON_DIR", directory or herdr_filter.ICON_DIR),
                patch.object(herdr_filter, "get_workspaces") as get_workspaces,
                redirect_stderr(io.StringIO()) as stderr,
                redirect_stdout(io.StringIO()) as stdout,
            ):
                self.assertEqual(herdr_filter.main([]), 1)
                self.assertIn(expected, stderr.getvalue())
                self.assertEqual(stdout.getvalue(), "")
                get_workspaces.assert_not_called()
                self.assertEqual(herdr_filter.preview_main("w1", "400"), 0)
                self.assertIn(expected, json.loads(stdout.getvalue())["text"])

    def test_generated_icons_are_complete_and_reproducible(self):
        sources = herdr_filter.ICON_DIR / "source"
        names = {"workspace", "session", "shell", "agent-generic"} | {
            f"agent-{agent}" for agent in herdr_filter.AGENT_ICONS
        } | {f"status-{name}" for name in ("working", "waiting", "idle", "done", "unknown")}
        self.assertEqual({path.stem for path in sources.glob("*.svg")}, names)
        for preset in herdr_filter.ICON_SETS:
            for name in names:
                with self.subTest(preset=preset, name=name):
                    generated = herdr_filter.ICON_DIR / preset / f"{name}.svg"
                    content = generated.read_text()
                    self.assertEqual(
                        content, generate_icons.render_icon(sources / f"{name}.svg", preset)
                    )
                    self.assertNotIn("currentColor", content)
                    root = ET.fromstring(content)
                    for element in root.iter():
                        self.assertFalse(element.tag.endswith(("image", "script", "style")))
                        for key, value in element.attrib.items():
                            self.assertFalse(key.endswith("href"))
                            self.assertNotIn("url(", value)


class HerdrStatusTests(unittest.TestCase):
    def status_items(self, query="", panes=None):
        return herdr_filter.build_status_items(
            SAMPLE_WORKSPACES, SAMPLE_TABS,
            SAMPLE_PANES if panes is None else panes, query,
        )

    def test_status_tree_groups_per_pane_and_focuses_each_agent_once(self):
        items = self.status_items(panes=[*SAMPLE_PANES, SAMPLE_PANES[0]])
        self.assertEqual(
            [(item["arg"], item["indent"]) for item in items],
            [
                ("status:working", 0), ("w1:p1", 1),
                ("status:blocked", 0), ("w1:p2", 1),
                ("status:idle", 0), ("w2:p9", 1),
            ],
        )
        self.assertEqual(items[2]["title"], "Waiting")
        self.assertFalse(items[2]["valid"])
        self.assertEqual(items[2]["subtitle"], "1 agent")
        self.assertIn("pi", items[2]["preview"]["text"])
        self.assertEqual(items[5]["title"], "Solo")
        self.assertIn("beta", items[5]["subtitle"])
        self.assertEqual(
            items[1]["variables"],
            {"HERDR_KIND": "agent", "HERDR_FOCUS": "w1:p1"},
        )
        self.assertIn("w1:p1", items[1]["title"])
        self.assertIn("w1:p2", items[3]["title"])

    def test_status_queries_keep_only_matching_groups_and_counts(self):
        cases = {
            "waiting": ["status:blocked", "w1:p2"],
            "blocked": ["status:blocked", "w1:p2"],
            "review": ["status:blocked", "w1:p2"],
            "beta": ["status:idle", "w2:p9"],
            "Solo": ["status:idle", "w2:p9"],
            "/home/user/project-a": ["status:working", "w1:p1"],
            "no match": [],
        }
        for query, expected in cases.items():
            with self.subTest(query=query):
                items = self.status_items(query)
                self.assertEqual([item["arg"] for item in items], expected)
        panes = [*SAMPLE_PANES, {**SAMPLE_PANES[1], "pane_id": "w1:p5"}]
        self.assertEqual(self.status_items("waiting", panes)[0]["subtitle"], "2 agents")

    def test_unknown_and_done_statuses_and_missing_parent_metadata(self):
        panes = [
            {"pane_id": "orphan", "agent": "future-agent", "agent_status": "paused"},
            {"pane_id": "done", "agent": "pi", "agent_status": "done"},
            {"agent": "pi", "agent_status": "working"},
        ]
        items = self.status_items(panes=panes)
        self.assertEqual(
            [item["arg"] for item in items],
            ["status:done", "done", "status:unknown", "orphan"],
        )
        self.assertIn("paused", items[3]["subtitle"])
        self.assertTrue(items[3]["icon"]["path"].endswith("agent-generic.svg"))
        self.assertEqual(self.status_items(panes=[]), [])

    def test_status_modes_env_flags_and_invalid_configuration(self):
        with patch.dict(os.environ, {"HERDR_VIEW": "status", "HERDR_SCOPE": ""}):
            self.assertEqual(herdr_filter.parse_argv(["query"]), ("status", "query"))
            self.assertEqual(herdr_filter.parse_argv(["--tree"]), ("workspace", ""))
            self.assertEqual(herdr_filter.parse_argv(["--agents"]), ("agents", ""))
            self.assertEqual(
                herdr_filter.parse_argv(["--tree", "--status", "waiting"]),
                ("status", "waiting"),
            )
        with (
            patch.dict(os.environ, {"HERDR_VIEW": "bad", "RAFFI_PREVIEW": "0"}),
            redirect_stderr(io.StringIO()) as stderr,
        ):
            self.assertEqual(herdr_filter.main([]), 1)
            self.assertIn("HERDR_VIEW must be", stderr.getvalue())

    def test_status_main_and_summary_preview_never_fetch_synthetic_ids(self):
        with (
            patch.object(herdr_filter, "get_workspaces", return_value=SAMPLE_WORKSPACES),
            patch.object(herdr_filter, "get_tabs", return_value=SAMPLE_TABS),
            patch.object(herdr_filter, "get_panes", return_value=SAMPLE_PANES),
            patch.object(herdr_filter, "herdr_object") as get_object,
            patch.dict(os.environ, {"RAFFI_QUERY": "waiting"}),
            redirect_stdout(io.StringIO()) as stdout,
        ):
            self.assertEqual(herdr_filter.main(["--status", "waiting"]), 0)
            self.assertEqual(
                json.loads(stdout.getvalue())["items"][0]["title"], "Waiting"
            )
            preview = herdr_filter.build_preview("status:blocked", "400")
            self.assertEqual(preview["subtitle"], "1 agent")
            self.assertIn("pi", preview["text"])
            empty = herdr_filter.build_preview("status:done", "400")
            self.assertEqual(empty["text"], "No agents in this group")
            with self.assertRaises(ValueError):
                herdr_filter.build_preview("status:invalid", "400")
            get_object.assert_not_called()


class HerdrPreviewTests(unittest.TestCase):
    def fake_herdr(self, *args):
        if args[:2] == ("workspace", "get"):
            return json.dumps({"result": {"workspace": SAMPLE_WORKSPACES[0]}})
        if args[:2] == ("workspace", "list"):
            return json.dumps({"result": {"workspaces": SAMPLE_WORKSPACES}})
        if args[:2] == ("tab", "get"):
            tab = next(tab for tab in SAMPLE_TABS if tab["tab_id"] == args[2])
            return json.dumps({"result": {"tab": tab}})
        if args[:2] == ("tab", "list"):
            return json.dumps({"result": {"tabs": SAMPLE_TABS}})
        if args[:2] == ("pane", "get"):
            pane = next(pane for pane in SAMPLE_PANES if pane["pane_id"] == args[2])
            return json.dumps({"result": {"pane": pane}})
        if args[:2] == ("pane", "list"):
            return json.dumps({"result": {"panes": SAMPLE_PANES}})
        if args[:2] == ("pane", "read"):
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

    def test_preview_kind_prefers_the_item_variable_over_the_id_shape(self):
        with patch.dict(os.environ, {"HERDR_KIND": "tab"}, clear=False):
            self.assertEqual(herdr_filter.preview_kind("w1:p1"), "tab")
        env = {key: value for key, value in os.environ.items() if key != "HERDR_KIND"}
        with patch.dict(os.environ, env, clear=True):
            self.assertEqual(herdr_filter.preview_kind("w1"), "workspace")
            self.assertEqual(herdr_filter.preview_kind("w1:t1"), "tab")
            self.assertEqual(herdr_filter.preview_kind("w1:p1"), "pane")

    def test_build_pane_preview_reads_pane_and_terminal(self):
        with patch.object(
            herdr_filter, "run_herdr", side_effect=self.fake_herdr
        ) as run:
            preview = herdr_filter.build_preview("w1:p1", "400")

        run.assert_any_call(
            "pane", "read", "w1:p1", "--source", "recent", "--lines", "20", "--ansi"
        )
        self.assertEqual(preview["title"], "claude [working] - project-a")
        self.assertEqual(preview["subtitle"], "implement feature X")
        self.assertEqual(preview["refresh"], herdr_filter.PREVIEW_REFRESH_SECONDS)
        self.assertEqual(
            preview["sections"][0]["content"], "\x1b[32mdone\x1b[0m\nnext line"
        )
        self.assertIn(["Agent", "claude"], preview["metadata"])
        self.assertIn(["Status", "working"], preview["metadata"])
        self.assertIn(["Pane", "w1:p1"], preview["metadata"])
        self.assertIn(["Workspace", "w1"], preview["metadata"])
        self.assertIn(["Tab", "w1:t1"], preview["metadata"])

    def test_build_tab_preview_lists_panes(self):
        with patch.object(
            herdr_filter, "run_herdr", side_effect=self.fake_herdr
        ) as run:
            preview = herdr_filter.build_preview("w1:t1", "400")

        # A tab with several panes lists them without reading a terminal.
        self.assertNotIn(("pane", "read"), tuple(call[:2] for call in run.call_args_list))
        self.assertEqual(preview["title"], "Feature")
        self.assertEqual(preview["subtitle"], "2 panes")
        self.assertEqual(preview["sections"][0]["heading"], "Panes")
        self.assertIn("claude [working] - project-a", preview["sections"][0]["content"])
        self.assertIn("pi [blocked] - project-b", preview["sections"][0]["content"])
        self.assertIn(["Panes", "2"], preview["metadata"])
        self.assertIn(["Workspace", "w1"], preview["metadata"])

    def test_build_tab_preview_reads_the_terminal_of_single_pane_tabs(self):
        with patch.object(
            herdr_filter, "run_herdr", side_effect=self.fake_herdr
        ) as run:
            preview = herdr_filter.build_preview("w1:t2", "400")

        run.assert_any_call(
            "pane", "read", "w1:p3", "--source", "recent", "--lines", "20", "--ansi"
        )
        self.assertEqual(
            [section["heading"] for section in preview["sections"]],
            ["Panes", "Terminal"],
        )

    def test_build_workspace_preview_lists_tabs(self):
        with patch.object(
            herdr_filter, "run_herdr", side_effect=self.fake_herdr
        ) as run:
            preview = herdr_filter.build_preview("w1", "400")

        run.assert_any_call("workspace", "get", "w1")
        run.assert_any_call("tab", "list")
        self.assertEqual(preview["title"], "alpha")
        self.assertEqual(preview["subtitle"], "2 tabs · 3 panes")
        content = preview["sections"][0]["content"]
        self.assertIn("Feature [working] - 2 panes", content)
        self.assertIn("Shell stuff [unknown] - 1 pane", content)
        self.assertIn(["Tabs", "2"], preview["metadata"])
        self.assertIn(["Active tab", "w1:t1"], preview["metadata"])

    def test_preview_icons_match_tree_rows_for_every_preset(self):
        for preset in herdr_filter.ICON_SETS:
            with (
                self.subTest(preset=preset),
                patch.dict(os.environ, {"HERDR_ICON_SET": preset}),
                patch.object(herdr_filter, "run_herdr", side_effect=self.fake_herdr),
            ):
                items = {item["arg"]: item for item in tree_items()}
                for item_id in ("w1", "w1:t1", "w1:t2", "w2:t9", "w1:p1"):
                    self.assertEqual(
                        herdr_filter.build_preview(item_id, "400")["icon"],
                        items[item_id]["icon"]["path"],
                    )

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
            result = herdr_filter.preview_main("w9:p9", None)

        self.assertEqual(result, 0)
        payload = json.loads(stdout.getvalue())
        self.assertIn("Cannot read this session", payload["text"])
        self.assertEqual(payload["refresh"], herdr_filter.PREVIEW_REFRESH_SECONDS)


if __name__ == "__main__":
    unittest.main()
