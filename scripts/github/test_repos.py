import fcntl
import io
import json
import os
import tempfile
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
    "CACHE_LIVE": "10m",
}


class ReposTestCase(unittest.TestCase):
    def setUp(self):
        repos.ACCOUNTS.clear()
        repos.PENDING.clear()
        repos.NAMED.clear()
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
