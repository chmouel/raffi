import io
import json
import os
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import MagicMock, patch

import pr


def node(number, title, ci_state=None, draft=False):
    rollup = {"state": ci_state} if ci_state else None
    return {
        "number": number,
        "title": title,
        "url": f"https://github.com/owner/repo/pull/{number}",
        "isDraft": draft,
        "repository": {"nameWithOwner": "owner/repo"},
        "author": {"login": "alice"},
        "commits": {"nodes": [{"commit": {"statusCheckRollup": rollup}}]},
    }


PULLS = [node(1, "Fix login", "FAILURE"), node(2, "Add docs", draft=True)]

VIEW = {
    "number": 7,
    "title": "Add feature",
    "url": "https://github.com/owner/repo/pull/7",
    "state": "OPEN",
    "isDraft": False,
    "author": {"login": "alice"},
    "headRefName": "feature",
    "baseRefName": "main",
    "reviewDecision": "APPROVED",
    "labels": [{"name": "bug"}],
    "additions": 10,
    "deletions": 2,
    "changedFiles": 1,
    "createdAt": "2026-01-01T00:00:00Z",
    "updatedAt": "2026-01-02T00:00:00Z",
    "body": "Intro\r\n<!-- template hint -->\n\n\n\nDetails",
    "statusCheckRollup": [
        {
            "__typename": "CheckRun",
            "name": "lint",
            "status": "COMPLETED",
            "conclusion": "SUCCESS",
        },
        {
            "__typename": "CheckRun",
            "name": "e2e",
            "workflowName": "CI",
            "status": "IN_PROGRESS",
            "conclusion": "",
        },
        {"__typename": "StatusContext", "context": "ci/legacy", "state": "FAILURE"},
    ],
}


class ParseQueryTests(unittest.TestCase):
    def test_targets(self):
        cases = {
            "": (None, ""),
            "myprs fix": (None, "fix"),
            "raffi docs": (None, "raffi docs"),
            "owner/repo fix login": ("owner/repo", "fix login"),
            "pac fix": ("tektoncd/pipelines-as-code", "fix"),
        }
        with patch.dict(os.environ, {"ALIAS_pac": "tektoncd/pipelines-as-code"}):
            for query, expected in cases.items():
                with self.subTest(query=query):
                    self.assertEqual(pr.parse_query(query), expected)

    def test_repo_argument_makes_whole_query_a_filter(self):
        self.assertEqual(
            pr.parse_query(" a/b c ", "owner/repo"), ("owner/repo", "a/b c")
        )


class ListItemsTests(unittest.TestCase):
    def list(self, query, response):
        with patch.object(pr, "run_gh", return_value={"data": response}) as run_gh:
            return pr.list_items(query), run_gh

    def test_repository_pulls_are_filtered(self):
        response = {"repository": {"pullRequests": {"nodes": PULLS}}}
        items, run_gh = self.list("owner/repo failed", response)

        self.assertIn("owner=owner", run_gh.call_args.args)
        self.assertEqual(items[0]["arg"], "https://github.com/owner/repo/pulls")
        self.assertEqual(
            [item["title"] for item in items[1:]], ["PR #1: Fix login - alice"]
        )
        self.assertIn("\x1b[31mCI: Failed\x1b[0m - Open", items[1]["subtitle"])

    def test_own_pulls_show_repository_and_draft(self):
        items, _ = self.list("docs", {"search": {"nodes": [*PULLS, {}]}})

        self.assertEqual(items[0]["arg"], "https://github.com/pulls")
        self.assertEqual(items[1]["title"], "PR #2: Add docs - owner/repo")
        self.assertTrue(items[1]["subtitle"].startswith("Draft"))

    def test_errors_become_items(self):
        self.assertEqual(pr.list_items("owner/")[0]["title"], "Invalid repository")
        with patch.object(pr, "run_gh", side_effect=pr.GhError("boom")):
            self.assertEqual(pr.list_items("")[0]["subtitle"], "boom")


class PreviewTests(unittest.TestCase):
    def test_build_preview(self):
        preview = pr.build_preview(VIEW)

        self.assertEqual(preview["title"], "#7 Add feature")
        self.assertEqual(preview["badge"], "Open")
        self.assertIn(["Review", "Approved"], preview["metadata"])
        self.assertIn(["Changes", "+10 -2 in 1 file"], preview["metadata"])
        checks, description = preview["sections"]
        self.assertEqual(checks["heading"], "Checks: 1 failed, 1 pending, 1 passed")
        self.assertEqual(
            pr.ANSI.sub("", checks["content"]).splitlines(),
            ["\uf057 ci/legacy", "\uf017 CI / e2e", "\uf058 lint"],
        )
        self.assertEqual(description["content"], "Intro\n\nDetails")
        self.assertEqual(preview["refresh"], pr.PREVIEW_REFRESH_SECONDS)

    def test_no_refresh_once_checks_are_done(self):
        done = dict(
            VIEW, isDraft=True, body="", statusCheckRollup=VIEW["statusCheckRollup"][:1]
        )
        preview = pr.build_preview(done)

        self.assertEqual(preview["badge"], "Draft")
        self.assertEqual(preview["sections"][1]["content"], "(no description)")
        self.assertNotIn("refresh", preview)

    def test_main_prints_preview_when_raffi_asks(self):
        stdout = io.StringIO()
        env = {"RAFFI_PREVIEW": "1", "RAFFI_VALUE": VIEW["url"]}
        with (
            patch.dict(os.environ, env),
            patch.object(pr, "run_gh", return_value=VIEW) as run_gh,
            redirect_stdout(stdout),
        ):
            self.assertEqual(pr.main(["--repo", "owner/repo", "query"]), 0)

        self.assertEqual(run_gh.call_args.args[:3], ("pr", "view", VIEW["url"]))
        self.assertNotIn("items", json.loads(stdout.getvalue()))

    def test_preview_error_is_shown_in_the_pane(self):
        with patch.object(pr, "run_gh", side_effect=pr.GhError("offline")):
            preview = pr.preview(VIEW["url"])

        self.assertIn("offline", preview["text"])


class MarkdownTests(unittest.TestCase):
    def test_markdown_is_rendered_with_ansi(self):
        body = """\
## Summary
- [x] done
  - [ ] todo
* **bold** `a*b*` _em_ snake_case [link](https://x) ![shot](https://i)
> [!NOTE]
> quoted
<details><summary>More</summary>
```
**raw**
```
| a | b |
|---|:-:|
***"""
        rendered = pr.render_markdown(body)

        self.assertIn("\x1b[1;35mSummary", rendered)
        self.assertIn("\x1b[1mbold\x1b[22m", rendered)
        self.assertIn("\x1b[4;34mlink\x1b[24;39m", rendered)
        self.assertEqual(
            pr.ANSI.sub("", rendered).splitlines(),
            [
                "Summary",
                "\u2611 done",
                "  \u2610 todo",
                "\u2022 bold a*b* em snake_case link [image: shot]",
                "\u2502 Note",
                "\u2502 quoted",
                "More",
                "  **raw**",
                "| a | b |",
                "\u2500" * 40,
            ],
        )


class AvatarTests(unittest.TestCase):
    def test_owner_avatar_is_cached_as_round_svg(self):
        response = MagicMock()
        response.__enter__.return_value = response
        response.headers.get_content_type.return_value = "image/jpeg"
        response.read.return_value = b"JPEG"
        repository = {"owner": {"login": "octo", "avatarUrl": "https://avatars/u/1"}}

        with (
            tempfile.TemporaryDirectory() as tmp,
            patch.object(pr, "AVATAR_DIR", Path(tmp)),
            patch.object(
                pr.urllib.request, "urlopen", return_value=response
            ) as urlopen,
        ):
            path = pr.owner_icon(repository)
            self.assertEqual(pr.owner_icon(repository), path)
            self.assertEqual(path, f"{tmp}/octo.svg")
            svg = Path(path).read_text()
            preview = pr.build_preview(
                dict(VIEW, url="https://github.com/octo/r/pull/7")
            )

        urlopen.assert_called_once()
        self.assertIn("data:image/jpeg;base64,SlBFRw==", svg)
        self.assertEqual(preview["icon"], path)

    def test_no_avatar_falls_back_to_github_icon(self):
        with patch.object(pr.urllib.request, "urlopen", side_effect=OSError("offline")):
            self.assertIsNone(
                pr.owner_icon({"owner": {"login": "x", "avatarUrl": "u"}})
            )
        self.assertIsNone(pr.owner_icon({"nameWithOwner": "x/y"}))


if __name__ == "__main__":
    unittest.main()
