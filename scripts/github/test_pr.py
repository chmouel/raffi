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


def check_run(name, status, conclusion=None, workflow=None):
    run = {"workflowRun": {"workflow": {"name": workflow}}} if workflow else None
    return {
        "__typename": "CheckRun",
        "name": name,
        "status": status,
        "conclusion": conclusion,
        "checkSuite": run,
    }


VIEW = {
    "number": 7,
    "title": "Add feature",
    "url": "https://github.com/owner/repo/pull/7",
    "state": "OPEN",
    "isDraft": False,
    "author": {"login": "alice"},
    "repository": {"nameWithOwner": "owner/repo"},
    "headRefName": "feature",
    "baseRefName": "main",
    "reviewDecision": "APPROVED",
    "mergeable": "MERGEABLE",
    "mergeStateStatus": "CLEAN",
    "autoMergeRequest": {"mergeMethod": "SQUASH", "enabledBy": {"login": "bob"}},
    "milestone": {"title": "v1.0"},
    "assignees": {"nodes": [{"login": "alice"}]},
    "labels": {"nodes": [{"name": "bug"}]},
    "reviewRequests": {"nodes": [{"requestedReviewer": {"name": "core"}}]},
    "latestReviews": {
        "nodes": [
            {"author": {"login": "bob"}, "state": "APPROVED"},
            {"author": {"login": "carol"}, "state": "CHANGES_REQUESTED"},
        ]
    },
    "reviewThreads": {"nodes": [{"isResolved": False}, {"isResolved": True}]},
    "comments": {
        "nodes": [
            {
                "author": {"login": "bob"},
                "createdAt": "2026-01-01T00:00:00Z",
                "body": "first",
            },
            {
                "author": {"__typename": "Bot", "login": "codecov"},
                "createdAt": "2026-01-01T01:00:00Z",
                "body": "coverage",
            },
            {
                "author": {"login": "ci-robot"},
                "createdAt": "2026-01-01T01:30:00Z",
                "body": "/approve",
            },
            {
                "author": {"login": "carol"},
                "createdAt": "2026-01-01T02:00:00Z",
                "body": "second",
            },
        ]
    },
    "closingIssuesReferences": {
        "nodes": [
            {
                "number": 3,
                "title": "Broken",
                "state": "OPEN",
                "repository": {"nameWithOwner": "owner/repo"},
            },
            {
                "number": 9,
                "title": "Upstream",
                "state": "CLOSED",
                "repository": {"nameWithOwner": "other/lib"},
            },
        ]
    },
    "additions": 10,
    "deletions": 2,
    "changedFiles": 3,
    "files": {
        "totalCount": 3,
        "nodes": [
            {"path": "a.py", "additions": 8, "deletions": 0, "changeType": "ADDED"},
            {"path": "b.py", "additions": 2, "deletions": 2, "changeType": "MODIFIED"},
        ],
    },
    "createdAt": "2026-01-01T00:00:00Z",
    "updatedAt": "2026-01-02T00:00:00Z",
    "body": "Intro\r\n<!-- template hint -->\n\n\n\nDetails",
    "commits": {
        "totalCount": 2,
        "nodes": [
            {
                "commit": {
                    "messageHeadline": "Add feature",
                    "statusCheckRollup": {
                        "contexts": {
                            "nodes": [
                                check_run("lint", "COMPLETED", "SUCCESS"),
                                check_run("e2e", "IN_PROGRESS", workflow="CI"),
                                {
                                    "__typename": "StatusContext",
                                    "context": "ci/legacy",
                                    "state": "FAILURE",
                                },
                            ]
                        }
                    },
                }
            }
        ],
    },
}


class ParseQueryTests(unittest.TestCase):
    def test_targets(self):
        cases = {
            "": ("myprs", ""),
            "myprs fix": ("myprs", "fix"),
            "review": ("review", ""),
            "involved docs": ("involved", "docs"),
            "raffi docs": ("myprs", "raffi docs"),
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
        self.assertIn("\x1b[31mCI: Failed\x1b[0m \u00b7 Open", items[1]["subtitle"])
        self.assertEqual(
            items[1]["variables"],
            {
                "PR_URL": "https://github.com/owner/repo/pull/1",
                "PR_REPO": "owner/repo",
                "PR_NUMBER": "1",
                "PR_BRANCH": "",
            },
        )

    def test_own_pulls_show_repository_and_draft(self):
        items, _ = self.list("docs", {"search": {"nodes": [*PULLS, {}]}})

        self.assertEqual(items[0]["arg"], "https://github.com/pulls")
        self.assertEqual(items[1]["title"], "PR #2: Add docs - owner/repo")
        self.assertTrue(items[1]["subtitle"].startswith("Draft"))

    def test_modes_search_their_own_pull_requests(self):
        items, run_gh = self.list("review", {"search": {"nodes": PULLS}})

        self.assertIn("q=is:pr is:open review-requested:@me", run_gh.call_args.args)
        self.assertEqual(items[0]["arg"], "https://github.com/pulls/review-requested")

    def test_subtitle_shows_review_conflicts_threads_and_size(self):
        pull = dict(
            PULLS[0],
            reviewDecision="CHANGES_REQUESTED",
            mergeable="CONFLICTING",
            reviewThreads={"nodes": [{"isResolved": False}]},
            additions=5,
            deletions=1,
        )
        subtitle = pr.ANSI.sub("", pr.build_item(pull, False)["subtitle"])

        self.assertIn(
            "Changes requested \u00b7 Conflicts \u00b7 \U0001f4ac 1 unresolved \u00b7 +5 -1",
            subtitle,
        )

    def test_errors_become_items(self):
        self.assertEqual(pr.list_items("owner/")[0]["title"], "Invalid repository")
        with patch.object(pr, "run_gh", side_effect=pr.GhError("boom")):
            self.assertEqual(pr.list_items("")[0]["subtitle"], "boom")


class PreviewTests(unittest.TestCase):
    def test_build_preview(self):
        preview = pr.build_preview(VIEW)

        self.assertEqual(preview["title"], "#7 Add feature")
        self.assertEqual(preview["badge"], "Open")
        metadata = dict(preview["metadata"])
        self.assertEqual(metadata["Review"], "Approved")
        self.assertEqual(metadata["Reviewers"], "\u2713 bob, \u2717 carol, \u23f3 core")
        self.assertEqual(metadata["Merge"], "Ready, auto-merge (squash) by bob")
        self.assertEqual(metadata["Threads"], "1 unresolved")
        self.assertEqual(metadata["Commits"], "2 commits, last: Add feature")
        self.assertEqual(metadata["Milestone"], "v1.0")
        self.assertEqual(metadata["Changes"], "+10 -2 in 3 files")
        checks, closes, description, *comments, files = preview["sections"]
        self.assertEqual(checks["heading"], "Checks: 1 failed, 1 pending, 1 passed")
        self.assertEqual(
            pr.ANSI.sub("", checks["content"]).splitlines(),
            ["\uf057 ci/legacy", "\uf017 CI / e2e", "\uf058 lint"],
        )
        self.assertEqual(
            pr.ANSI.sub("", closes["content"]).splitlines(),
            ["#3 [open] Broken", "other/lib#9 [closed] Upstream"],
        )
        self.assertEqual(description["content"], "Intro\n\nDetails")
        self.assertEqual(
            [c["content"] for c in comments], ["second", "first"], "bots skipped"
        )
        self.assertEqual(files["heading"], "Files (3)")
        self.assertEqual(
            pr.ANSI.sub("", files["content"]).splitlines(),
            ["A a.py +8 -0", "M b.py +2 -2", "\u2026 and 1 more"],
        )
        self.assertEqual(preview["refresh"], pr.PREVIEW_REFRESH_SECONDS)

    def test_merged_and_conflicting_states(self):
        merged = dict(VIEW, state="MERGED", mergedAt=None)
        conflicting = dict(VIEW, mergeable="CONFLICTING", autoMergeRequest=None)

        self.assertEqual(pr.merge_state(merged), "Merged")
        self.assertEqual(pr.merge_state(conflicting), "Conflicts")

    def test_no_refresh_once_checks_are_done(self):
        commit = VIEW["commits"]["nodes"][0]["commit"]
        lint = commit["statusCheckRollup"]["contexts"]["nodes"][:1]
        rollup = {"contexts": {"nodes": lint}}
        commits = {
            "totalCount": 1,
            "nodes": [{"commit": dict(commit, statusCheckRollup=rollup)}],
        }
        done = dict(VIEW, isDraft=True, body="", commits=commits)
        preview = pr.build_preview(done)

        self.assertEqual(preview["badge"], "Draft")
        self.assertEqual(preview["sections"][2]["content"], "(no description)")
        self.assertNotIn("refresh", preview)

    def test_main_prints_preview_when_raffi_asks(self):
        stdout = io.StringIO()
        env = {"RAFFI_PREVIEW": "1", "RAFFI_VALUE": VIEW["url"]}
        with (
            patch.dict(os.environ, env),
            patch.object(
                pr,
                "run_gh",
                return_value={"data": {"repository": {"pullRequest": VIEW}}},
            ) as run_gh,
            redirect_stdout(stdout),
        ):
            self.assertEqual(pr.main(["--repo", "owner/repo", "query"]), 0)

        args = run_gh.call_args.args
        self.assertEqual(args[:2], ("api", "graphql"))
        self.assertIn("number=7", args)
        self.assertNotIn("--cache", args)
        self.assertEqual(json.loads(stdout.getvalue())["title"], "#7 Add feature")

    def test_preview_error_is_shown_in_the_pane(self):
        with patch.object(pr, "run_gh", side_effect=pr.GhError("offline")):
            preview = pr.preview(VIEW["url"])

        self.assertIn("offline", preview["text"])
        self.assertEqual(pr.preview("nope")["text"], "Not a pull request URL.")
        missing = {"data": {"repository": {"pullRequest": None}}}
        with patch.object(pr, "run_gh", return_value=missing):
            self.assertIn("not found", pr.preview(VIEW["url"])["text"])


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
                dict(VIEW, repository={"nameWithOwner": "octo/r"})
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
