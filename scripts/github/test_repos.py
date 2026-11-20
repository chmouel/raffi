import fcntl
import io
import json
import os
import tempfile
import threading
import time
import unittest
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from datetime import UTC, datetime, timedelta
from pathlib import Path
from unittest.mock import patch

import repos

# A fixed age for the live rows, so their "updated" text is predictable.
UPDATED = (datetime.now(UTC) - timedelta(hours=3)).isoformat()


def repo(name, **fields):
    item = {
        "name": name,
        "nameWithOwner": f"owner/{name}",
        "description": "",
        "url": f"https://github.com/owner/{name}",
        "isPrivate": False,
        "isFork": False,
        "isArchived": False,
        "stargazerCount": 0,
        "forkCount": 0,
        "pushedAt": None,
        "updatedAt": None,
        "primaryLanguage": None,
        "repositoryTopics": {"nodes": []},
    }
    item.update(fields)
    return item


def account(login, items, total=None):
    return {
        "login": login,
        "url": f"https://github.com/{login}",
        "total": len(items) if total is None else total,
        "repos": items,
    }


ACCOUNTS = {
    "me": account(
        "me",
        [
            repo("raffi", description="A launcher"),
            repo("pipelines", description="Pipelines helper"),
            repo("notes"),
        ],
    ),
    "tektoncd": account(
        "tektoncd",
        [
            repo("pipeline", description="Pipeline resources", stargazerCount=9100),
            repo("pipelines-as-code", description="Pipelines as Code", isPrivate=True),
            repo("operator"),
            repo("triggers", description="Event triggers"),
        ],
    ),
    "small": account("small", [repo("one")], total=1),
    "order": account(
        "order",
        [repo("alpha", description="delta in the description"), repo("delta-repo")],
    ),
}


def live_pull(repo_name, number, title, **fields):
    node = {
        "number": number,
        "title": title,
        "url": f"https://github.com/{repo_name}/pull/{number}",
        "isDraft": False,
        "updatedAt": UPDATED,
        "author": {"login": "chmouel"},
        "headRefName": f"branch-{number}",
        "reviewDecision": None,
    }
    node.update(fields)
    return node


def live_issue(repo_name, number, title, labels=(), **fields):
    node = {
        "number": number,
        "title": title,
        "url": f"https://github.com/{repo_name}/issues/{number}",
        "updatedAt": UPDATED,
        "author": {"login": "chmouel"},
        "labels": {"nodes": [{"name": label} for label in labels]},
    }
    node.update(fields)
    return node


# Deterministic open lists, for the repositories the live query is asked about.
LIVE = {
    "owner/raffi": {
        "pullRequests": {
            "totalCount": 34,
            "nodes": [
                live_pull(
                    "owner/raffi", 412, "Add clipboard view",
                    reviewDecision="APPROVED",
                ),
                live_pull(
                    "owner/raffi", 410, "Fix config reload",
                    isDraft=True, reviewDecision="CHANGES_REQUESTED",
                ),
            ],
        },
        "issues": {
            "totalCount": 2,
            "nodes": [
                live_issue(
                    "owner/raffi", 388, "Crash on empty config",
                    labels=("bug", "crash"),
                ),
            ],
        },
    },
    "tektoncd/pipeline": {
        "pullRequests": {
            "totalCount": 1,
            "nodes": [live_pull("tektoncd/pipeline", 7, "Bump deps")],
        },
        "issues": {"totalCount": 0, "nodes": []},
    },
}


def graphql(query, cache=None, timeout=30, **variables):
    if "viewer" in query:
        return {"viewer": {"login": "me"}}
    if "pullRequests" in query:
        name = f"{variables.get('owner')}/{variables.get('name')}"
        return {"repository": LIVE.get(name)}
    found = ACCOUNTS.get(variables.get("owner"))
    if "repository(" in query:
        named = any(
            item["name"].casefold() == variables.get("name", "").casefold()
            for item in (found or {}).get("repos", [])
        )
        return {"repository": {"name": variables.get("name")} if named else None}
    if not found:
        return {"repositoryOwner": None}
    page = {
        "totalCount": found["total"],
        "pageInfo": {"hasNextPage": False, "endCursor": None},
        "nodes": found["repos"],
    }
    return {
        "repositoryOwner": {
            "login": found["login"],
            "url": found["url"],
            "repositories": page,
        }
    }


@contextmanager
def environment(**values):
    """A pinned environment, so the suite does not inherit the ambient one."""
    saved = {key: os.environ[key] for key in os.environ if key.startswith("ALIAS_")}
    for key in saved:
        del os.environ[key]
    pinned = {
        "RAFFI_VIEW": "",
        "GITHUB_USER": "",
        "USAGE_SORT": "",
        "LIVE": "",
        "LIVE_ITEMS": str(repos.LIVE_ITEMS),
        "LIVE_PREFETCH": str(repos.LIVE_PREFETCH),
        "LIVE_BUDGET": repos.LIVE_BUDGET,
        "LIVE_RETRY": repos.LIVE_RETRY,
        "CACHE_LIVE": repos.CACHE_LIVE,
        "CACHE_REPOS": repos.CACHE_REPOS,
        "CACHE_MISSING": repos.CACHE_MISSING,
        "CACHE_LOGIN": repos.CACHE_LOGIN,
        "REPO_PATH": "",
        "REPO_PAGES": "",
        "REPO_ACTIONS": "",
        "OWNERS": "",
        "ANY_OWNER": "",
    }
    with patch.dict(os.environ, pinned), patch.dict(os.environ, values):
        yield


# The settings the suite runs with, pinned on the module itself in setUp: the
# module reads the environment at import, so an ambient LIVE_ITEMS or
# LIVE_PREFETCH would otherwise change what the assertions compare against.
SETTLED = {
    "LIVE_ITEMS": 20,
    "FETCH_ITEMS": 20,
    "LIVE_PREFETCH": 20,
    "LIVE_BUDGET": "120s",
    "LIVE_RETRY": "1m",
    "LIVE_ERRORS": "1h",
    "CACHE_LIVE": "10m",
}


class ReposTestCase(unittest.TestCase):
    def setUp(self):
        repos.ACCOUNTS.clear()
        repos.PENDING.clear()
        repos.NAMED.clear()
        repos.reload_errors()
        for name, value in SETTLED.items():
            self.addCleanup(setattr, repos, name, getattr(repos, name))
            setattr(repos, name, value)
        # Options() reads LIVE_ITEMS when the class is defined, so its own
        # defaults carry the ambient one; both are pinned here.
        self.addCleanup(setattr, repos, "DEFAULTS", repos.DEFAULTS)
        repos.DEFAULTS = repos.Options(live_items=SETTLED["LIVE_ITEMS"])
        self.addCleanup(repos.NAMED.clear)
        self.prefetched = []
        # The real start_prefetch, which the patch below hides from the tests.
        self.real_start_prefetch = repos.start_prefetch
        self.cache = tempfile.TemporaryDirectory()
        cache_dir = Path(self.cache.name)
        self.patches = [
            patch.object(repos, "graphql", graphql),
            patch.object(repos, "CACHE_DIR", cache_dir),
            patch.object(repos, "start_prefetch", lambda: self.prefetched.append(True)),
        ]
        for patcher in self.patches:
            patcher.start()
            self.addCleanup(patcher.stop)
        self.addCleanup(self.cache.cleanup)
        self.addCleanup(repos.ACCOUNTS.clear)
        self.addCleanup(repos.PENDING.clear)

    def titles(self, items):
        return [repos.ANSI.sub("", item["title"]) for item in items]

    def options(self, **kwargs):
        """Options with the suite's live_items, not whatever the env said."""
        kwargs.setdefault("live_items", repos.LIVE_ITEMS)
        return repos.Options(**kwargs)

    def queued(self):
        return repos.read_queue().names


class AliasTests(ReposTestCase):
    def test_only_me_is_built_in(self):
        with environment():
            self.assertIsNone(repos.alias_target("t"))
            self.assertIsNone(repos.alias_target("osp"))

    def test_me_is_the_logged_in_account(self):
        with environment():
            self.assertEqual(repos.alias_target("me"), "me")

    def test_alias_from_environment(self):
        with environment(ALIAS_kube="kubernetes", ALIAS_t="tektoncd"):
            self.assertEqual(repos.alias_target("kube"), "kubernetes")
            self.assertEqual(repos.alias_target("T"), "tektoncd")

    def test_unknown_word_is_not_an_alias(self):
        with environment():
            self.assertIsNone(repos.alias_target("zzz"))


class ResolveTests(ReposTestCase):
    def test_empty_query(self):
        with environment():
            self.assertEqual(repos.resolve("", "", ["me"]), ("me", "", None, False))

    def test_alias_in_front(self):
        with environment(ALIAS_t="tektoncd"):
            self.assertEqual(
                repos.resolve("t pipeline", "", ["me"]),
                ("tektoncd", "pipeline", None, True),
            )

    def test_repository(self):
        with environment():
            self.assertEqual(
                repos.resolve("chmouel/raffi", "", ["me"]),
                ("chmouel", "", "chmouel/raffi", True),
            )

    def test_view_wins_over_the_default_account(self):
        with environment():
            self.assertEqual(
                repos.resolve("", "tektoncd", ["me"]), ("tektoncd", "", None, False)
            )


class ListItemsTests(ReposTestCase):
    """Typing any account, as --any-owner allows."""

    def list(self, query, view="", owners=("me",), **kwargs):
        kwargs.setdefault("any_owner", True)
        env = {key: value for key, value in kwargs.items() if key.startswith("ALIAS_")}
        options = self.options(
            **{key: value for key, value in kwargs.items() if key not in env}
        )
        with environment(**env):
            return repos.list_items(query, view, list(owners), options)

    def test_default_account_lists_every_repository(self):
        owner, items = self.list("")
        self.assertEqual(owner, "me")
        self.assertEqual(
            self.titles(items), ["raffi", "pipelines", "notes", "me"]
        )

    def test_typing_an_account_lists_it(self):
        owner, items = self.list("tektoncd")
        self.assertEqual(owner, "tektoncd")
        self.assertEqual(
            self.titles(items),
            ["pipeline", "pipelines-as-code", "operator", "triggers", "tektoncd"],
        )

    def test_typing_an_account_then_a_filter(self):
        owner, items = self.list("tektoncd pipeline")
        self.assertEqual(owner, "tektoncd")
        self.assertEqual(
            self.titles(items),
            ["pipeline", "pipelines-as-code", "tektoncd"],
        )

    def test_a_repository_name_filters_instead_of_switching_account(self):
        owner, items = self.list("raffi")
        self.assertEqual(owner, "me")
        self.assertEqual(self.titles(items), ["raffi", "me"])

    def test_a_repository_name_that_is_an_account_still_filters(self):
        # "twin" is both an account and one of my repositories: the repository
        # wins, so a single word stays a filter.
        mine = account("me", [repo("raffi"), repo("twin")])
        with patch.dict(ACCOUNTS, {"me": mine, "twin": account("twin", [repo("x")])}):
            owner, items = self.list("twin")
        self.assertEqual(owner, "me")
        self.assertEqual(self.titles(items), ["twin", "me"])

    def test_me_forces_a_filter(self):
        owner, items = self.list("me tektoncd")
        self.assertEqual(owner, "me")
        self.assertEqual(self.titles(items), ["me", "No repository matches 'tektoncd'"])

    def test_no_match_row_is_not_actionable(self):
        _, items = self.list("me tektoncd")
        self.assertFalse(items[1]["valid"])

    def test_initials_find_a_repository(self):
        _, items = self.list("t pac", ALIAS_t="tektoncd")
        self.assertEqual(self.titles(items), ["pipelines-as-code", "tektoncd"])

    def test_name_matches_rank_above_initials(self):
        found = account("mixed", [repo("a-b-c", description="abc"), repo("abc")])
        with patch.dict(ACCOUNTS, {"mixed": found}):
            _, items = self.list("mixed abc")
        self.assertEqual(self.titles(items), ["abc", "a-b-c", "mixed"])

    def test_unknown_account_is_a_filter(self):
        owner, items = self.list("nothing-matches-this")
        self.assertEqual(owner, "me")
        self.assertEqual(
            self.titles(items), ["me", "No repository matches 'nothing-matches-this'"]
        )

    def test_names_matching_the_filter_come_first(self):
        _, items = self.list("order delta")
        self.assertEqual(self.titles(items), ["delta-repo", "alpha", "order"])

    def test_the_listed_account_name_in_front_only_filters(self):
        _, items = self.list("tektoncd trigger", view="tektoncd")
        self.assertEqual(self.titles(items), ["triggers", "tektoncd"])

    def test_view_lists_its_account(self):
        _, items = self.list("", view="tektoncd")
        self.assertEqual(self.titles(items)[-1], "tektoncd")
        self.assertEqual(len(items), 5)

    def test_view_and_typed_filter(self):
        _, items = self.list("private", view="tektoncd")
        self.assertEqual(self.titles(items), ["pipelines-as-code", "tektoncd"])

    def test_all_words_must_match(self):
        _, items = self.list("private pipeline", view="tektoncd")
        self.assertEqual(self.titles(items), ["pipelines-as-code", "tektoncd"])

    def test_filter_reads_the_description(self):
        _, items = self.list("event", view="tektoncd")
        self.assertEqual(self.titles(items), ["triggers", "tektoncd"])

    def test_repository_written_out_is_opened_directly(self):
        owner, items = self.list("tektoncd/pipelines-as-code")
        self.assertEqual(owner, "tektoncd")
        self.assertEqual(len(items), 1)
        self.assertEqual(
            items[0]["arg"], "https://github.com/tektoncd/pipelines-as-code"
        )

    def test_limit_applies_to_a_typed_account(self):
        _, items = self.list("tektoncd", limit=1)
        self.assertEqual(self.titles(items), ["pipeline", "tektoncd"])

    def test_the_whole_description_is_searched(self):
        long = "x" * 90 + " needle"
        with patch.dict(ACCOUNTS, {"long": account("long", [repo("one", description=long)])}):
            _, items = self.list("long needle")
        self.assertEqual(self.titles(items), ["one", "long"])

    def test_unknown_account_view_reports_an_error(self):
        with environment():
            owner, items = repos.list_items(
                "", "ghost", ["me"], self.options(any_owner=True)
            )
        self.assertEqual(owner, "ghost")
        self.assertEqual(items[0]["title"], "Account not found")
        self.assertFalse(items[0]["valid"])

    def test_views_are_the_configured_accounts(self):
        with environment(ALIAS_t="tektoncd"), patch(
            "sys.argv", ["repos.py", "--owners", "me,t", "raffi"]
        ), patch("builtins.print") as printed:
            repos.main()
        output = json.loads(printed.call_args[0][0])
        self.assertEqual(
            [view["id"] for view in output["views"]], ["@all", "me", "tektoncd"]
        )
        self.assertEqual(output["view"], "@all")

    def test_a_single_owner_has_no_all_view(self):
        with environment(), patch("sys.argv", ["repos.py", "--owners", "me"]), patch(
            "builtins.print"
        ) as printed:
            repos.main()
        output = json.loads(printed.call_args[0][0])
        self.assertEqual([view["id"] for view in output["views"]], ["me"])


class OwnersOnlyTests(ReposTestCase):
    """Without --any-owner only the configured accounts and aliases list."""

    def list(self, query, view="", owners=("me",), **kwargs):
        with environment(**kwargs):
            return repos.list_items(query, view, list(owners))

    def test_an_unconfigured_account_only_filters(self):
        with patch.object(repos, "account_repos", wraps=repos.account_repos) as looked:
            owner, items = self.list("tektoncd")
        self.assertEqual(owner, "me")
        self.assertEqual(
            self.titles(items), ["me", "No repository matches 'tektoncd'"]
        )
        self.assertEqual([call.args[0] for call in looked.call_args_list], ["me"])

    def test_a_configured_account_lists_it(self):
        owner, items = self.list("tektoncd pipeline", owners=("me", "tektoncd"))
        self.assertEqual(owner, "tektoncd")
        self.assertEqual(self.titles(items), ["pipeline", "pipelines-as-code", "tektoncd"])

    def test_an_alias_lists_its_account(self):
        owner, _ = self.list("t pac", ALIAS_t="tektoncd")
        self.assertEqual(owner, "tektoncd")

    def test_an_unconfigured_view_falls_back_to_the_default(self):
        owner, _ = self.list("", view="tektoncd")
        self.assertEqual(owner, "me")

    def test_a_configured_view_lists_its_account(self):
        owner, _ = self.list("", view="tektoncd", owners=("me", "tektoncd"))
        self.assertEqual(owner, "tektoncd")

    def test_flag_and_environment(self):
        for argv, env in ((["--any-owner"], {}), ([], {"ANY_OWNER": "1"})):
            with environment(**env), patch(
                "sys.argv", ["repos.py", *argv, "tektoncd"]
            ), patch("builtins.print") as printed:
                repos.main()
            self.assertEqual(json.loads(printed.call_args[0][0])["view"], "tektoncd")
        with environment(), patch("sys.argv", ["repos.py", "tektoncd"]), patch(
            "builtins.print"
        ) as printed:
            repos.main()
        self.assertEqual(json.loads(printed.call_args[0][0])["view"], "me")


def pushed(name, year, **fields):
    return repo(name, pushedAt=f"{year}-01-01T00:00:00Z", **fields)


class AllViewTests(ReposTestCase):
    """More than one --owners account mixes them in the default view."""

    def list(self, query, view="", owners=("me", "tektoncd"), **env):
        with environment(**env):
            return repos.list_items(query, view, list(owners))

    def mixed(self, accounts):
        return patch.dict(ACCOUNTS, accounts)

    def test_the_default_view_mixes_accounts_by_last_push(self):
        accounts = {
            "me": account("me", [pushed("new", 2024), pushed("old", 2020)]),
            "tektoncd": account("tektoncd", [pushed("mid", 2022)]),
        }
        with self.mixed(accounts):
            for view in ("", "@all"):
                owner, items = self.list("", view=view)
                self.assertEqual(owner, "@all")
                self.assertEqual(self.titles(items), ["new", "mid", "old"])

    def test_forks_come_after_other_repositories(self):
        accounts = {
            "me": account("me", [pushed("fork-a", 2024, isFork=True), pushed("mine", 2020)]),
            "tektoncd": account(
                "tektoncd", [pushed("fork-b", 2023, isFork=True), pushed("theirs", 2019)]
            ),
        }
        with self.mixed(accounts):
            _, items = self.list("")
        self.assertEqual(self.titles(items), ["mine", "theirs", "fork-a", "fork-b"])

    def test_forks_come_last_in_an_account_view_too(self):
        accounts = {"me": account("me", [repo("fork", isFork=True), repo("mine")])}
        with self.mixed(accounts):
            _, items = self.list("", owners=("me",))
        self.assertEqual(self.titles(items), ["mine", "fork", "me"])

    def test_rows_name_their_owner(self):
        accounts = {
            "me": account("me", [repo("a", nameWithOwner="me/a")]),
            "tektoncd": account("tektoncd", [repo("b", nameWithOwner="tektoncd/b")]),
        }
        with self.mixed(accounts):
            _, items = self.list("")
        owners = {
            repos.ANSI.sub("", item["subtitle"]).split(" \u00b7 ")[0] for item in items
        }
        self.assertEqual(owners, {"me", "tektoncd"})

    def test_a_filter_searches_every_account(self):
        owner, items = self.list("pipeline")
        self.assertEqual(owner, "@all")
        self.assertEqual(
            self.titles(items), ["pipelines", "pipeline", "pipelines-as-code"]
        )

    def test_a_typed_account_or_alias_lists_only_it(self):
        owner, items = self.list("tektoncd trigger")
        self.assertEqual(
            (owner, self.titles(items)), ("tektoncd", ["triggers", "tektoncd"])
        )
        owner, items = self.list("me notes")
        self.assertEqual((owner, self.titles(items)), ("me", ["notes", "me"]))

    def test_an_account_view_lists_only_it(self):
        owner, items = self.list("", view="tektoncd")
        self.assertEqual(owner, "tektoncd")
        self.assertEqual(len(items), 5)

    def test_nothing_matching_says_so(self):
        _, items = self.list("zzz")
        self.assertEqual(len(items), 1)
        self.assertFalse(items[0]["valid"])

    def test_a_repository_of_any_listed_account_filters_with_any_owner(self):
        # "twin" is an account and a repository of acme, the second account:
        # the All view lists acme too, so the word filters.
        accounts = {
            "mine": account("mine", [repo("raffi")]),
            "acme": account("acme", [repo("twin")]),
            "twin": account("twin", [repo("x")]),
        }
        with self.mixed(accounts), environment():
            owner, items = repos.list_items(
                "twin", "", ["mine", "acme"], self.options(any_owner=True)
            )
        self.assertEqual(owner, "@all")
        self.assertEqual(self.titles(items), ["twin"])


class MainTests(ReposTestCase):
    def run_main(self, *argv, env=None):
        with environment(**(env or {})), patch("sys.argv", ["repos.py", *argv]), patch(
            "builtins.print"
        ) as printed:
            self.assertEqual(repos.main(), 0)
        return json.loads(printed.call_args[0][0])

    def test_direct_repository_needs_no_github_call(self):
        def boom(*args, **kwargs):
            raise AssertionError("GitHub should not be asked")

        with patch.object(repos, "graphql", boom):
            output = self.run_main("chmouel/raffi")
        self.assertEqual(
            output["items"][0]["arg"], "https://github.com/chmouel/raffi"
        )
        # Views are left alone, so Ctrl+Tab keeps working after the query.
        self.assertNotIn("views", output)
        self.assertNotIn("view", output)

    def test_path_flag(self):
        output = self.run_main("--path", "pulls", "me", "raffi")
        self.assertEqual(
            output["items"][0]["arg"], "https://github.com/owner/raffi/pulls"
        )

    def test_actions_flag(self):
        output = self.run_main("--actions", "pulls,issues", "me", "raffi")
        item = output["items"][0]
        titles = [entry["title"] for entry in item["actions"]]
        self.assertEqual(titles[:2], ["Pull requests", "Issues"])
        # Both pages are live, so they are also listed under the row.
        self.assertEqual(
            [child["title"] for child in item["children"]],
            ["Open owner/raffi", "Pull requests", "Issues"],
        )

    def test_pages_flag(self):
        output = self.run_main("--pages", "pulls,issues", "me", "raffi")
        titles = [child["title"] for child in output["items"][0]["children"]]
        self.assertEqual(titles, ["Open owner/raffi", "Pull requests", "Issues"])

    def test_output_hints_the_cache_window(self):
        with patch.object(repos, "CACHE_REPOS", "24h"):
            output = self.run_main("--owners", "me", "raffi")
        self.assertEqual(output["cache"], {"seconds": 86400})

    def test_the_output_asks_for_the_picked_repositories_first(self):
        output = self.run_main("--owners", "me", "raffi")
        self.assertEqual(output["sort"], "usage")

    def test_no_usage_sort_keeps_the_script_order(self):
        for argv, env in ((["--no-usage-sort"], {}), ([], {"USAGE_SORT": "0"})):
            output = self.run_main("--owners", "me", *argv, env=env)
            self.assertNotIn("sort", output)
            self.assertEqual(output["cache"], {"seconds": 600}, "the rest is unchanged")

    def test_failure_rows_ask_for_no_sorting(self):
        def offline(*args, **kwargs):
            raise repos.GhError("offline")

        with patch.object(repos, "graphql", offline):
            output = self.run_main("--owners", "me", "raffi")
        self.assertNotIn("sort", output)

    def test_github_errors_are_not_kept(self):
        def offline(*args, **kwargs):
            raise repos.GhError("offline")

        with patch.object(repos, "graphql", offline):
            output = self.run_main("--owners", "me", "raffi")
        self.assertEqual(output["cache"], {"seconds": 0})
        self.assertEqual(output["items"][0]["title"], "GitHub error")

    def test_an_unknown_account_is_not_kept(self):
        output = self.run_main("--owners", "ghost")
        self.assertEqual(output["items"][0]["title"], "Account not found")
        self.assertEqual(output["cache"], {"seconds": 0})

    def test_missing_cache_is_per_host(self):
        self.assertEqual(repos.cache_dir().name, repos.API_HOST)

    def test_cache_files_stay_under_the_cache_directory(self):
        for path in (repos.missing_file(), repos.login_file()):
            self.assertEqual(path.parent, repos.CACHE_DIR)

    def test_login_is_cached_beside_the_other_caches(self):
        with environment():
            self.assertEqual(repos.login(), "me")
            self.assertEqual(repos.read_cache(repos.login_file())["login"], "me")

    def test_a_blank_setting_does_not_crash(self):
        output = self.run_main(
            "--pages",
            "pulls",
            "me",
            "raffi",
            env={
                "LIVE_ITEMS": "",
                "LIVE_PREFETCH": "",
                "LIVE_BUDGET": "",
                "LIVE_RETRY": "",
                "OWNERS": "",
            },
        )
        self.assertEqual(output["view"], "me")
        self.assertEqual(output["cache"], {"seconds": 60})

    def test_live_zero_in_the_environment_turns_the_menus_off(self):
        output = self.run_main("--pages", "pulls,issues", env={"LIVE": "0"})
        children = output["items"][0]["children"]
        self.assertEqual(
            [repos.ANSI.sub("", child["title"]) for child in children],
            ["Open owner/raffi", "Pull requests", "Issues"],
        )
        self.assertTrue(all("children" not in child for child in children))
        self.assertEqual(self.queued(), [])

    def test_a_list_that_is_still_filling_asks_for_a_short_cache(self):
        with patch.object(repos, "CACHE_REPOS", "24h"):
            output = self.run_main("--pages", "pulls", "me", "raffi")
        # Raffi would serve the loading rows for a day otherwise.
        self.assertEqual(output["cache"], {"seconds": 60})

    def test_a_typed_repository_carries_the_same_short_cache(self):
        with patch.object(repos, "CACHE_REPOS", "24h"):
            output = self.run_main("--pages", "pulls", "owner/raffi")
        self.assertEqual(output["cache"], {"seconds": 60})
        self.assertEqual(self.queued(), ["owner/raffi"])


class ActionTests(ReposTestCase):
    def test_parse_actions(self):
        self.assertEqual(
            repos.parse_actions("pulls,issues:Issues,graphs/traffic,/tags/"),
            (
                ("pulls", "Pull requests"),
                ("issues", "Issues"),
                ("graphs/traffic", "Traffic"),
                ("tags", "Tags"),
            ),
        )

    def test_parse_actions_labels_unknown_paths(self):
        self.assertEqual(
            repos.parse_actions("my-page"), (("my-page", "My Page"),)
        )

    def test_parse_actions_ignores_blanks(self):
        self.assertEqual(repos.parse_actions(" , ,"), ())

    def test_path_changes_what_a_row_opens(self):
        item = repos.build_item(
            repo("raffi"), self.options(path="pulls")
        )
        self.assertEqual(item["arg"], "https://github.com/owner/raffi/pulls")
        self.assertEqual(item["variables"]["REPO_URL"], "https://github.com/owner/raffi")
        self.assertIn("Pull requests", item["subtitle"])

    def test_path_applies_to_a_typed_repository(self):
        item = repos.repo_item("owner/raffi", self.options(path="releases"))
        self.assertEqual(item["arg"], "https://github.com/owner/raffi/releases")

    def titles_of(self, entries):
        return [entry["title"] for entry in entries]

    def test_actions_list_pages_then_copy_entries(self):
        item = repos.build_item(
            repo("raffi"),
            self.options(actions=(("pulls", "Pull requests"), ("issues", "Issues"))),
        )
        self.assertEqual(
            self.titles_of(item["actions"]),
            [
                "Pull requests",
                "Issues",
                "Copy URL",
                "Copy Markdown link",
                "Copy SSH clone URL",
                "Copy HTTPS clone URL",
                "Copy name",
            ],
        )
        self.assertEqual(
            item["actions"][0]["arg"], "https://github.com/owner/raffi/pulls"
        )
        # Both pages are live, so they are also listed under the row.
        self.assertEqual(
            self.titles_of(item["children"]),
            ["Open owner/raffi", "Pull requests", "Issues"],
        )
        self.assertEqual(item["arg"], "https://github.com/owner/raffi")

    def test_copy_entries_copy_the_right_values(self):
        item = repos.build_item(repo("raffi"))
        by_title = {entry["title"]: entry for entry in item["actions"]}
        self.assertEqual(by_title["Copy URL"]["arg"], "https://github.com/owner/raffi")
        self.assertEqual(
            by_title["Copy Markdown link"]["arg"],
            "[owner/raffi](https://github.com/owner/raffi)",
        )
        self.assertEqual(
            by_title["Copy SSH clone URL"]["arg"], "git@github.com:owner/raffi.git"
        )
        self.assertEqual(
            by_title["Copy HTTPS clone URL"]["arg"],
            "https://github.com/owner/raffi.git",
        )
        self.assertEqual(by_title["Copy name"]["arg"], "owner/raffi")
        self.assertTrue(all(entry["action"] == "copy" for entry in item["actions"]))

    def test_pages_make_enter_list_them(self):
        item = repos.build_item(
            repo("raffi"),
            self.options(pages=(("pulls", "Pull requests"), ("issues", "Issues"))),
        )
        self.assertEqual(
            self.titles_of(item["children"]),
            ["Open owner/raffi", "Pull requests", "Issues"],
        )
        self.assertEqual(
            [child["arg"] for child in item["children"]],
            [
                "https://github.com/owner/raffi",
                "https://github.com/owner/raffi/pulls",
                "https://github.com/owner/raffi/issues",
            ],
        )
        self.assertEqual(
            item["children"][1]["variables"]["REPO_NAME"], "owner/raffi"
        )
        # Enter still means "list the pages", so the row's own arg stays.
        self.assertEqual(item["arg"], "https://github.com/owner/raffi")

    def test_a_page_link_carries_a_compact_preview(self):
        item = repos.build_item(
            repo("raffi"),
            self.options(pages=(("tags", "Tags"), ("releases", "Releases"))),
        )
        leaf = item["children"][2]
        url = "https://github.com/owner/raffi/releases"
        self.assertEqual(leaf["title"], "Releases")
        self.assertEqual(leaf["arg"], url)
        self.assertEqual(leaf["subtitle"], url)
        self.assertEqual(leaf["variables"]["REPO_NAME"], "owner/raffi")
        self.assertEqual(
            leaf["preview"],
            {
                "text": f"Enter opens {url}\nCtrl+Enter copies it.",
                "icon": repos.ICON,
                "dynamic": False,
            },
        )
        self.assertEqual(
            self.titles_of(leaf["actions"]), ["Copy URL", "Copy Markdown link"]
        )
        self.assertEqual(leaf["actions"][0]["arg"], url)
        self.assertEqual(
            leaf["actions"][1]["arg"], f"[Releases \u00b7 owner/raffi]({url})"
        )

    def test_the_repository_link_carries_a_compact_preview(self):
        item = repos.build_item(
            repo("raffi"), self.options(pages=(("tags", "Tags"),))
        )
        leaf = item["children"][0]
        url = "https://github.com/owner/raffi"
        self.assertEqual(leaf["title"], "Open owner/raffi")
        self.assertEqual(
            leaf["preview"],
            {
                "text": f"Enter opens {url}\nCtrl+Enter copies it.",
                "icon": repos.ICON,
                "dynamic": False,
            },
        )
        self.assertEqual(leaf["actions"][1]["arg"], f"[owner/raffi]({url})")

    def test_pages_carry_their_own_context_menu(self):
        item = repos.build_item(
            repo("raffi"), self.options(pages=(("pulls", "Pull requests"),))
        )
        pulls = item["children"][1]
        self.assertEqual(
            self.titles_of(pulls["actions"]),
            ["Open on GitHub", "Copy URL", "Copy Markdown link"],
        )
        self.assertEqual(
            pulls["actions"][2]["arg"],
            "[Pull requests \u00b7 owner/raffi](https://github.com/owner/raffi/pulls)",
        )
        self.assertEqual(
            item["children"][0]["actions"][1]["arg"],
            "[owner/raffi](https://github.com/owner/raffi)",
        )

    def test_no_children_without_pages(self):
        self.assertNotIn("children", repos.build_item(repo("raffi")))

    def test_path_applies_to_a_typed_repository_menu(self):
        item = repos.repo_item(
            "owner/raffi",
            self.options(
                path="pulls", actions=(("issues", "Issues"),), pages=(("tags", "Tags"),)
            ),
        )
        self.assertEqual(item["arg"], "https://github.com/owner/raffi/pulls")
        self.assertEqual(item["actions"][0]["arg"], "https://github.com/owner/raffi/issues")
        self.assertEqual(
            [child["arg"] for child in item["children"]],
            [
                "https://github.com/owner/raffi",
                "https://github.com/owner/raffi/tags",
                "https://github.com/owner/raffi/issues",
            ],
        )
        # issues is a live page, so it is a menu rather than a plain link.
        self.assertIn("children", item["children"][2])
        self.assertNotIn("children", item["children"][1])


class LiveMenuTests(ReposTestCase):
    """pulls and issues list the real open pull requests and issues."""

    def write_live(self, at=None, pulls=None, issues=None, first=None):
        pulls = LIVE["owner/raffi"]["pullRequests"] if pulls is None else pulls
        issues = LIVE["owner/raffi"]["issues"] if issues is None else issues
        repos.write_cache(
            repos.live_file("owner/raffi"),
            {
                "at": time.time() if at is None else at,
                "first": repos.FETCH_ITEMS if first is None else first,
                "pull_requests": pulls,
                "issues": issues,
            },
        )

    def pages_item(self, **kwargs):
        kwargs.setdefault(
            "pages", (("pulls", "Pull requests"), ("issues", "Issues"))
        )
        return repos.build_item(repo("raffi"), self.options(**kwargs))

    def run_main(self, *argv, env=None):
        with environment(**(env or {})), patch(
            "sys.argv", ["repos.py", *argv]
        ), patch("builtins.print") as printed:
            self.assertEqual(repos.main(), 0)
        return json.loads(printed.call_args[0][0])

    def plain(self, text):
        return repos.ANSI.sub("", text)

    def test_a_live_page_is_a_menu_with_its_count(self):
        self.write_live()
        pulls = self.pages_item()["children"][1]
        self.assertEqual(pulls["title"], "Pull requests \u00b7 34")
        self.assertEqual(pulls["subtitle"], "34 open \u00b7 owner/raffi")
        self.assertEqual(pulls["variables"]["REPO_NAME"], "owner/raffi")
        self.assertEqual(pulls["arg"], "https://github.com/owner/raffi/pulls")
        self.assertEqual(
            [entry["title"] for entry in pulls["actions"]],
            ["Open on GitHub", "Copy URL", "Copy Markdown link"],
        )
        self.assertFalse(pulls["preview"]["dynamic"])

    def test_pull_request_rows_carry_what_pr_actions_need(self):
        self.write_live()
        pulls = self.pages_item()["children"][1]
        first = pulls["children"][0]
        subtitle = self.plain(first["subtitle"])
        self.assertEqual(first["title"], "#412 Add clipboard view")
        self.assertIn("@chmouel", subtitle)
        self.assertIn("approved", subtitle)
        self.assertIn("updated 3h ago", subtitle)
        self.assertEqual(first["arg"], "https://github.com/owner/raffi/pull/412")
        self.assertEqual(first["variables"]["PR_URL"], first["arg"])
        self.assertEqual(first["variables"]["PR_REPO"], "owner/raffi")
        self.assertEqual(first["variables"]["PR_NUMBER"], "412")
        self.assertEqual(first["variables"]["PR_BRANCH"], "branch-412")
        self.assertEqual(first["variables"]["REPO_NAME"], "owner/raffi")
        self.assertEqual(first["actions"][0]["title"], "Copy URL")
        self.assertEqual(first["actions"][0]["arg"], first["arg"])
        self.assertEqual(first["preview"]["dynamic"], False)

    def test_the_second_pull_request_shows_draft_and_review(self):
        self.write_live()
        second = self.pages_item()["children"][1]["children"][1]
        subtitle = self.plain(second["subtitle"])
        self.assertIn("draft", subtitle)
        self.assertIn("changes requested", subtitle)

    def test_a_pull_request_colours_its_review_decision(self):
        self.write_live()
        first = self.pages_item()["children"][1]["children"][0]
        self.assertIn(repos.GREEN + "approved", first["subtitle"])
        second = self.pages_item()["children"][1]["children"][1]
        self.assertIn(repos.RED + "changes requested", second["subtitle"])

    def test_issue_rows_carry_what_issue_actions_need(self):
        self.write_live()
        issues = self.pages_item()["children"][2]
        self.assertEqual(issues["title"], "Issues \u00b7 2")
        row = issues["children"][0]
        subtitle = self.plain(row["subtitle"])
        self.assertEqual(row["title"], "#388 Crash on empty config")
        self.assertIn("@chmouel", subtitle)
        self.assertIn("bug, crash", subtitle)
        self.assertIn("updated 3h ago", subtitle)
        self.assertEqual(row["variables"]["ISSUE_NUMBER"], "388")
        self.assertEqual(row["variables"]["ISSUE_REPO"], "owner/raffi")
        self.assertEqual(
            row["variables"]["ISSUE_URL"],
            "https://github.com/owner/raffi/issues/388",
        )
        self.assertEqual(row["actions"][0]["arg"], row["arg"])

    def test_the_last_row_opens_the_whole_page(self):
        self.write_live()
        children = self.pages_item()["children"]
        self.assertEqual(
            children[1]["children"][-1]["title"], "All pull requests on GitHub"
        )
        self.assertEqual(
            children[1]["children"][-1]["arg"],
            "https://github.com/owner/raffi/pulls",
        )
        self.assertEqual(children[2]["children"][-1]["title"], "All issues on GitHub")
        self.assertEqual(
            [entry["title"] for entry in children[2]["children"][-1]["actions"]],
            ["Copy URL", "Copy Markdown link"],
        )

    def test_link_rows_keep_their_titles_and_markdown_labels(self):
        self.write_live()
        children = self.pages_item(
            pages=(("pulls", "Pull requests"), ("tags", "Tags"))
        )["children"]
        self.assertEqual(
            [child["title"] for child in children],
            ["Open owner/raffi", "Pull requests \u00b7 34", "Tags"],
        )
        links = [children[0], children[2], children[1]["children"][-1]]
        self.assertEqual(
            [leaf["title"] for leaf in links],
            ["Open owner/raffi", "Tags", "All pull requests on GitHub"],
        )
        self.assertEqual(
            [leaf["actions"][-1]["arg"] for leaf in links],
            [
                "[owner/raffi](https://github.com/owner/raffi)",
                "[Tags \u00b7 owner/raffi](https://github.com/owner/raffi/tags)",
                (
                    "[Pull requests \u00b7 owner/raffi]"
                    "(https://github.com/owner/raffi/pulls)"
                ),
            ],
        )
        # A plain string preview would make Raffi re-run the script for a row
        # that gains nothing, so every link opts out of dynamic previews
        for leaf in links:
            self.assertEqual(
                leaf["preview"],
                {
                    "text": f"Enter opens {leaf['arg']}\nCtrl+Enter copies it.",
                    "icon": repos.ICON,
                    "dynamic": False,
                },
            )

    def test_live_menus_and_their_rows_keep_a_preview_object(self):
        self.write_live()
        pulls = self.pages_item()["children"][1]
        for item in (pulls, pulls["children"][0]):
            for key in ("title", "subtitle", "icon", "text"):
                self.assertIn(key, item["preview"])

    def test_a_cold_live_menu_still_ends_with_a_plain_link(self):
        leaf = self.pages_item()["children"][1]["children"][-1]
        self.assertEqual(leaf["title"], "All pull requests on GitHub")
        self.assertEqual(
            leaf["preview"],
            {
                "text": (
                    "Enter opens https://github.com/owner/raffi/pulls\n"
                    "Ctrl+Enter copies it."
                ),
                "icon": repos.ICON,
                "dynamic": False,
            },
        )

    def test_a_cold_cache_shows_a_placeholder_and_the_whole_page(self):
        pulls = self.pages_item()["children"][1]
        placeholder = pulls["children"][0]
        self.assertEqual(placeholder["title"], "Pull requests")
        self.assertFalse(placeholder["valid"])
        self.assertIn("loading in the background", placeholder["subtitle"])
        self.assertIn("Ctrl+R", placeholder["subtitle"])
        # The menu is never a dead end, even before its rows arrive.
        self.assertEqual(pulls["children"][-1]["title"], "All pull requests on GitHub")
        self.assertEqual(pulls["children"][-1]["arg"], pulls["arg"])
        self.assertEqual(repos.PENDING, ["owner/raffi"])
        self.assertEqual(self.prefetched, [])

    def test_a_freshly_empty_list_is_the_plain_page_link(self):
        self.write_live(
            pulls={"totalCount": 0, "nodes": []},
            issues={"totalCount": 0, "nodes": []},
        )
        children = self.pages_item()["children"]
        self.assertEqual(
            [child["title"] for child in children],
            ["Open owner/raffi", "Pull requests", "Issues"],
        )
        # The row Raffi shows is the one a --no-live run draws: no menu, no
        # count, the same page URL, variables and copy actions.
        self.assertEqual(children[1], self.pages_item(live=False)["children"][1])
        self.assertNotIn("children", children[1])
        self.assertEqual(children[1]["arg"], "https://github.com/owner/raffi/pulls")
        self.assertEqual(children[2], self.pages_item(live=False)["children"][2])

    def test_only_the_empty_list_collapses(self):
        self.write_live(
            pulls={"totalCount": 0, "nodes": []},
            issues={
                "totalCount": 3,
                "nodes": [live_issue("owner/raffi", 388, "Crash on empty config")],
            },
        )
        pulls, issues = self.pages_item()["children"][1:]
        self.assertEqual(pulls["title"], "Pull requests")
        self.assertNotIn("children", pulls)
        self.assertEqual(issues["title"], "Issues \u00b7 3")
        self.assertEqual(issues["children"][0]["title"], "#388 Crash on empty config")

    def test_a_collapsed_list_queues_no_fetch(self):
        self.write_live(
            pulls={"totalCount": 0, "nodes": []},
            issues={"totalCount": 0, "nodes": []},
        )
        self.pages_item()
        self.assertEqual(repos.PENDING, [])
        self.assertEqual(self.queued(), [])

    def test_a_count_whose_first_page_is_empty_collapses(self):
        # Nothing to list even though the count says otherwise.
        self.write_live(pulls={"totalCount": 3, "nodes": []})
        self.assertNotIn("children", self.pages_item()["children"][1])

    def test_a_missing_section_collapses(self):
        self.write_live(pulls={})
        self.assertNotIn("children", self.pages_item()["children"][1])

    def test_ctrl_r_keeps_the_menu_of_a_freshly_empty_list(self):
        self.write_live(
            pulls={"totalCount": 0, "nodes": []},
            issues={"totalCount": 0, "nodes": []},
        )
        with patch.object(repos.ghcommon, "REFRESH", True):
            pulls = self.pages_item()["children"][1]
        self.assertEqual(pulls["title"], "Pull requests \u00b7 0")
        self.assertEqual(repos.PENDING, ["owner/raffi"])

    def test_a_stale_empty_cache_still_shows_its_menus(self):
        self.write_live(
            at=0.0,
            pulls={"totalCount": 0, "nodes": []},
            issues={"totalCount": 0, "nodes": []},
        )
        pulls = self.pages_item()["children"][1]
        self.assertEqual(pulls["title"], "Pull requests \u00b7 0")
        self.assertEqual(
            pulls["children"][-1]["title"], "All pull requests on GitHub"
        )
        self.assertEqual(repos.PENDING, ["owner/raffi"])

    def test_an_empty_cache_asking_for_fewer_rows_still_shows_its_menu(self):
        # A cache that a bigger --live-items would fetch again is not settled.
        self.write_live(first=1, pulls={"totalCount": 0, "nodes": []})
        self.assertEqual(
            self.pages_item()["children"][1]["title"], "Pull requests \u00b7 0"
        )

    def test_a_cold_cache_keeps_its_menu_with_the_loading_row(self):
        pulls = self.pages_item()["children"][1]
        self.assertEqual(
            [leaf["title"] for leaf in pulls["children"]],
            ["Pull requests", "All pull requests on GitHub"],
        )

    def test_the_rows_are_read_back_from_the_cache(self):
        self.write_live()
        fetched = []

        def recording(query, cache=None, timeout=30, **variables):
            fetched.append("pullRequests" in query)
            return graphql(query, cache=cache, timeout=timeout, **variables)

        with patch.object(repos, "graphql", recording):
            output = self.run_main("--pages", "pulls", "me", "raffi")
        # A fresh cache is served as is: no live fetch, no queue, no child.
        self.assertFalse(any(fetched))
        self.assertEqual(repos.PENDING, [])
        self.assertEqual(self.queued(), [])
        self.assertEqual(self.prefetched, [])
        pulls = output["items"][0]["children"][1]
        self.assertEqual(pulls["children"][0]["title"], "#412 Add clipboard view")

    def test_a_stale_cache_still_lists_its_rows_and_queues_a_fetch(self):
        self.write_live(at=0.0)
        pulls = self.pages_item()["children"][1]
        self.assertEqual(pulls["children"][0]["title"], "#412 Add clipboard view")
        self.assertEqual(repos.PENDING, ["owner/raffi"])

    def test_main_starts_a_prefetch_for_what_it_drew(self):
        self.run_main("--pages", "pulls", "me", "raffi")
        # Only the rows the query kept queue a fetch, in the order they show.
        self.assertEqual(self.queued(), ["owner/raffi"])
        self.assertEqual(self.prefetched, [True])

    def test_a_filtered_out_repository_is_not_prefetched(self):
        self.run_main("--pages", "pulls", "me", "notes")
        self.assertEqual(self.queued(), ["owner/notes"])
        self.assertNotIn("owner/raffi", repos.PENDING)

    def test_ctrl_r_refreshes_a_cache_that_is_still_fresh(self):
        self.write_live()
        with patch.object(repos.ghcommon, "REFRESH", True):
            self.pages_item()
        self.assertEqual(repos.PENDING, ["owner/raffi"])

    def test_ctrl_r_refetches_a_fresh_cache_and_shows_the_new_rows(self):
        calls = []

        def changing(query, cache=None, timeout=30, **variables):
            if "pullRequests" not in query:
                return graphql(query, cache=cache, timeout=timeout, **variables)
            calls.append(f"{variables.get('owner')}/{variables.get('name')}")
            if len(calls) == 1:
                return {"repository": LIVE["owner/raffi"]}
            landed = live_pull("owner/raffi", 500, "Landed after Ctrl+R")
            return {
                "repository": {
                    "pullRequests": {"totalCount": 1, "nodes": [landed]},
                    "issues": {"totalCount": 0, "nodes": []},
                }
            }

        repos.write_queue(["owner/raffi"])
        with patch.object(repos, "graphql", changing):
            self.assertEqual(repos.run_prefetch(), 0)
            pulls = self.run_main("--pages", "pulls", "me", "raffi")
            self.assertEqual(
                pulls["items"][0]["children"][1]["children"][0]["title"],
                "#412 Add clipboard view",
            )
            with patch.object(repos.ghcommon, "REFRESH", True):
                output = self.run_main("--pages", "pulls", "me", "raffi")
        # The cache was still fresh, yet Ctrl+R fetched the row again: the
        # queue carries the forced names past the freshness check.
        self.assertEqual(calls, ["owner/raffi", "owner/raffi"])
        pulls = output["items"][0]["children"][1]
        self.assertEqual(pulls["title"], "Pull requests \u00b7 1")
        self.assertEqual(pulls["children"][0]["title"], "#500 Landed after Ctrl+R")
        self.assertEqual(self.queued(), [])

    def test_ctrl_r_reaches_the_fetch_of_a_fresh_typed_repository(self):
        self.write_live()
        calls = []

        def counting(query, cache=None, timeout=30, **variables):
            if "pullRequests" in query:
                calls.append(f"{variables.get('owner')}/{variables.get('name')}")
            return graphql(query, cache=cache, timeout=timeout, **variables)

        with patch.object(repos, "graphql", counting), patch.object(
            repos.ghcommon, "REFRESH", True
        ):
            output = self.run_main("--pages", "pulls", "owner/raffi")
        # The fresh cache was fetched anyway, and the rows are live.
        self.assertEqual(calls, ["owner/raffi"])
        pulls = output["items"][0]["children"][1]
        self.assertEqual(pulls["title"], "Pull requests \u00b7 34")
        self.assertEqual(self.queued(), [])

    def test_a_forced_name_beats_a_fresh_cache_in_the_next_batch(self):
        self.write_live()
        repos.queue_pending(["owner/raffi"], refresh=True)
        self.assertEqual(repos.read_queue().force, ["owner/raffi"])
        batch = repos.next_batch(1)
        self.assertEqual(batch.names, ["owner/raffi"])
        self.assertTrue(repos.ack_batch(batch))
        # The marker went with the batch, so the name is not forced again.
        self.assertEqual(repos.read_queue().entries, [])

    def test_a_fresh_name_nobody_forced_is_skipped_by_the_next_batch(self):
        self.write_live()
        repos.write_queue(["owner/raffi"])
        self.assertEqual(repos.next_batch(1).names, [])

    def test_a_forced_name_whose_fetch_failed_is_not_forced_forever(self):
        self.write_live()
        repos.queue_pending(["owner/raffi"], refresh=True)

        def failing(query, cache=None, timeout=30, **variables):
            raise repos.GhError("boom")

        with patch.object(repos, "graphql", failing):
            repos.load_pending()
        # The forced fetch was tried and acknowledged; another Ctrl+R marks
        # the name again.
        self.assertEqual(repos.read_queue().entries, [])

    def test_a_shorter_cache_is_fetched_again(self):
        self.write_live(first=1)
        self.pages_item()
        self.assertEqual(repos.PENDING, ["owner/raffi"])

    def test_a_settled_list_keeps_the_repos_window(self):
        self.write_live()
        with patch.object(repos, "CACHE_REPOS", "24h"):
            output = self.run_main("--pages", "pulls", "me", "raffi")
        self.assertEqual(output["cache"], {"seconds": 86400})

    def test_a_settled_typed_repository_keeps_the_repos_window_too(self):
        self.write_live()
        with patch.object(repos, "CACHE_REPOS", "24h"):
            output = self.run_main("--pages", "pulls", "owner/raffi")
        self.assertEqual(output["cache"], {"seconds": 86400})

    def test_a_filling_list_keeps_the_live_retry_window(self):
        with patch.object(repos, "CACHE_REPOS", "24h"), patch.object(
            repos, "LIVE_RETRY", "5s"
        ):
            output = self.run_main("--pages", "pulls", "me", "raffi")
        self.assertEqual(output["cache"], {"seconds": 5})

    def test_a_live_retry_under_a_second_still_keeps_the_list_a_second(self):
        with patch.object(repos, "CACHE_REPOS", "24h"), patch.object(
            repos, "LIVE_RETRY", "0s"
        ):
            output = self.run_main("--pages", "pulls", "me", "raffi")
        # Seconds: 0 reads as a failed refresh, so the window floors at one.
        self.assertEqual(output["cache"], {"seconds": 1})

    def test_the_live_items_flag_asks_github_for_more_rows(self):
        sizes = []

        def recording(query, cache=None, timeout=30, **variables):
            sizes.append(variables.get("first"))
            return graphql(query, cache=cache, timeout=timeout, **variables)

        with patch.object(repos, "graphql", recording), patch.object(
            repos.ghcommon, "REFRESH", True
        ):
            self.run_main("--pages", "pulls", "--live-items", "50", "me", "raffi")
        self.assertEqual(repos.FETCH_ITEMS, 50)
        self.assertIn(50, sizes)

    def test_a_live_items_of_one_fetches_one_row(self):
        sizes = []

        def recording(query, cache=None, timeout=30, **variables):
            if "pullRequests" in query:
                sizes.append(variables.get("first"))
            return graphql(query, cache=cache, timeout=timeout, **variables)

        with patch.object(repos, "graphql", recording), patch.object(
            repos.ghcommon, "REFRESH", True
        ):
            output = self.run_main(
                "--pages", "pulls", "--live-items", "1", "me", "raffi"
            )
        self.assertEqual(sizes, [1])
        pulls = output["items"][0]["children"][1]
        # One pull request, then the row opening the whole page.
        self.assertEqual(len(pulls["children"]), 2)

    def test_a_bigger_fetch_is_not_fetched_again_by_a_smaller_draw(self):
        with patch.object(repos.ghcommon, "REFRESH", True):
            self.run_main("--pages", "pulls", "--live-items", "50", "me", "raffi")
        entry = repos.read_cache(repos.live_file("owner/raffi"))
        self.assertEqual(entry["first"], 50)
        output = self.run_main("--pages", "pulls", "me", "raffi")
        self.assertEqual(repos.PENDING, [])
        self.assertEqual(self.queued(), [])
        self.assertEqual(self.prefetched, [])
        pulls = output["items"][0]["children"][1]
        self.assertEqual(pulls["title"], "Pull requests \u00b7 34")

    def test_one_failing_repository_does_not_lose_the_others(self):
        def half(query, cache=None, timeout=30, **variables):
            if variables.get("name") == "raffi":
                raise PermissionError("gh is not executable")
            return graphql(query, cache=cache, timeout=timeout, **variables)

        with patch.object(repos, "graphql", half):
            repos.fetch_quietly("owner/raffi")
            repos.fetch_quietly("tektoncd/pipeline")
        self.assertEqual(
            repos.read_cache(repos.live_file("owner/raffi")), {}
        )
        self.assertEqual(
            repos.read_cache(repos.live_file("tektoncd/pipeline"))["first"],
            repos.FETCH_ITEMS,
        )

    def test_the_cache_is_written_through_a_temporary_of_its_own(self):
        names = []
        real = repos.Path.replace

        def recording(self, target):
            names.append(self.name)
            return real(self, target)

        with patch.object(repos.Path, "replace", recording):
            self.write_live()
            self.write_live()
        self.assertEqual(len(names), 2)
        self.assertNotEqual(names[0], names[1])
        self.assertTrue(all(name.endswith(".part") for name in names))
        self.assertEqual(list(repos.live_file("owner/raffi").parent.glob("*.part")), [])

    def test_threads_writing_one_cache_leave_it_readable(self):
        file = repos.live_file("owner/raffi")
        payloads = [
            {"at": float(number), "nodes": [number] * 500} for number in range(8)
        ]

        def write(payload):
            repos.write_cache(file, payload)

        with ThreadPoolExecutor(max_workers=8) as pool:
            list(pool.map(write, payloads))
        self.assertIn(repos.read_cache(file), payloads)
        self.assertEqual(list(file.parent.glob("*.part")), [])

    def test_no_live_restores_the_plain_links(self):
        for argv, env in ((["--no-live"], {}), ([], {"LIVE": "0"})):
            output = self.run_main("--pages", "pulls,issues", *argv, env=env)
            children = output["items"][0]["children"]
            self.assertEqual(
                [child["title"] for child in children],
                ["Open owner/raffi", "Pull requests", "Issues"],
            )
            self.assertTrue(all("children" not in child for child in children))
        self.assertEqual(repos.PENDING, [])

    def test_live_items_caps_the_rows(self):
        self.write_live()
        options = self.options(
            pages=(("pulls", "Pull requests"),), live_items=1
        )
        pulls = repos.build_item(repo("raffi"), options)["children"][1]
        self.assertEqual(len(pulls["children"]), 2)
        self.assertEqual(pulls["title"], "Pull requests \u00b7 34")

    def test_the_live_items_flag_and_environment_cap_the_rows(self):
        self.write_live()
        for argv, env in (
            (("--live-items", "1"), {}),
            ((), {"LIVE_ITEMS": "1"}),
        ):
            output = self.run_main("--pages", "pulls", "me", "raffi", *argv, env=env)
            pulls = output["items"][0]["children"][1]
            self.assertEqual(len(pulls["children"]), 2)

    def test_ctrl_r_loads_the_rows_in_the_same_run(self):
        with patch.object(repos.ghcommon, "REFRESH", True):
            output = self.run_main("--pages", "pulls", "me", "raffi")
        pulls = output["items"][0]["children"][1]
        self.assertEqual(pulls["title"], "Pull requests \u00b7 34")
        self.assertEqual(pulls["children"][0]["title"], "#412 Add clipboard view")
        self.assertEqual(self.prefetched, [])

    def test_ctrl_r_loads_a_typed_repository_at_once(self):
        with patch.object(repos.ghcommon, "REFRESH", True):
            output = self.run_main("--pages", "pulls", "owner/raffi")
        pulls = output["items"][0]["children"][1]
        self.assertEqual(pulls["title"], "Pull requests \u00b7 34")

    def test_ctrl_r_leaves_the_rest_of_the_queue_to_the_background(self):
        repos.write_queue([f"owner/r{number}" for number in range(5)])
        with patch.object(repos, "LIVE_PREFETCH", 2), patch.object(
            repos.ghcommon, "REFRESH", True
        ):
            self.run_main("--pages", "pulls", "owner/r0")
        # One batch is fetched, the rest stays queued for a detached run.
        self.assertTrue(repos.read_cache(repos.live_file("owner/r0")))
        self.assertTrue(repos.read_cache(repos.live_file("owner/r1")))
        self.assertEqual(repos.read_cache(repos.live_file("owner/r2")), {})
        self.assertEqual(self.queued(), ["owner/r2", "owner/r3", "owner/r4"])

    def test_prefetch_drains_the_queue_in_batches(self):
        names = [f"owner/r{number}" for number in range(5)]
        repos.write_queue(names)
        sizes = []
        real = repos.fetch_batch

        def recording(batch):
            sizes.append(len(batch))
            real(batch)

        with patch.object(repos, "fetch_batch", recording), patch.object(
            repos, "LIVE_PREFETCH", 2
        ), patch("builtins.print") as printed:
            self.assertEqual(repos.run_prefetch(), 0)
        printed.assert_not_called()
        # Two rows per batch, four fetched side by side, then the last one.
        self.assertEqual(sizes, [2, 2, 1])
        self.assertEqual(self.queued(), [])
        for name in names:
            entry = repos.read_cache(repos.live_file(name))
            self.assertEqual(entry["first"], repos.FETCH_ITEMS, name)

    def test_prefetch_stops_at_the_budget(self):
        names = [f"owner/r{number}" for number in range(4)]
        repos.write_queue(names)
        with patch.object(repos, "LIVE_PREFETCH", 1), patch.object(
            repos, "LIVE_BUDGET", "0s"
        ):
            self.assertEqual(repos.run_prefetch(), 0)
        self.assertEqual(self.queued(), ["owner/r1", "owner/r2", "owner/r3"])

    def test_a_second_prefetch_run_carries_on_with_the_next_names(self):
        repos.write_queue([f"owner/r{number}" for number in range(3)])
        # A spent budget stops each run after its batch, as a slow one would.
        with patch.object(repos, "LIVE_PREFETCH", 1), patch.object(
            repos, "LIVE_BUDGET", "0s"
        ):
            repos.run_prefetch()
            self.assertEqual(self.queued(), ["owner/r1", "owner/r2"])
            repos.run_prefetch()
        self.assertEqual(
            repos.read_cache(repos.live_file("owner/r1"))["first"], repos.FETCH_ITEMS
        )
        self.assertEqual(self.queued(), ["owner/r2"])

    def test_a_budget_crossed_during_a_batch_stops_the_run(self):
        repos.write_queue([f"owner/r{number}" for number in range(3)])
        # Ten minutes pass inside the first batch, so the budget is spent
        # before the second one and the rest waits for the next run.
        clock = iter([0.0, 600.0])
        with patch.object(repos, "LIVE_PREFETCH", 1), patch.object(
            repos, "LIVE_BUDGET", "1s"
        ), patch.object(repos.time, "monotonic", lambda: next(clock, 600.0)):
            self.assertEqual(repos.run_prefetch(), 0)
        self.assertEqual(self.queued(), ["owner/r1", "owner/r2"])

    def test_prefetch_skips_repositories_whose_cache_is_fresh(self):
        self.write_live()
        fresh = repos.read_cache(repos.live_file("owner/raffi"))
        repos.write_queue(["owner/raffi", "tektoncd/pipeline"])
        fetched = []

        def recording(query, cache=None, timeout=30, **variables):
            if "pullRequests" in query:
                name = f"{variables.get('owner')}/{variables.get('name')}"
                fetched.append(name)
            return graphql(query, cache=cache, timeout=timeout, **variables)

        with patch.object(repos, "graphql", recording):
            self.assertEqual(repos.run_prefetch(), 0)
        # Only the stale name reaches GitHub; the fresh one leaves the queue
        # without a request, its cache untouched.
        self.assertEqual(fetched, ["tektoncd/pipeline"])
        self.assertEqual(self.queued(), [])
        self.assertEqual(repos.read_cache(repos.live_file("owner/raffi")), fresh)
        tekton = repos.read_cache(repos.live_file("tektoncd/pipeline"))
        self.assertEqual(tekton["pull_requests"]["totalCount"], 1)

    def test_a_run_killed_mid_batch_leaves_its_names_queued(self):
        names = [f"owner/r{number}" for number in range(3)]
        repos.write_queue(names)

        def dying(query, cache=None, timeout=30, **variables):
            if variables.get("name") == "r1":
                raise RuntimeError("killed")
            return graphql(query, cache=cache, timeout=timeout, **variables)

        with patch.object(repos, "graphql", dying), patch.object(
            repos, "LIVE_PREFETCH", 2
        ), self.assertRaises(RuntimeError):
            repos.run_prefetch()
        # The batch was never acknowledged, so nothing was lost.
        self.assertEqual(self.queued(), names)
        # The next run takes the unfinished names and finishes the queue.
        with patch.object(repos, "LIVE_PREFETCH", 2):
            self.assertEqual(repos.run_prefetch(), 0)
        self.assertEqual(self.queued(), [])
        for name in names:
            self.assertEqual(
                repos.read_cache(repos.live_file(name))["first"],
                repos.FETCH_ITEMS,
                name,
            )

    def test_prefetch_does_not_read_stdin(self):
        repos.write_queue(["owner/raffi"])
        with patch("sys.stdin", io.StringIO("not json")):
            self.assertEqual(repos.run_prefetch(), 0)
        entry = repos.read_cache(repos.live_file("owner/raffi"))
        self.assertEqual(entry["pull_requests"]["totalCount"], 34)
        self.assertEqual(entry["issues"]["totalCount"], 2)

    def test_prefetch_ignores_a_broken_queue(self):
        repos.queue_file().parent.mkdir(parents=True, exist_ok=True)
        repos.queue_file().write_text("not json")
        with patch("builtins.print") as printed:
            self.assertEqual(repos.run_prefetch(), 0)
        printed.assert_not_called()
        self.assertEqual(repos.PENDING, [])

    def test_prefetch_stops_when_another_run_holds_the_lock(self):
        repos.write_queue(["owner/raffi"])
        repos.CACHE_DIR.mkdir(parents=True, exist_ok=True)
        with open(repos.lock_file(), "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertEqual(repos.run_prefetch(), 0)
            self.assertEqual(self.queued(), ["owner/raffi"])
        self.assertEqual(repos.read_cache(repos.live_file("owner/raffi")), {})


    def test_long_titles_are_not_shortened(self):
        long = "a " * 40 + "needle"
        self.write_live(
            pulls={"totalCount": 1, "nodes": [live_pull("owner/raffi", 1, long)]}
        )
        row = self.pages_item()["children"][1]["children"][0]
        # Raffi filters children on the title, so a word past character 70
        # must still be there.
        self.assertEqual(row["title"], f"#1 {long}")
        self.assertEqual(row["preview"]["text"], long)

    def test_an_issue_title_is_kept_whole_too(self):
        long = "b " * 40 + "needle"
        self.write_live(
            issues={"totalCount": 1, "nodes": [live_issue("owner/raffi", 2, long)]}
        )
        row = self.pages_item()["children"][2]["children"][0]
        self.assertEqual(row["title"], f"#2 {long}")


class LiveErrorTests(ReposTestCase):
    """A live fetch that keeps failing says why instead of loading forever."""

    def failing(self, query, cache=None, timeout=30, **variables):
        raise repos.GhError("API rate limit exceeded")

    def record(self, message):
        def failing(query, cache=None, timeout=30, **variables):
            raise repos.GhError(message)

        with patch.object(repos, "graphql", failing):
            repos.fetch_quietly("owner/raffi")

    def test_a_failing_fetch_records_its_message_and_time(self):
        before = time.time()
        with patch.object(repos, "graphql", self.failing):
            repos.fetch_quietly("owner/raffi")
        entry = repos.read_errors()["owner/raffi"]
        self.assertEqual(entry["error"], "API rate limit exceeded")
        # The timestamp is what LIVE_ERRORS expires the record on.
        self.assertGreaterEqual(entry["at"], before)
        self.assertLessEqual(entry["at"], time.time())

    def test_a_later_success_clears_the_record(self):
        with patch.object(repos, "graphql", self.failing):
            repos.fetch_quietly("owner/raffi")
        self.assertIn("owner/raffi", repos.read_errors())
        repos.fetch_quietly("owner/raffi")
        self.assertEqual(repos.read_errors(), {})

    def test_a_failure_is_kept_to_one_short_line(self):
        self.record("boom " * 40 + "\nsecond line")
        error = repos.read_errors()["owner/raffi"]["error"]
        self.assertNotIn("second line", error)
        self.assertLessEqual(len(error), 120)

    def test_the_cold_placeholder_carries_the_failure(self):
        self.record("gh: Not Found (HTTP 404)")
        pulls = self.pages_item()["children"][1]
        placeholder = pulls["children"][0]
        self.assertEqual(
            placeholder["subtitle"],
            "failed: gh: Not Found (HTTP 404) \u00b7 Ctrl+R retries it",
        )
        self.assertFalse(placeholder["valid"])
        self.assertIn("gh: Not Found (HTTP 404)", placeholder["preview"]["text"])
        self.assertIn("Ctrl+R", placeholder["preview"]["text"])
        # The menu is still no dead end.
        self.assertEqual(
            pulls["children"][-1]["title"], "All pull requests on GitHub"
        )
        self.assertIn("gh: Not Found (HTTP 404)", pulls["preview"]["text"])

    def test_another_repository_keeps_the_loading_wording(self):
        self.record("gh: Not Found (HTTP 404)")
        pages = self.options(pages=(("pulls", "Pull requests"),))
        notes = repos.build_item(repo("notes"), pages)["children"][1]
        self.assertEqual(
            notes["children"][0]["subtitle"],
            "loading in the background \u00b7 Ctrl+R loads it now",
        )
        self.assertIn("loading in the background", notes["preview"]["text"])

    def test_a_menu_with_cached_rows_is_unaffected(self):
        self.record("gh: Not Found (HTTP 404)")
        self.write_live()
        pulls = self.pages_item()["children"][1]
        self.assertEqual(pulls["title"], "Pull requests \u00b7 34")
        self.assertEqual(pulls["children"][0]["title"], "#412 Add clipboard view")
        for row in (pulls, *pulls["children"]):
            self.assertNotIn("Not Found", json.dumps(row))

    def test_an_expired_failure_is_ignored(self):
        repos.write_cache(
            repos.errors_file(), {"owner/raffi": {"at": 0.0, "error": "gone"}}
        )
        self.assertEqual(repos.read_errors(), {})
        placeholder = self.pages_item()["children"][1]["children"][0]
        self.assertEqual(
            placeholder["subtitle"],
            "loading in the background \u00b7 Ctrl+R loads it now",
        )

    def test_a_corrupt_errors_file_is_ignored_without_raising(self):
        repos.errors_file().parent.mkdir(parents=True, exist_ok=True)
        junk = (
            "not json",
            "[]",
            json.dumps({"owner/raffi": "boom"}),
            json.dumps({"owner/raffi": {"at": "soon", "error": 3}}),
            json.dumps({"owner/raffi": {"at": True, "error": "boom"}}),
        )
        for payload in junk:
            repos.errors_file().write_text(payload)
            self.assertEqual(repos.read_errors(), {}, payload)
            # A working list stays a working list.
            placeholder = self.pages_item()["children"][1]["children"][0]
            self.assertEqual(
                placeholder["subtitle"],
                "loading in the background \u00b7 Ctrl+R loads it now",
            )

    def test_a_failure_is_recorded_over_a_corrupt_file_too(self):
        repos.errors_file().parent.mkdir(parents=True, exist_ok=True)
        repos.errors_file().write_text("not json")
        self.record("gh: Not Found (HTTP 404)")
        self.assertEqual(
            repos.read_errors()["owner/raffi"]["error"], "gh: Not Found (HTTP 404)"
        )

    def test_the_file_stays_bounded_when_many_names_fail(self):
        with patch.object(repos, "LIVE_ERRORS_CAP", 5), patch.object(
            repos, "graphql", self.failing
        ):
            for number in range(8):
                repos.fetch_quietly(f"owner/r{number}")
        entries = repos.read_cache(repos.errors_file())
        self.assertEqual(len(entries), 5)
        # The oldest failures made room for the newest ones.
        self.assertIn("owner/r7", entries)
        self.assertNotIn("owner/r0", entries)

    def test_many_long_failures_stay_readable(self):
        # The byte bound is not what trims a map that fits: every record of a
        # file inside it reads back, none lost to the read trimming the write.
        message = "\U0001f600" * 120
        with patch.object(repos, "ERRORS_BYTES", 512 * 1024):
            for number in range(50):
                repos.remember_error(f"owner/r{number}", repos.GhError(message))
            self.assertEqual(len(repos.read_errors()), 50)

    def test_the_write_bound_keeps_the_file_parseable(self):
        # A file past ERRORS_BYTES reads as junk and loses every record at
        # once, so the writer has to stay well inside it however long a
        # failure message is.
        message = "\U0001f600" * 120
        for number in range(80):
            repos.remember_error(f"owner/r{number}", repos.GhError(message))
        payload = repos.errors_file().read_bytes()
        self.assertLess(len(payload), repos.ERRORS_BYTES)
        entries = json.loads(payload)
        self.assertIsInstance(entries, dict)
        # Trimmed by bytes, not reset to the one record still on disk.
        self.assertGreater(len(entries), 1)
        self.assertIn("owner/r79", entries)
        self.assertNotIn("owner/r0", entries)
        self.assertEqual(len(repos.read_errors()), len(entries))

    def test_a_key_that_is_not_a_repository_is_ignored(self):
        now = time.time()
        repos.write_cache(
            repos.errors_file(),
            {
                "not a repo": {"at": now, "error": "boom"},
                "": {"at": now, "error": "boom"},
                "owner/real": {"at": now, "error": "boom"},
            },
        )
        self.assertEqual(list(repos.read_errors()), ["owner/real"])
        repos.remember_error("not a repo", repos.GhError("boom"))
        self.assertEqual(list(repos.read_errors()), ["owner/real"])

    def test_a_record_too_big_to_fit_is_never_written(self):
        # A foreign key big enough to fill the file on its own must not be
        # republished by our writer: the file has to stay parseable.
        now = time.time()
        repos.write_cache(
            repos.errors_file(),
            {"owner/" + "\U0001f600" * 60000: {"at": now + 5, "error": "boom"}},
        )
        repos.remember_error("owner/new", repos.GhError("boom"))
        payload = repos.errors_file().read_bytes()
        self.assertLess(len(payload), repos.ERRORS_BYTES)
        self.assertEqual(list(json.loads(payload)), ["owner/new"])

    def test_an_unsorted_file_evicts_the_oldest_not_the_first(self):
        # forget_error rewrites whatever order the file had, so the byte bound
        # has to rank by time rather than trust the file's order.
        now = time.time()
        entries = {
            f"owner/new{number}": {"at": now - number, "error": "z" * 120}
            for number in range(5)
        }
        entries.update(
            {
                f"owner/old{number}": {"at": now - 4000 - number, "error": "z" * 120}
                for number in range(200)
            }
        )
        repos.write_cache(repos.errors_file(), entries)
        repos.forget_error("owner/new0")
        after = repos.read_errors()
        self.assertTrue(all(f"owner/new{number}" in after for number in range(1, 5)))
        self.assertLess(len(after), len(entries))
        self.assertNotIn("owner/old199", after)

    def test_a_record_past_the_write_bound_keeps_the_newest_ones(self):
        # Crossing the bound must cost the furthest records, never all but the
        # one being written.
        message = "\U0001f600" * 120
        for number in range(40):
            repos.remember_error(f"owner/r{number}", repos.GhError(message))
        kept = len(repos.read_errors())
        repos.remember_error("owner/last", repos.GhError(message))
        found = repos.read_errors()
        self.assertGreater(len(found), 1)
        self.assertGreaterEqual(len(found), kept - 1)
        self.assertIn("owner/last", found)
        self.assertNotIn("owner/r0", found)

    def test_an_oversized_file_of_ours_is_read_again(self):
        # A payload that stops inside the file parses as nothing, so a record
        # past the read bound would read as no failures at all; the read takes
        # the whole file again instead, keeping what it holds.
        record = json.dumps({"owner/raffi": {"at": time.time(), "error": "boom"}})
        repos.errors_file().parent.mkdir(parents=True, exist_ok=True)
        repos.errors_file().write_text(" " * (2 * repos.ERRORS_BYTES) + record)
        self.assertEqual(repos.read_errors()["owner/raffi"]["error"], "boom")

    def test_a_hostile_file_past_the_retry_read_is_best_effort(self):
        record = json.dumps({"owner/raffi": {"at": time.time(), "error": "boom"}})
        repos.errors_file().parent.mkdir(parents=True, exist_ok=True)
        # Past four times ERRORS_BYTES the tail is junk, so nothing is parsed
        # and nothing raises; a menu still draws.
        repos.errors_file().write_text(record + "x" * (5 * repos.ERRORS_BYTES))
        self.assertEqual(repos.read_errors(), {})
        self.assert_serializable(self.pages_item())

    def test_two_concurrent_failures_both_survive(self):
        # The lock keeps one read-modify-write from overwriting the other.
        errors = {}
        with patch.object(repos, "EDIT_PAUSE", 0), patch.object(
            repos, "EDIT_ATTEMPTS", 100
        ):
            for round in range(10):
                pair = {
                    f"owner/a{round}": "first",
                    f"owner/b{round}": "second",
                }
                errors.update(pair)
                start = threading.Barrier(len(pair))

                def record(name, message, start=start):
                    start.wait()
                    repos.remember_error(name, repos.GhError(message))

                with ThreadPoolExecutor(max_workers=len(pair)) as pool:
                    futures = [
                        pool.submit(record, *entry) for entry in pair.items()
                    ]
                    for future in futures:
                        future.result()
        found = repos.read_errors()
        self.assertEqual(
            {name: entry["error"] for name, entry in found.items()}, errors
        )

    def test_a_success_does_not_resurrect_a_cleared_failure(self):
        # A forget racing a remember of another name keeps only the new one.
        # A forget that finds the lock busy skips by design, so clear once
        # more before asking whether the record came back.
        with patch.object(repos, "EDIT_PAUSE", 0), patch.object(
            repos, "EDIT_ATTEMPTS", 100
        ):
            for round in range(10):
                gone, other = f"owner/gone{round}", f"owner/other{round}"
                repos.remember_error(gone, repos.GhError("old"))
                start = threading.Barrier(2)

                def clear(gone=gone, start=start):
                    start.wait()
                    repos.forget_error(gone)

                def add(other=other, start=start):
                    start.wait()
                    repos.remember_error(other, repos.GhError("new"))

                with ThreadPoolExecutor(max_workers=2) as pool:
                    futures = [pool.submit(clear), pool.submit(add)]
                    for future in futures:
                        future.result()
                repos.forget_error(gone)
                found = repos.read_errors()
                self.assertNotIn(gone, found, round)
                self.assertEqual(found[other]["error"], "new", round)

    def test_a_busy_error_lock_skips_the_record(self):
        repos.write_cache(
            repos.errors_file(), {"owner/old": {"at": time.time(), "error": "kept"}}
        )
        repos.CACHE_DIR.mkdir(parents=True, exist_ok=True)
        with open(repos.errors_lock_file(), "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with patch.object(repos, "EDIT_PAUSE", 0), patch.object(
                repos, "EDIT_ATTEMPTS", 1
            ):
                # A busy lock costs the hint, never the file or the caller.
                repos.remember_error("owner/raffi", repos.GhError("boom"))
        found = repos.read_errors()
        self.assertNotIn("owner/raffi", found)
        self.assertEqual(found["owner/old"]["error"], "kept")
        # Free again, the record lands.
        repos.remember_error("owner/raffi", repos.GhError("boom"))
        self.assertIn("owner/raffi", repos.read_errors())

    def test_a_deeply_nested_file_does_not_raise(self):
        repos.errors_file().parent.mkdir(parents=True, exist_ok=True)
        # Deep enough that json.loads raises RecursionError, which the read
        # has to swallow like any other junk.
        repos.errors_file().write_text("[" * 60000 + "]" * 60000)
        with patch.object(repos, "ERRORS_BYTES", 256 * 1024):
            self.assertEqual(repos.read_errors(), {})
            item = self.pages_item()
            self.assertEqual(
                item["children"][1]["children"][0]["subtitle"],
                "loading in the background \u00b7 Ctrl+R loads it now",
            )
            self.assert_serializable(item)

    def test_an_impossible_timestamp_is_not_recent(self):
        repos.errors_file().parent.mkdir(parents=True, exist_ok=True)
        for at in ("1e999", "1e300", "-1e300", str(10**400)):
            repos.errors_file().write_text(
                f'{{"owner/raffi": {{"at": {at}, "error": "boom"}}}}'
            )
            self.assertEqual(repos.read_errors(), {}, at)
        item = self.pages_item()
        self.assertEqual(
            item["children"][1]["children"][0]["subtitle"],
            "loading in the background \u00b7 Ctrl+R loads it now",
        )
        self.assert_serializable(item)

    def test_a_future_timestamp_is_not_a_recent_failure(self):
        # An age bound alone is symmetric by accident: a date far ahead gives
        # a huge negative age and would pass for a failure of a moment ago.
        repos.errors_file().parent.mkdir(parents=True, exist_ok=True)
        now = time.time()
        for ahead in (86400, 100 * 365 * 86400, 3000 * 365 * 86400):
            repos.errors_file().write_text(
                json.dumps({"owner/raffi": {"at": now + ahead, "error": "boom"}})
            )
            self.assertEqual(repos.read_errors(), {}, ahead)
            self.assertIsNone(repos.error_time(now + ahead, now), ahead)
        # A clock a few seconds ahead is still a failure of a moment ago.
        repos.errors_file().write_text(
            json.dumps({"owner/raffi": {"at": now + 5, "error": "boom"}})
        )
        self.assertEqual(repos.read_errors()["owner/raffi"]["error"], "boom")
        self.assertEqual(repos.error_time(now + 5, now), now + 5)

    def test_an_oversized_file_is_read_bounded(self):
        repos.errors_file().parent.mkdir(parents=True, exist_ok=True)
        record = json.dumps({"owner/raffi": {"at": time.time(), "error": "boom"}})
        # The record at the head of a big file still reads.
        repos.errors_file().write_text(record + " " * (2 * repos.ERRORS_BYTES))
        self.assertEqual(repos.read_errors()["owner/raffi"]["error"], "boom")
        # Past the cap nothing is parsed, so a huge tail cannot be read in.
        with patch.object(repos, "ERRORS_BYTES", 8):
            self.assertEqual(repos.read_errors(), {})
        self.assert_serializable(self.pages_item())

    def test_a_lone_surrogate_leaves_the_output_encodable(self):
        repos.write_cache(
            repos.errors_file(),
            {"owner/raffi": {"at": time.time(), "error": "boom \ud800 tail"}},
        )
        found = repos.read_errors()["owner/raffi"]["error"]
        self.assertNotIn("\ud800", found)
        placeholder = self.pages_item()["children"][1]["children"][0]
        self.assert_serializable(placeholder)
        self.assertIn("boom", placeholder["subtitle"])

    def test_controls_do_not_swallow_the_retry_hint(self):
        message = (
            "x" * 100
            + "\x1b[31m"
            + "y" * 10
            + "\x1b]8;;https://example.invalid/a\x07"
            + "boom"
        )
        repos.write_cache(
            repos.errors_file(),
            {"owner/raffi": {"at": time.time(), "error": message}},
        )
        placeholder = self.pages_item()["children"][1]["children"][0]
        # Stripped before the cap, so the hint after the message survives.
        self.assertEqual(
            placeholder["subtitle"],
            "failed: " + "x" * 100 + "y" * 10 + "boom \u00b7 Ctrl+R retries it",
        )
        self.assertNotIn("\x1b", placeholder["subtitle"])
        self.assertNotIn("\x07", placeholder["subtitle"])
        self.assert_serializable(placeholder)

    def test_a_cache_that_cannot_be_written_is_a_failure(self):
        self.record("gh: Not Found (HTTP 404)")
        # The temporary file cannot be renamed over a directory, so the live
        # cache never lands: that is a failure, not a success clearing one.
        repos.live_file("owner/raffi").mkdir()
        repos.fetch_quietly("owner/raffi")
        found = repos.read_errors()
        self.assertIn("owner/raffi", found)
        self.assertNotEqual(found["owner/raffi"]["error"], "")

    def test_a_cold_page_reads_the_error_file_once(self):
        self.record("gh: Not Found (HTTP 404)")
        repos.reload_errors()
        reads = []
        real = repos.read_errors

        def counting():
            reads.append(True)
            return real()

        with patch.object(repos, "read_errors", counting):
            for number in range(4):
                item = repos.build_item(
                    repo(f"r{number}"),
                    self.options(pages=(("pulls", "Pull requests"),)),
                )
                self.assertFalse(item["children"][1]["children"][0]["valid"])
        # Preview and placeholder share one snapshot, not one read each.
        self.assertEqual(len(reads), 1)

    def test_a_ctrl_r_rebuild_shows_the_failure_it_records(self):
        def failing_live(query, cache=None, timeout=30, **variables):
            if "pullRequests" in query:
                raise repos.GhError("API rate limit exceeded")
            return graphql(query, cache=cache, timeout=timeout, **variables)

        with patch.object(repos, "graphql", failing_live), patch.object(
            repos.ghcommon, "REFRESH", True
        ):
            output = self.run_main("--pages", "pulls", "me", "raffi")
        placeholder = output["items"][0]["children"][1]["children"][0]
        self.assertEqual(
            placeholder["subtitle"],
            "failed: API rate limit exceeded \u00b7 Ctrl+R retries it",
        )

    def test_a_new_run_reads_the_errors_again(self):
        # A draw caches a snapshot; the next run must not keep serving it.
        self.pages_item()
        repos.write_cache(
            repos.errors_file(),
            {"owner/raffi": {"at": time.time(), "error": "gone wrong"}},
        )
        output = self.run_main("--pages", "pulls", "me", "raffi")
        placeholder = output["items"][0]["children"][1]["children"][0]
        self.assertIn("failed: gone wrong", placeholder["subtitle"])

    def assert_serializable(self, item):
        """Rafi prints with ensure_ascii=False, which a lone surrogate breaks."""
        self.assertTrue(json.dumps(item, ensure_ascii=False).encode("utf-8"))

    def run_main(self, *argv, env=None):
        with environment(**(env or {})), patch(
            "sys.argv", ["repos.py", *argv]
        ), patch("builtins.print") as printed:
            self.assertEqual(repos.main(), 0)
        return json.loads(printed.call_args[0][0])

    def write_live(self, at=None):
        repos.write_cache(
            repos.live_file("owner/raffi"),
            {
                "at": time.time() if at is None else at,
                "first": repos.FETCH_ITEMS,
                "pull_requests": LIVE["owner/raffi"]["pullRequests"],
                "issues": LIVE["owner/raffi"]["issues"],
            },
        )

    def pages_item(self, **kwargs):
        kwargs.setdefault("pages", (("pulls", "Pull requests"),))
        return repos.build_item(repo("raffi"), self.options(**kwargs))


class QueueTests(ReposTestCase):
    """The persistent queue keeps a long list filling over several runs."""

    def run_main(self, *argv, env=None):
        with environment(**(env or {})), patch(
            "sys.argv", ["repos.py", *argv]
        ), patch("builtins.print") as printed:
            self.assertEqual(repos.main(), 0)
        return json.loads(printed.call_args[0][0])

    def test_a_draw_queues_its_names_once(self):
        drawn = ["owner/raffi", "owner/pipelines", "owner/notes"]
        self.run_main("--pages", "pulls", "me")
        self.assertEqual(self.queued(), drawn)
        self.run_main("--pages", "pulls", "me")
        self.assertEqual(self.queued(), drawn)

    def test_queueing_appends_behind_what_is_already_waiting(self):
        repos.write_queue(["other/repo"])
        self.run_main("--pages", "pulls", "me", "raffi")
        self.assertEqual(self.queued(), ["other/repo", "owner/raffi"])

    def test_a_busy_queue_edit_leaves_the_names_to_the_next_draw(self):
        repos.write_queue(["other/repo"])
        repos.CACHE_DIR.mkdir(parents=True, exist_ok=True)
        with open(repos.edit_lock_file(), "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with patch.object(repos, "EDIT_PAUSE", 0):
                repos.queue_pending(["owner/raffi"])
            self.assertEqual(self.queued(), ["other/repo"])
        # Free again, the next draw queues the name it could not before.
        self.run_main("--pages", "pulls", "me", "raffi")
        self.assertEqual(self.queued(), ["other/repo", "owner/raffi"])

    def test_a_drain_never_holds_a_draw_out_of_the_queue(self):
        repos.write_queue(["other/repo"])
        repos.CACHE_DIR.mkdir(parents=True, exist_ok=True)
        with open(repos.lock_file(), "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            repos.queue_pending(["owner/raffi"])
        # The drain lock is about one run draining; a draw writes beside it.
        self.assertEqual(self.queued(), ["other/repo", "owner/raffi"])

    def test_every_queue_edit_locks_its_whole_read_and_write(self):
        # Every read-modify-write of the queue holds the edit lock from the
        # read to the write, so nobody can read [A, B] and then overwrite
        # [A, B, C] with its own remainder.
        repos.write_queue(["owner/a"])
        seen = []
        real = repos.read_queue

        def probing_read():
            queue = real()
            with repos.edit_lock() as locked:
                seen.append(locked)
            return queue

        edits = (
            lambda: repos.queue_pending(["owner/b"], refresh=True),
            lambda: repos.ack_batch(repos.Batch(["owner/a"], 0)),
        )
        with patch.object(repos, "read_queue", probing_read), patch.object(
            repos, "EDIT_ATTEMPTS", 1
        ):
            for edit in edits:
                edit()
        self.assertEqual(seen, [False, False])
        self.assertEqual(self.queued(), ["owner/b"])

    def test_no_live_never_queues(self):
        for argv, env in ((["--no-live"], {}), ([], {"LIVE": "0"})):
            self.run_main("--pages", "pulls,issues", *argv, env=env)
            self.assertEqual(self.queued(), [])
            self.assertEqual(self.prefetched, [])

    def test_the_queue_file_round_trips_and_survives_junk(self):
        repos.write_queue(["owner/raffi", "tektoncd/pipeline"], ["tektoncd/pipeline"])
        self.assertEqual(repos.queue_file().parent, repos.live_file("x").parent)
        self.assertEqual(
            repos.read_queue(),
            repos.Queue(
                [
                    repos.Entry("owner/raffi", 0, False),
                    repos.Entry("tektoncd/pipeline", 0, True),
                ],
                0,
            ),
        )
        # On disk, each entry carries the generation it was added at.
        payload = json.loads(repos.queue_file().read_text())
        self.assertEqual(payload["gen"], 0)
        self.assertEqual(
            payload["entries"],
            [
                {"name": "owner/raffi", "gen": 0, "force": False},
                {"name": "tektoncd/pipeline", "gen": 0, "force": True},
            ],
        )
        # The newest add outranks the counter it was written against.
        payload["gen"] = 2
        payload["entries"].append({"name": "owner/notes", "gen": 5, "force": False})
        repos.queue_file().write_text(json.dumps(payload))
        self.assertEqual(repos.read_queue().gen, 5)
        # An older queue file is a bare list of names, none of them forced.
        repos.queue_file().write_text(json.dumps(["owner/raffi", 3]))
        self.assertEqual(
            repos.read_queue(), repos.Queue([repos.Entry("owner/raffi", 0, False)])
        )
        # The shape after that kept the forced names in their own list.
        repos.queue_file().write_text(
            json.dumps({"names": ["owner/raffi", 3], "force": ["owner/raffi", 4]})
        )
        self.assertEqual(
            repos.read_queue(), repos.Queue([repos.Entry("owner/raffi", 0, True)])
        )
        # A name an older file marked forced is still fetched over a fresh
        # cache, and acknowledging that batch still clears it.
        repos.write_cache(
            repos.live_file("owner/raffi"),
            {"at": time.time(), "first": repos.FETCH_ITEMS},
        )
        batch = repos.next_batch(1)
        self.assertEqual(batch, (["owner/raffi"], 0))
        self.assertTrue(repos.ack_batch(batch))
        self.assertEqual(repos.read_queue().entries, [])
        repos.queue_file().write_text(json.dumps({"owner/raffi": 1}))
        self.assertEqual(repos.read_queue().entries, [])
        # Entries without a name or a generation are junk, and dropped.
        repos.queue_file().write_text(
            json.dumps({"entries": [{"name": "owner/raffi"}, {"gen": 1}, 4]})
        )
        self.assertEqual(repos.read_queue().entries, [])

    def test_an_ack_from_an_older_batch_keeps_a_later_force_marker(self):
        # A background batch takes owner/raffi, then Ctrl+R asks for it again:
        # the marker was written after the batch was selected, so the batch's
        # acknowledgement has to leave it, or the refresh is lost.
        repos.write_queue(["owner/raffi"])
        batch = repos.next_batch(1)
        self.assertEqual(batch.names, ["owner/raffi"])
        repos.queue_pending(["owner/raffi"], refresh=True)
        self.assertTrue(repos.ack_batch(batch))
        self.assertEqual(self.queued(), ["owner/raffi"])
        self.assertEqual(repos.read_queue().force, ["owner/raffi"])
        # Still queued and still forced, so the next drain fetches it.
        fetched = []

        def recording(query, cache=None, timeout=30, **variables):
            if "pullRequests" in query:
                fetched.append(f"{variables.get('owner')}/{variables.get('name')}")
            return graphql(query, cache=cache, timeout=timeout, **variables)

        with patch.object(repos, "graphql", recording):
            repos.load_pending()
        self.assertEqual(fetched, ["owner/raffi"])
        self.assertEqual(self.queued(), [])

    def test_a_name_readded_while_a_batch_runs_survives_its_ack(self):
        repos.write_queue(["owner/raffi", "tektoncd/pipeline"])
        batch = repos.next_batch(1)
        # A draw re-queues the name the batch is about to acknowledge; that
        # newer work is not the batch's to drop.
        repos.queue_pending(["owner/raffi"])
        self.assertTrue(repos.ack_batch(batch))
        self.assertEqual(self.queued(), ["tektoncd/pipeline", "owner/raffi"])

    def test_ctrl_r_with_a_busy_queue_edit_still_shows_fresh_rows(self):
        # A run holds queue.lock past the draw's few tries: the forced enqueue
        # cannot happen, so the draw fetches the row itself instead of losing
        # the refresh the user asked for.
        landed = live_pull("owner/raffi", 500, "Landed after Ctrl+R")
        calls = []

        def changing(query, cache=None, timeout=30, **variables):
            if "pullRequests" not in query:
                return graphql(query, cache=cache, timeout=timeout, **variables)
            calls.append(f"{variables.get('owner')}/{variables.get('name')}")
            return {
                "repository": {
                    "pullRequests": {"totalCount": 1, "nodes": [landed]},
                    "issues": {"totalCount": 0, "nodes": []},
                }
            }

        repos.CACHE_DIR.mkdir(parents=True, exist_ok=True)
        with open(repos.edit_lock_file(), "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with patch.object(repos, "graphql", changing), patch.object(
                repos, "EDIT_PAUSE", 0
            ), patch.object(repos.ghcommon, "REFRESH", True):
                output = self.run_main("--pages", "pulls", "me", "raffi")
        self.assertEqual(calls, ["owner/raffi"])
        pulls = output["items"][0]["children"][1]
        self.assertEqual(pulls["children"][0]["title"], "#500 Landed after Ctrl+R")
        self.assertEqual(self.queued(), [])

    def test_ctrl_r_forces_only_the_batch_it_drains(self):
        drawn = ["owner/raffi", "owner/pipelines", "owner/notes"]
        fetched = []
        queued_before = []

        def recording(query, cache=None, timeout=30, **variables):
            if "pullRequests" in query:
                fetched.append(f"{variables.get('owner')}/{variables.get('name')}")
            return graphql(query, cache=cache, timeout=timeout, **variables)

        real_load = repos.load_pending

        def capture():
            queued_before.append(repos.read_queue())
            real_load()

        with patch.object(repos, "LIVE_PREFETCH", 2), patch.object(
            repos, "graphql", recording
        ), patch.object(repos, "load_pending", capture), patch.object(
            repos.ghcommon, "REFRESH", True
        ):
            self.run_main("--pages", "pulls", "me")
        # Every drawn row is queued, in the order drawn, but only the batch
        # the synchronous refresh drains is forced.
        self.assertEqual(len(queued_before), 1)
        self.assertEqual(queued_before[0].names, drawn)
        self.assertEqual(queued_before[0].force, drawn[:2])
        self.assertEqual(fetched, drawn[:2])
        # What the refresh did not drain stays queued for a detached run.
        self.assertEqual(self.queued(), ["owner/notes"])


class PrefetchSpawnTests(ReposTestCase):
    """The detached run, as the parent starts it."""

    def test_start_prefetch_runs_the_script_detached(self):
        calls = []

        def record(*args, **kwargs):
            calls.append((args, kwargs))

        with patch.object(repos.subprocess, "Popen", record), patch.object(
            repos, "FETCH_ITEMS", 7
        ):
            self.real_start_prefetch()
        self.assertEqual(len(calls), 1)
        argv, kwargs = calls[0]
        argv = argv[0]
        self.assertEqual(argv[0], repos.sys.executable)
        self.assertEqual(argv[1], str(Path(repos.__file__).resolve()))
        self.assertIn("--prefetch", argv)
        # The fetch size has to match, or the child writes a smaller cache.
        self.assertEqual(argv[argv.index("--live-items") + 1], "7")
        self.assertEqual(kwargs["stdout"], repos.subprocess.DEVNULL)
        self.assertEqual(kwargs["stderr"], repos.subprocess.DEVNULL)
        self.assertTrue(kwargs["start_new_session"])
        # The names travel through the queue, not through a pipe.
        self.assertNotIn("stdin", kwargs)

    def test_a_failing_spawn_is_swallowed(self):
        with patch.object(repos.subprocess, "Popen", side_effect=OSError("no exec")):
            self.real_start_prefetch()


class LiveQueryTests(ReposTestCase):
    def test_the_live_query_asks_for_the_newest_open_rows_first(self):
        query = repos.LIVE_QUERY
        self.assertIn("$first: Int!", query)
        self.assertEqual(query.count("states: OPEN"), 2)
        self.assertEqual(query.count("field: UPDATED_AT"), 2)
        self.assertEqual(query.count("direction: DESC"), 2)
        self.assertIn("pullRequests(", query)
        self.assertIn("issues(", query)
        self.assertIn("repository(owner: $owner, name: $name)", query)


class TypedTimeTests(ReposTestCase):
    def pushed(self):
        return datetime.now(UTC) - timedelta(hours=3)

    def test_by_default_the_row_carries_baked_text_and_no_time(self):
        pushed = self.pushed().isoformat()
        with patch.object(repos, "TYPED_TIME", False):
            item = repos.build_item(repo("raffi", pushedAt=pushed))
        self.assertIn("pushed 3h ago", repos.ANSI.sub("", item["subtitle"]))
        self.assertNotIn("time", item)

    def test_typed_time_sends_data_and_no_baked_text(self):
        pushed = self.pushed().isoformat()
        with patch.object(repos, "TYPED_TIME", True):
            item = repos.build_item(repo("raffi", pushedAt=pushed))
        self.assertEqual(item["time"], {"at": pushed, "label": "pushed"})
        self.assertNotIn("pushed", repos.ANSI.sub("", item["subtitle"]))

    def test_the_filter_still_matches_the_age_the_row_shows(self):
        pushed = self.pushed().isoformat()
        words = ["pushed"]
        for typed in (False, True):
            with patch.object(repos, "TYPED_TIME", typed):
                found = repos.items_for(
                    account("me", [repo("raffi", pushedAt=pushed)]), words
                )
            self.assertEqual(self.titles(found)[0], "raffi", typed)

    def test_the_preview_does_not_repeat_a_typed_time(self):
        pushed = self.pushed().isoformat()
        for typed, expected in ((False, True), (True, False)):
            with patch.object(repos, "TYPED_TIME", typed):
                preview = repos.build_preview(repo("raffi", pushedAt=pushed))
            rows = [name for name, _ in preview["metadata"]]
            self.assertEqual("Last push" in rows, expected, typed)

    def test_typed_time_without_a_push_date_sends_nothing(self):
        with patch.object(repos, "TYPED_TIME", True):
            item = repos.build_item(repo("raffi"))
        self.assertNotIn("time", item)


class ItemTests(ReposTestCase):
    def test_item_carries_action_variables(self):
        item = repos.build_item(repo("raffi", description="A launcher"))
        self.assertEqual(item["arg"], "https://github.com/owner/raffi")
        self.assertEqual(item["variables"]["REPO_NAME"], "owner/raffi")
        self.assertEqual(
            item["variables"]["REPO_SSH"], "git@github.com:owner/raffi.git"
        )
        self.assertEqual(
            item["variables"]["REPO_CLONE"], "https://github.com/owner/raffi.git"
        )
        self.assertEqual(item["preview"]["dynamic"], False)

    def test_item_shows_language_and_flags(self):
        item = repos.build_item(
            repo(
                "old",
                isArchived=True,
                isPrivate=True,
                primaryLanguage={"name": "Go"},
                stargazerCount=1200,
            )
        )
        subtitle = repos.ANSI.sub("", item["subtitle"])
        self.assertIn("Go", subtitle)
        self.assertIn("archived", subtitle)
        self.assertIn("private", subtitle)
        self.assertIn("1.2k", subtitle)

    def test_count(self):
        self.assertEqual(repos.count(999), "999")
        self.assertEqual(repos.count(1000), "1k")
        self.assertEqual(repos.count(1234), "1.2k")
        self.assertEqual(repos.count(2_000_000), "2M")

    def test_ago(self):
        now = datetime.now(UTC)
        self.assertEqual(repos.ago((now - timedelta(hours=3)).isoformat()), "3h ago")
        self.assertEqual(repos.ago(None), "")


class RepoNamedTests(ReposTestCase):
    def test_a_repository_is_looked_up_once_per_run(self):
        calls = []

        def counting(query, cache=None, timeout=30, **variables):
            calls.append(variables.get("name"))
            return graphql(query, cache, timeout, **variables)

        with environment(), patch.object(repos, "graphql", counting):
            self.assertTrue(repos.repo_named("me", "raffi"))
            self.assertTrue(repos.repo_named("ME", "Raffi"))
        self.assertEqual(calls, ["raffi"])

    def test_a_missing_repository_is_remembered_too(self):
        calls = []

        def counting(query, cache=None, timeout=30, **variables):
            calls.append(variables.get("name"))
            return graphql(query, cache, timeout, **variables)

        with environment(), patch.object(repos, "graphql", counting):
            self.assertFalse(repos.repo_named("me", "ghost"))
            self.assertFalse(repos.repo_named("me", "ghost"))
        # One lookup, remembered as a miss, as the positive answer is.
        self.assertEqual(calls, ["ghost"])
        self.assertEqual(repos.NAMED, {("me", "ghost"): False})

    def test_main_forgets_the_answers_of_the_run_before(self):
        repos.NAMED[("me", "raffi")] = True
        with environment(), patch("sys.argv", ["repos.py", "me"]), patch(
            "builtins.print"
        ):
            repos.main()
        self.assertEqual(repos.NAMED, {})


class MissingAccountTests(ReposTestCase):
    def test_a_missing_account_is_remembered(self):
        with environment():
            self.assertIsNone(repos.as_account("ghost"))
            self.assertTrue(repos.recent_miss("ghost"))
        self.assertTrue(repos.missing_file().exists())

    def test_a_stale_miss_is_forgotten(self):
        repos.missing_file().write_text(json.dumps({"ghost": 0.0}))
        self.assertFalse(repos.recent_miss("ghost"))

    def test_a_miss_is_not_looked_up_twice(self):
        calls = []

        def counting(query, cache=None, timeout=30, **variables):
            calls.append(variables.get("owner"))
            return graphql(query, cache, timeout, **variables)

        with environment(), patch.object(repos, "graphql", counting):
            repos.as_account("ghost")
            repos.as_account("ghost")
        self.assertEqual(calls, ["ghost"])


if __name__ == "__main__":
    unittest.main()
