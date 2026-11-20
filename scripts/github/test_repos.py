import io
import json
import os
import tempfile
import time
import unittest
from contextlib import contextmanager, redirect_stdout
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
        "CACHE_LIVE": repos.CACHE_LIVE,
        "CACHE_REPOS": repos.CACHE_REPOS,
        "CACHE_MISSING": repos.CACHE_MISSING,
        "CACHE_LOGIN": repos.CACHE_LOGIN,
        "REPO_PATH": "",
        "REPO_PAGES": "",
        "REPO_ACTIONS": "",
        "OWNERS": "",
        "ANY_OWNER": "",
        "RAFFI_CHILDREN": "",
        "REPO_NAME": "",
        "LIVE_PATH": "",
        "LIVE_LABEL": "",
    }
    with patch.dict(os.environ, pinned), patch.dict(os.environ, values):
        yield


# The settings the suite runs with, pinned on the module itself in setUp: the
# module reads the environment at import, so an ambient LIVE_ITEMS would
# otherwise change what the assertions compare against.
SETTLED = {
    "LIVE_ITEMS": 20,
    "CACHE_LIVE": "10m",
}


class ReposTestCase(unittest.TestCase):
    def setUp(self):
        repos.ACCOUNTS.clear()
        repos.NAMED.clear()
        for name, value in SETTLED.items():
            self.addCleanup(setattr, repos, name, getattr(repos, name))
            setattr(repos, name, value)
        # Options() reads LIVE_ITEMS when the class is defined, so its own
        # defaults carry the ambient one; both are pinned here.
        self.addCleanup(setattr, repos, "DEFAULTS", repos.DEFAULTS)
        repos.DEFAULTS = repos.Options(live_items=SETTLED["LIVE_ITEMS"])
        self.addCleanup(repos.NAMED.clear)
        self.cache = tempfile.TemporaryDirectory()
        cache_dir = Path(self.cache.name)
        self.patches = [
            patch.object(repos, "graphql", graphql),
            patch.object(repos, "CACHE_DIR", cache_dir),
        ]
        for patcher in self.patches:
            patcher.start()
            self.addCleanup(patcher.stop)
        self.addCleanup(self.cache.cleanup)
        self.addCleanup(repos.ACCOUNTS.clear)

    def titles(self, items):
        return [repos.ANSI.sub("", item["title"]) for item in items]

    def options(self, **kwargs):
        """Options with the suite's live_items, not whatever the env said."""
        kwargs.setdefault("live_items", repos.LIVE_ITEMS)
        return repos.Options(**kwargs)


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
                "OWNERS": "",
            },
        )
        self.assertEqual(output["view"], "me")
        self.assertEqual(output["cache"], {"seconds": 600})

    def test_live_zero_in_the_environment_turns_the_menus_off(self):
        output = self.run_main("--pages", "pulls,issues", env={"LIVE": "0"})
        children = output["items"][0]["children"]
        self.assertEqual(
            [repos.ANSI.sub("", child["title"]) for child in children],
            ["Open owner/raffi", "Pull requests", "Issues"],
        )
        self.assertTrue(
            all("children" not in child and not child.get("children_lazy") for child in children)
        )

    def test_a_list_with_lazy_live_menus_keeps_full_repo_cache(self):
        with patch.object(repos, "CACHE_REPOS", "24h"):
            output = self.run_main("--pages", "pulls", "me", "raffi")
        self.assertEqual(output["cache"], {"seconds": 86400})

    def test_a_typed_repository_keeps_full_repo_cache(self):
        with patch.object(repos, "CACHE_REPOS", "24h"):
            output = self.run_main("--pages", "pulls", "owner/raffi")
        self.assertEqual(output["cache"], {"seconds": 86400})


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
        self.assertTrue(item["children"][2]["children_lazy"])
        self.assertNotIn("children", item["children"][1])


class LiveChildrenTests(ReposTestCase):
    def run_main(self, *argv, **values):
        out = io.StringIO()
        with environment(**values), redirect_stdout(out):
            self.assertEqual(repos.main(list(argv)), 0)
        return json.loads(out.getvalue())

    def test_live_page_is_lazy_and_makes_no_live_graphql_call(self):
        calls = []

        def tracked(query, **variables):
            calls.append(query)
            return graphql(query, **variables)

        with patch.object(repos, "graphql", tracked):
            output = self.run_main("--pages", "pulls,issues,releases")

        self.assertFalse(any("pullRequests" in q for q in calls))
        raffi = next(item for item in output["items"] if item["title"] == "raffi")
        by_title = {child["title"]: child for child in raffi["children"]}
        self.assertTrue(by_title["Pull requests"]["children_lazy"])
        self.assertNotIn("children", by_title["Pull requests"])
        self.assertEqual(by_title["Pull requests"]["variables"]["LIVE_PATH"], "pulls")
        self.assertEqual(
            by_title["Pull requests"]["variables"]["REPO_NAME"], "owner/raffi"
        )
        self.assertTrue(by_title["Issues"]["children_lazy"])
        self.assertEqual(by_title["Issues"]["variables"]["LIVE_PATH"], "issues")
        self.assertNotIn("children_lazy", by_title["Releases"])

    def test_no_live_turns_pulls_and_issues_into_plain_links(self):
        output = self.run_main("--pages", "pulls,issues", "--no-live")
        raffi = next(item for item in output["items"] if item["title"] == "raffi")
        by_title = {child["title"]: child for child in raffi["children"]}
        self.assertNotIn("children_lazy", by_title["Pull requests"])
        self.assertNotIn("children_lazy", by_title["Issues"])

    def test_actions_live_path_still_attaches_lazy_child(self):
        output = self.run_main("--actions", "pulls,releases")
        raffi = next(item for item in output["items"] if item["title"] == "raffi")
        titles = [child["title"] for child in raffi["children"]]
        self.assertEqual(titles, ["Open owner/raffi", "Pull requests"])
        self.assertTrue(raffi["children"][1]["children_lazy"])

    def test_raffi_children_fetches_open_pull_requests(self):
        live = {
            "repository": {
                "pullRequests": {
                    "totalCount": 2,
                    "nodes": [
                        {
                            "number": 42,
                            "title": "Add lazy children",
                            "url": "https://github.com/chmouel/raffi/pull/42",
                            "isDraft": True,
                            "updatedAt": "2026-04-10T10:00:00Z",
                            "author": {"login": "chmouel"},
                            "headRefName": "lazy-children",
                            "reviewDecision": "APPROVED",
                        },
                    ],
                },
                "issues": {"totalCount": 0, "nodes": []},
            }
        }
        with patch.object(repos, "graphql", lambda query, **kw: live):
            output = self.run_main(
                "--pages",
                "pulls,issues",
                RAFFI_CHILDREN="1",
                REPO_NAME="chmouel/raffi",
                LIVE_PATH="pulls",
                LIVE_LABEL="Pull requests",
            )

        self.assertEqual(output["cache"]["seconds"], 600)
        titles = self.titles(output["items"])
        self.assertEqual(
            titles, ["#42 Add lazy children", "All pull requests on GitHub"]
        )
        pr = output["items"][0]
        self.assertEqual(pr["arg"], "https://github.com/chmouel/raffi/pull/42")
        self.assertEqual(pr["variables"]["PR_NUMBER"], "42")
        self.assertEqual(pr["variables"]["PR_BRANCH"], "lazy-children")
        self.assertIn("@chmouel", pr["subtitle"])
        self.assertIn("draft", repos.ANSI.sub("", pr["subtitle"]))
        self.assertIn("approved", repos.ANSI.sub("", pr["subtitle"]))

    def test_raffi_children_fetches_open_issues_and_respects_live_items(self):
        live = {
            "repository": {
                "pullRequests": {"totalCount": 0, "nodes": []},
                "issues": {
                    "totalCount": 2,
                    "nodes": [
                        {
                            "number": 7,
                            "title": "First issue",
                            "url": "https://github.com/chmouel/raffi/issues/7",
                            "updatedAt": "2026-04-10T10:00:00Z",
                            "author": {"login": "alice"},
                            "labels": {"nodes": [{"name": "bug"}]},
                        },
                        {
                            "number": 8,
                            "title": "Second issue",
                            "url": "https://github.com/chmouel/raffi/issues/8",
                            "updatedAt": "2026-04-09T10:00:00Z",
                            "author": {"login": "bob"},
                            "labels": {"nodes": []},
                        },
                    ],
                },
            }
        }
        with patch.object(repos, "graphql", lambda query, **kw: live):
            output = self.run_main(
                "--pages",
                "issues",
                "--live-items",
                "1",
                RAFFI_CHILDREN="1",
                REPO_NAME="chmouel/raffi",
                LIVE_PATH="issues",
                LIVE_LABEL="Issues",
            )

        self.assertEqual(
            self.titles(output["items"]),
            ["#7 First issue", "All issues on GitHub"],
        )
        issue = output["items"][0]
        self.assertEqual(issue["variables"]["ISSUE_NUMBER"], "7")
        self.assertIn("bug", issue["subtitle"])

    def test_raffi_children_error_returns_uncached_error_and_fallback_link(self):
        def boom(query, **kw):
            raise repos.GhError("rate limited")

        with patch.object(repos, "graphql", boom):
            output = self.run_main(
                "--pages",
                "pulls",
                RAFFI_CHILDREN="1",
                REPO_NAME="chmouel/raffi",
                LIVE_PATH="pulls",
                LIVE_LABEL="Pull requests",
            )

        self.assertEqual(output["cache"]["seconds"], 0)
        self.assertEqual(len(output["items"]), 2)
        self.assertFalse(output["items"][0]["valid"])
        self.assertIn("rate limited", output["items"][0]["subtitle"])
        self.assertEqual(output["items"][1]["title"], "All pull requests on GitHub")

    def test_raffi_children_ignores_unknown_path(self):
        output = self.run_main(
            RAFFI_CHILDREN="1", REPO_NAME="chmouel/raffi", LIVE_PATH="releases"
        )
        self.assertEqual(output["items"], [])
        self.assertEqual(output["cache"]["seconds"], 0)


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
