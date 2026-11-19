import json
import os
import tempfile
import unittest
from contextlib import contextmanager
from datetime import UTC, datetime, timedelta
from pathlib import Path
from unittest.mock import patch

import repos


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


def graphql(query, cache=None, timeout=30, **variables):
    if "viewer" in query:
        return {"viewer": {"login": "me"}}
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
    saved = {key: os.environ[key] for key in os.environ if key.startswith("ALIAS_")}
    for key in saved:
        del os.environ[key]
    with patch.dict(
        os.environ, {key: "" for key in ("RAFFI_VIEW", "GITHUB_USER")}
    ), patch.dict(os.environ, values):
        yield


class ReposTestCase(unittest.TestCase):
    def setUp(self):
        repos.ACCOUNTS.clear()
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
    def list(self, query, view="", owners=("me",), **kwargs):
        env = {key: value for key, value in kwargs.items() if key.startswith("ALIAS_")}
        options = repos.Options(
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
            owner, items = repos.list_items("", "ghost", ["me"])
        self.assertEqual(owner, "ghost")
        self.assertEqual(items[0]["title"], "Account not found")
        self.assertFalse(items[0]["valid"])

    def test_views_are_the_configured_accounts(self):
        with environment(ALIAS_t="tektoncd"), patch(
            "sys.argv", ["repos.py", "--owners", "me,t", "raffi"]
        ), patch("builtins.print") as printed:
            repos.main()
        output = json.loads(printed.call_args[0][0])
        self.assertEqual([view["id"] for view in output["views"]], ["me", "tektoncd"])
        self.assertEqual(output["view"], "me")


class MainTests(ReposTestCase):
    def run_main(self, *argv):
        with environment(), patch("sys.argv", ["repos.py", *argv]), patch(
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
        titles = [entry["title"] for entry in output["items"][0]["actions"]]
        self.assertEqual(titles[:2], ["Pull requests", "Issues"])
        self.assertNotIn("children", output["items"][0])

    def test_pages_flag(self):
        output = self.run_main("--pages", "pulls,issues", "me", "raffi")
        titles = [child["title"] for child in output["items"][0]["children"]]
        self.assertEqual(titles, ["Open owner/raffi", "Pull requests", "Issues"])

    def test_output_hints_the_cache_window(self):
        with patch.object(repos, "CACHE_REPOS", "24h"):
            output = self.run_main("--owners", "me", "raffi")
        self.assertEqual(output["cache"], {"seconds": 86400})

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
            repo("raffi"), repos.Options(path="pulls")
        )
        self.assertEqual(item["arg"], "https://github.com/owner/raffi/pulls")
        self.assertEqual(item["variables"]["REPO_URL"], "https://github.com/owner/raffi")
        self.assertIn("Pull requests", item["subtitle"])

    def test_path_applies_to_a_typed_repository(self):
        item = repos.repo_item("owner/raffi", repos.Options(path="releases"))
        self.assertEqual(item["arg"], "https://github.com/owner/raffi/releases")

    def titles_of(self, entries):
        return [entry["title"] for entry in entries]

    def test_actions_list_pages_then_copy_entries(self):
        item = repos.build_item(
            repo("raffi"),
            repos.Options(actions=(("pulls", "Pull requests"), ("issues", "Issues"))),
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
        self.assertNotIn("children", item)
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
            repos.Options(pages=(("pulls", "Pull requests"), ("issues", "Issues"))),
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
            repo("raffi"), repos.Options(pages=(("pulls", "Pull requests"),))
        )
        pulls = item["children"][1]
        self.assertEqual(
            self.titles_of(pulls["actions"]), ["Copy URL", "Copy Markdown link"]
        )
        self.assertEqual(
            pulls["actions"][1]["arg"],
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
            repos.Options(
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
            ],
        )


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
