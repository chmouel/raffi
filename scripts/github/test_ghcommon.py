import subprocess
import unittest
from datetime import UTC, datetime, timedelta
from unittest.mock import MagicMock, patch

import ghcommon


def completed(stdout="", returncode=0, stderr=""):
    return MagicMock(stdout=stdout, returncode=returncode, stderr=stderr)


class RunGhTests(unittest.TestCase):
    def test_parses_the_json_output(self):
        with patch.object(subprocess, "run", return_value=completed('{"a": 1}')):
            self.assertEqual(ghcommon.run_gh("api", "x"), {"a": 1})

    def test_a_missing_gh_is_explained(self):
        with (
            patch.object(subprocess, "run", side_effect=FileNotFoundError),
            self.assertRaisesRegex(ghcommon.GhError, "GitHub CLI not found"),
        ):
            ghcommon.run_gh("api")

    def test_a_timeout_is_reported(self):
        timeout = subprocess.TimeoutExpired("gh", 3)
        with (
            patch.object(subprocess, "run", side_effect=timeout),
            self.assertRaisesRegex(ghcommon.GhError, "did not answer within 3"),
        ):
            ghcommon.run_gh("api", timeout=3)

    def test_a_failure_shows_gh_stderr(self):
        failed = completed(returncode=1, stderr="not logged in\n")
        with (
            patch.object(subprocess, "run", return_value=failed),
            self.assertRaisesRegex(ghcommon.GhError, "^not logged in$"),
        ):
            ghcommon.run_gh("api")

    def test_a_failure_without_stderr_shows_the_status(self):
        with (
            patch.object(subprocess, "run", return_value=completed(returncode=4)),
            self.assertRaisesRegex(ghcommon.GhError, "exited with 4"),
        ):
            ghcommon.run_gh("api")

    def test_invalid_json_is_reported(self):
        with (
            patch.object(subprocess, "run", return_value=completed("nope")),
            self.assertRaisesRegex(ghcommon.GhError, "invalid JSON"),
        ):
            ghcommon.run_gh("api")


class GraphqlTests(unittest.TestCase):
    def call(self, response=None, **kwargs):
        response = response or {"data": {"ok": True}}
        with patch.object(ghcommon, "run_gh", return_value=response) as run_gh:
            result = ghcommon.graphql("query", **kwargs)
        return result, run_gh.call_args.args

    def test_no_cache_unless_asked(self):
        _, args = self.call()
        self.assertNotIn("--cache", args)

    def test_a_cache_window_is_passed_to_gh(self):
        _, args = self.call(cache="10m")
        self.assertEqual(args[args.index("--cache") + 1], "10m")

    def test_raffi_refresh_bypasses_the_gh_cache(self):
        with patch.object(ghcommon, "REFRESH", True):
            _, args = self.call(cache="10m")
        self.assertNotIn("--cache", args)

    def test_integers_are_sent_as_numbers_and_the_rest_as_strings(self):
        _, args = self.call(owner="me", first=100)
        self.assertEqual(args[args.index("owner=me") - 1], "-f")
        self.assertEqual(args[args.index("first=100") - 1], "-F")

    def test_variables_set_to_none_are_left_out(self):
        _, args = self.call(owner="me", after=None)
        self.assertFalse(any(arg.startswith("after=") for arg in args))

    def test_the_host_is_passed(self):
        _, args = self.call()
        self.assertEqual(args[args.index("--hostname") + 1], ghcommon.API_HOST)

    def test_returns_the_data(self):
        result, _ = self.call({"data": {"viewer": {"login": "me"}}})
        self.assertEqual(result, {"viewer": {"login": "me"}})

    def test_graphql_errors_are_raised(self):
        with self.assertRaisesRegex(ghcommon.GhError, "Bad query"):
            self.call({"errors": [{"message": "Bad query"}]})


class HelperTests(unittest.TestCase):
    def test_ago(self):
        now = datetime.now(UTC)
        self.assertEqual(ghcommon.ago((now - timedelta(hours=3)).isoformat()), "3h ago")
        self.assertEqual(ghcommon.ago((now - timedelta(days=400)).isoformat()), "1y ago")
        self.assertEqual(ghcommon.ago(now.isoformat()), "just now")
        self.assertEqual(ghcommon.ago(None), "")

    def test_ago_reads_github_timestamps(self):
        self.assertTrue(ghcommon.ago("2020-01-01T00:00:00Z").endswith("y ago"))

    def test_colored_resets_after_the_text(self):
        self.assertEqual(
            ghcommon.colored("x", ghcommon.RED), f"{ghcommon.RED}x{ghcommon.RESET}"
        )

    def test_ansi_pattern_strips_the_colours(self):
        self.assertEqual(ghcommon.ANSI.sub("", ghcommon.colored("x", ghcommon.GREEN)), "x")

    def test_error_rows_do_nothing_when_chosen(self):
        row = ghcommon.error_item("GitHub error", "boom")
        self.assertFalse(row["valid"])
        self.assertEqual(row["subtitle"], "boom")
        self.assertFalse(row["preview"]["dynamic"])


class CacheContractTests(unittest.TestCase):
    def test_gh_durations_become_seconds(self):
        self.assertEqual(ghcommon.seconds("10m"), 600)
        self.assertEqual(ghcommon.seconds("24h"), 86400)
        self.assertEqual(ghcommon.seconds("1.5h"), 5400)
        self.assertEqual(ghcommon.seconds("90s"), 90)
        self.assertEqual(ghcommon.seconds("2d"), 172800)

    def test_unreadable_durations_fall_back(self):
        self.assertEqual(ghcommon.seconds("soon"), 600)
        self.assertEqual(ghcommon.seconds("", default=30), 30)

    def test_the_cache_window_becomes_the_hint(self):
        output = ghcommon.with_cache({"view": "x"}, [{"title": "a"}], "24h")
        self.assertEqual(output["cache"], {"seconds": 86400})
        self.assertEqual(output["view"], "x")
        self.assertEqual(output["items"], [{"title": "a"}])

    def test_error_rows_ask_not_to_be_kept(self):
        rows = ghcommon.error_rows("GitHub error", "boom")
        self.assertEqual(rows[0]["title"], "GitHub error")
        self.assertEqual(ghcommon.with_cache({}, rows, "24h")["cache"], {"seconds": 0})

    def test_a_successful_list_never_sends_the_error_sentinel(self):
        for window in ("0s", "0.5s", "0m"):
            output = ghcommon.with_cache({}, [{"title": "a"}], window)
            self.assertEqual(output["cache"], {"seconds": 1}, window)

    def test_queries_worth_warming_ride_along_with_the_hint(self):
        output = ghcommon.with_cache({}, [], "10m", warm=("", "review"))
        self.assertEqual(output["cache"], {"seconds": 600, "warm": ["", "review"]})
        self.assertNotIn("warm", ghcommon.with_cache({}, [], "10m")["cache"])

    def test_an_error_never_asks_for_warming(self):
        rows = ghcommon.error_rows("GitHub error", "boom")
        output = ghcommon.with_cache({}, rows, "10m", warm=("review",))
        self.assertEqual(output["cache"], {"seconds": 0})

    def test_a_plain_empty_list_is_still_kept(self):
        self.assertEqual(ghcommon.with_cache({}, [], "10m")["cache"], {"seconds": 600})


if __name__ == "__main__":
    unittest.main()
