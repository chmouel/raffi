#!/usr/bin/env python3
"""Raffi script filter for GitHub repositories.

Lists the repositories of a GitHub account: yours by default, or the account
you name or alias. The accounts given to --owners are Raffi views, switched
with Ctrl+Tab and passed in RAFFI_VIEW; with more than one, the default All
view mixes their repositories. Typing one of those accounts or an
alias in front of the query lists that account instead, any GitHub account with
--any-owner, and `owner/repo` opens one repository directly.

The pull requests and issues pages of a repository are live menus when --pages
or --actions name them: their children are fetched on demand when expanded
(RAFFI_CHILDREN=1).
"""

import argparse
import json
import os
import re
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import NamedTuple

import ghcommon
from ghcommon import (
    ANSI,
    API_HOST,
    BOLD,
    GREEN,
    GREY,
    ICON,
    RED,
    RESET,
    TYPED_TIME,
    YELLOW,
    GhError,
    ago,
    colored,
    error_rows,
    seconds,
    time_field,
    time_text,
    with_cache,
)

CACHE_REPOS = os.environ.get("CACHE_REPOS", "10m")
CACHE_LOGIN = os.environ.get("CACHE_LOGIN", "1h")
CACHE_MISSING = os.environ.get("CACHE_MISSING", "24h")
CACHE_LIVE = os.environ.get("CACHE_LIVE", "10m")
PAGE_SIZE = 100
LIMIT = 500


def env_int(name: str, default: int) -> int:
    """An int from the environment, blank or badly written values aside."""
    try:
        return int(os.environ.get(name, "").strip() or default)
    except ValueError:
        return default


# How many rows a live menu shows by default.
LIVE_ITEMS = env_int("LIVE_ITEMS", 20)

# Paths listing GitHub state instead of linking a static page.
LIVE_PATHS = ("pulls", "issues")
# Where each live path keeps its nodes in the per-repository cache file.
LIVE_KEYS = {"pulls": "pull_requests", "issues": "issues"}

# Pages --actions can point a repository at, with the labels they get by
# default. A path missing here is labelled after its last segment.
PAGES = {
    "pulls": "Pull requests",
    "issues": "Issues",
    "releases": "Releases",
    "actions": "Actions",
    "tags": "Tags",
    "branches": "Branches",
    "commits": "Commits",
    "discussions": "Discussions",
    "wiki": "Wiki",
    "security": "Security",
    "settings": "Settings",
    "pulse": "Pulse",
    "graphs/traffic": "Traffic",
    "graphs/contributors": "Contributors",
}



def cache_dir() -> Path:
    """Where account lookups are remembered, per GitHub host."""
    root = Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache")
    return root / "raffi" / "github-repos" / API_HOST


CACHE_DIR = cache_dir()


def missing_file() -> Path:
    return CACHE_DIR / "missing.json"


def login_file() -> Path:
    return CACHE_DIR / "login.json"


# Short names accepted in front of the query, and as --owners entries. "me"
# stands for the account gh is logged in as; any other alias comes from an
# ALIAS_<name> variable in the filter's env.
ALIASES = {"me": ""}

# The view listing every --owners account together. Not a valid login, so it
# never names an account.
ALL_VIEW = "@all"

OWNER = re.compile(r"[A-Za-z0-9][A-Za-z0-9-]{0,38}")
REPO = re.compile(r"[\w.-]+/[\w.-]+")


OWNER_QUERY = """
query($owner: String!, $first: Int!, $after: String) {
  repositoryOwner(login: $owner) {
    login
    url
    repositories(
      first: $first
      after: $after
      orderBy: { field: PUSHED_AT, direction: DESC }
      ownerAffiliations: [OWNER]
    ) {
      totalCount
      pageInfo { hasNextPage endCursor }
      nodes {
        name
        nameWithOwner
        description
        url
        isPrivate
        isFork
        isArchived
        stargazerCount
        forkCount
        pushedAt
        updatedAt
        primaryLanguage { name }
        repositoryTopics(first: 20) { nodes { topic { name } } }
      }
    }
  }
}
"""

LOGIN_QUERY = """
query { viewer { login } }
"""

REPO_EXISTS_QUERY = """
query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) { name }
}
"""

LIVE_QUERY = """
query($owner: String!, $name: String!, $first: Int!) {
  repository(owner: $owner, name: $name) {
    nameWithOwner
    pullRequests(
      first: $first
      states: OPEN
      orderBy: { field: UPDATED_AT, direction: DESC }
    ) {
      totalCount
      nodes {
        number
        title
        url
        isDraft
        updatedAt
        author { login }
        headRefName
        reviewDecision
      }
    }
    issues(
      first: $first
      states: OPEN
      orderBy: { field: UPDATED_AT, direction: DESC }
    ) {
      totalCount
      nodes {
        number
        title
        url
        updatedAt
        author { login }
        labels(first: 3) { nodes { name } }
      }
    }
  }
}
"""

# Review decisions as the row shows them, and their colour.
REVIEWS = {
    "APPROVED": ("approved", GREEN),
    "CHANGES_REQUESTED": ("changes requested", RED),
    "REVIEW_REQUIRED": ("review required", YELLOW),
}


class Options(NamedTuple):
    """How the filter lists and links repositories."""

    limit: int = LIMIT
    # GitHub page each row opens, empty for the repository itself.
    path: str = ""
    # Pages listed in the action menu of each row, as (path, label) pairs.
    actions: tuple[tuple[str, str], ...] = ()
    # Pages Enter lists under each row before opening one, same pairs.
    pages: tuple[tuple[str, str], ...] = ()
    # Whether a typed word may list any GitHub account, not only --owners.
    any_owner: bool = False
    # Whether pulls and issues list the real pull requests and issues.
    live: bool = True
    # How many pull requests or issues a live menu lists.
    live_items: int = LIVE_ITEMS


DEFAULTS = Options()


def graphql(
    query: str, cache: str | None = CACHE_REPOS, timeout: float = 30, **variables
) -> dict:
    return ghcommon.graphql(query, cache=cache, timeout=timeout, **variables)


def login() -> str:
    """The account gh is logged in as, cached to spare a gh run."""
    if account := os.environ.get("GITHUB_USER"):
        return account
    entry = read_cache(login_file())
    if not ghcommon.REFRESH and fresh(entry.get("at"), CACHE_LOGIN):
        return entry["login"]
    account = graphql(LOGIN_QUERY, cache=CACHE_LOGIN)["viewer"]["login"]
    write_cache(login_file(), {"login": account, "at": time.time()})
    return account


def env_aliases() -> dict[str, str]:
    """The ALIAS_<name> variables of the filter's env, by lowercase name."""
    prefix = "ALIAS_"
    return {
        key[len(prefix) :].casefold(): value
        for key, value in os.environ.items()
        if key.startswith(prefix) and value
    }


def alias_target(word: str) -> str | None:
    """The account a typed name or ALIAS_<name> variable stands for."""
    if target := env_aliases().get(word.casefold()):
        return target
    if word.casefold() not in ALIASES:
        return None
    return ALIASES[word.casefold()] or login()


def read_cache(file: Path) -> dict:
    try:
        data = json.loads(file.read_text())
    except (OSError, ValueError):
        return {}
    return data if isinstance(data, dict) else {}


def write_cache(file: Path, data: object, strict: bool = False) -> None:
    """Write data to file so a reader never sees half of it.

    Each write gets a temporary file of its own: a background fetch and a
    Ctrl+R, or two threads of one run, can write the same file at once, and a
    shared name would let one truncate the file the other is renaming.
    strict re-raises an OSError instead of dropping the write, for a caller
    that has to know the file it publishes landed.
    """
    partial = None
    try:
        file.parent.mkdir(parents=True, exist_ok=True)
        handle, partial = tempfile.mkstemp(
            dir=file.parent, prefix=f"{file.name}.", suffix=".part"
        )
        with os.fdopen(handle, "w") as stream:
            stream.write(json.dumps(data))
        Path(partial).replace(file)
        partial = None
    except OSError:
        if strict:
            raise
    finally:
        if partial:
            Path(partial).unlink(missing_ok=True)


def fresh(at: object, duration: str) -> bool:
    """Whether a cached timestamp is younger than duration."""
    return isinstance(at, float) and time.time() - at < seconds(duration)


def recent_miss(name: str) -> bool:
    """Whether name was looked up and is not a GitHub account, lately."""
    return fresh(read_cache(missing_file()).get(name.casefold()), CACHE_MISSING)


def remember_miss(name: str) -> None:
    entries = {
        key: at
        for key, at in read_cache(missing_file()).items()
        if fresh(at, CACHE_MISSING)
    }
    entries[name.casefold()] = time.time()
    write_cache(missing_file(), entries)


# Repository existence answers of this run.
NAMED: dict[tuple[str, str], bool] = {}


def live_section(section: dict | None) -> dict:
    """A totalCount and the nodes of one live list, without empty entries."""
    section = section or {}
    return {
        "totalCount": section.get("totalCount") or 0,
        "nodes": [node for node in section.get("nodes") or [] if node],
    }


def fetch_live(name: str, first: int = LIVE_ITEMS) -> dict:
    """Fetch a repository's open pull requests and issues."""
    owner, _, repo = name.partition("/")
    data = graphql(
        LIVE_QUERY, cache=CACHE_LIVE, owner=owner, name=repo, first=max(1, first)
    )
    found = data.get("repository") or {}
    return {
        "pull_requests": live_section(found.get("pullRequests")),
        "issues": live_section(found.get("issues")),
    }


ACCOUNTS: dict[str, dict | None] = {}


def account_repos(name: str, limit: int = LIMIT) -> dict | None:
    """The account and its repositories, or None when no account has that name."""
    if name in ACCOUNTS:
        return ACCOUNTS[name]
    repos: list[dict] = []
    after = None
    while True:
        data = graphql(OWNER_QUERY, owner=name, first=PAGE_SIZE, after=after)
        account = data.get("repositoryOwner")
        if not account:
            remember_miss(name)
            ACCOUNTS[name] = None
            return None
        page = account["repositories"]
        repos.extend(repo for repo in page["nodes"] if repo)
        after = page["pageInfo"]["endCursor"]
        if not page["pageInfo"]["hasNextPage"] or len(repos) >= max(limit, 1):
            break
    ACCOUNTS[name] = {
        "login": account["login"],
        "url": account["url"],
        "total": page["totalCount"],
        "repos": repos[: max(limit, 1)],
    }
    return ACCOUNTS[name]


def resolve(
    query: str, view: str, owners: list[str]
) -> tuple[str, str, str | None, bool]:
    """Split the query into (account, filter, direct repository, named account).

    The last value is True when the first word was an account or an alias, so
    that it is not looked up as an account again.
    """
    owner = view if OWNER.fullmatch(view) else owners[0]
    word, _, rest = query.strip().partition(" ")
    rest = rest.strip()
    if not word:
        return owner, "", None, False
    if target := alias_target(word):
        return target, rest, None, True
    if direct := direct_repository(query):
        return direct.split("/", 1)[0], "", direct, True
    return owner, query.strip(), None, False


def as_account(word: str, limit: int = LIMIT) -> dict | None:
    """word as a GitHub account, unless it cannot be one or was ruled out."""
    if not word or not OWNER.fullmatch(word) or recent_miss(word):
        return None
    return account_repos(word, limit)


def direct_repository(query: str) -> str | None:
    """The owner/repo a query names, to open without asking GitHub."""
    word, _, rest = query.strip().partition(" ")
    return word if REPO.fullmatch(word) and not rest.strip() else None


def count(number: int) -> str:
    if number < 1000:
        return str(number)
    if number < 1_000_000:
        return f"{number / 1000:.1f}k".replace(".0k", "k")
    return f"{number / 1_000_000:.1f}M".replace(".0M", "M")


def shorten(text: str, width: int = 80) -> str:
    text = " ".join(text.split())
    return text if len(text) <= width else text[: width - 1] + "\u2026"


def topics(repo: dict) -> list[str]:
    nodes = (repo.get("repositoryTopics") or {}).get("nodes") or []
    return [node["topic"]["name"] for node in nodes if node and node.get("topic")]


def language(repo: dict) -> str:
    return (repo.get("primaryLanguage") or {}).get("name") or ""


def build_preview(repo: dict) -> dict:
    metadata = [["Repository", repo["nameWithOwner"]]]
    if language(repo):
        metadata.append(["Language", language(repo)])
    metadata.append(["Stars", count(repo["stargazerCount"])])
    metadata.append(["Forks", count(repo["forkCount"])])
    metadata.append(["Visibility", "private" if repo["isPrivate"] else "public"])
    if repo["isArchived"]:
        metadata.append(["State", "archived"])
    if repo["isFork"]:
        metadata.append(["Fork of", "another repository"])
    # Raffi adds a live row of its own for a typed time.
    if not TYPED_TIME and (pushed := ago(repo.get("pushedAt"))):
        metadata.append(["Last push", pushed])
    if repo.get("updatedAt"):
        metadata.append(["Updated", ago(repo["updatedAt"])])
    if found := topics(repo):
        metadata.append(["Topics", ", ".join(found)])
    return {
        "title": repo["name"],
        "subtitle": repo["nameWithOwner"],
        "icon": ICON,
        "text": repo.get("description") or "(no description)",
        "metadata": metadata,
        "dynamic": False,
    }


def repo_variables(repo: dict) -> dict:
    name = repo["nameWithOwner"]
    return {
        "REPO_URL": repo["url"],
        "REPO_NAME": name,
        "REPO_OWNER": name.split("/", 1)[0],
        "REPO_SSH": f"git@{API_HOST}:{name}.git",
        "REPO_CLONE": f"https://{API_HOST}/{name}.git",
    }


def parse_actions(spec: str) -> tuple[tuple[str, str], ...]:
    """Turn `pulls,issues:Issues,graphs/traffic` into (path, label) pairs."""
    actions = []
    for entry in spec.split(","):
        entry = entry.strip().strip("/")
        if not entry:
            continue
        path, _, label = entry.partition(":")
        path = path.strip().strip("/")
        if not path:
            continue
        label = label.strip() or PAGES.get(path) or path.rsplit("/", 1)[-1].replace(
            "-", " "
        ).title()
        actions.append((path, label))
    return tuple(actions)


def page_url(name: str, path: str) -> str:
    """The URL of a repository's page, such as its pull requests."""
    return f"https://{API_HOST}/{name}/{path}" if path else f"https://{API_HOST}/{name}"


def copy_actions(url: str, label: str, clone: bool = False) -> list[dict]:
    """Copy entries for the action menu of a row that opens url."""
    entries = [
        {"title": "Copy URL", "arg": url, "action": "copy"},
        {
            "title": "Copy Markdown link",
            "arg": f"[{label}]({url})",
            "action": "copy",
        },
    ]
    if clone:
        name = url.removeprefix(f"https://{API_HOST}/")
        entries += [
            {
                "title": "Copy SSH clone URL",
                "arg": f"git@{API_HOST}:{name}.git",
                "action": "copy",
            },
            {
                "title": "Copy HTTPS clone URL",
                "arg": f"https://{API_HOST}/{name}.git",
                "action": "copy",
            },
            {"title": "Copy name", "arg": name, "action": "copy"},
        ]
    return entries


def row_actions(name: str, options: Options) -> list[dict]:
    """The action menu of a repository: --actions pages, then copy entries."""
    pages = [
        {"title": label, "arg": page_url(name, path)} for path, label in options.actions
    ]
    return pages + copy_actions(page_url(name, ""), name, clone=True)


def link_leaf(
    name: str, url: str, title: str, variables: dict, page: str = ""
) -> dict:
    """A link row of a repository: Enter opens url.

    The row says all there is to say about a plain link, so the preview
    repeats none of it and opts out of dynamic previews, which would only
    re-run this script for nothing. page labels its Markdown link, and is
    empty for the repository itself.
    """
    return {
        "title": title,
        "subtitle": url,
        "arg": url,
        "variables": variables,
        "actions": copy_actions(url, f"{page} \u00b7 {name}" if page else name),
        "preview": {
            "text": f"Enter opens {url}\nCtrl+Enter copies it.",
            "icon": ICON,
            "dynamic": False,
        },
    }


def live_paths(options: Options) -> tuple[tuple[str, str], ...]:
    """The live pages named by --pages or --actions, --pages first."""
    if not options.live:
        return ()
    found: dict[str, str] = {}
    for path, label in (*options.pages, *options.actions):
        if path in LIVE_PATHS and path not in found:
            found[path] = label
    return tuple(found.items())


def live_preview(label: str, url: str, name: str) -> dict:
    """What the preview says about a live menu."""
    pages = label.lower()
    text = (
        f"Enter lists the open {pages} of {name}.\n"
        f"Right \u2192 Copy URL copies {url}; Open on GitHub opens it."
    )
    return {
        "title": label,
        "subtitle": url,
        "icon": ICON,
        "text": text,
        "dynamic": False,
    }


def pr_leaf(name: str, pr: dict, variables: dict) -> dict:
    """A row for an open pull request, carrying what pr.py's actions need."""
    url = pr["url"]
    number = pr["number"]
    title = f"#{number} {pr.get('title') or ''}"
    details = []
    if author := (pr.get("author") or {}).get("login"):
        details.append(f"@{author}")
    if pr.get("isDraft"):
        details.append(colored("draft", YELLOW))
    if review := REVIEWS.get(pr.get("reviewDecision") or ""):
        details.append(colored(*review))
    if not TYPED_TIME and (updated := ago(pr.get("updatedAt"))):
        details.append(f"updated {updated}")
    item = {
        "title": title,
        "subtitle": " \u00b7 ".join(details),
        "arg": url,
        "variables": {
            **variables,
            "PR_URL": url,
            "PR_REPO": name,
            "PR_NUMBER": str(number),
            "PR_BRANCH": pr.get("headRefName") or "",
        },
        "actions": copy_actions(url, f"{name}#{number}"),
        "preview": {
            "title": title,
            "subtitle": url,
            "icon": ICON,
            "text": pr.get("title") or "",
            "dynamic": False,
        },
    }
    if TYPED_TIME and (when := time_field(pr.get("updatedAt"), "updated")):
        item["time"] = when
    return item


def issue_leaf(name: str, issue: dict, variables: dict) -> dict:
    """A row for an open issue, carrying what issue actions need."""
    url = issue["url"]
    number = issue["number"]
    title = f"#{number} {issue.get('title') or ''}"
    details = []
    if author := (issue.get("author") or {}).get("login"):
        details.append(f"@{author}")
    labels = [
        node["name"]
        for node in (issue.get("labels") or {}).get("nodes") or []
        if node and node.get("name")
    ]
    if labels:
        details.append(", ".join(labels[:3]))
    if not TYPED_TIME and (updated := ago(issue.get("updatedAt"))):
        details.append(f"updated {updated}")
    item = {
        "title": title,
        "subtitle": " \u00b7 ".join(details),
        "arg": url,
        "variables": {
            **variables,
            "ISSUE_URL": url,
            "ISSUE_REPO": name,
            "ISSUE_NUMBER": str(number),
        },
        "actions": copy_actions(url, f"{name}#{number}"),
        "preview": {
            "title": title,
            "subtitle": url,
            "icon": ICON,
            "text": issue.get("title") or "",
            "dynamic": False,
        },
    }
    if TYPED_TIME and (when := time_field(issue.get("updatedAt"), "updated")):
        item["time"] = when
    return item


def live_menu(name: str, path: str, label: str, options: Options) -> dict:
    """A page of a repository whose open rows are fetched lazily on Enter."""
    del options
    url = page_url(name, path)
    variables = {
        **repo_variables({"nameWithOwner": name, "url": page_url(name, "")}),
        "LIVE_PATH": path,
        "LIVE_LABEL": label,
    }
    return {
        "title": label,
        "subtitle": f"open {url}",
        "arg": url,
        "variables": variables,
        "actions": [{"title": "Open on GitHub", "arg": url}]
        + copy_actions(url, f"{label} \u00b7 {name}"),
        "preview": live_preview(label, url, name),
        "children_lazy": True,
    }


def live_children(options: Options) -> dict:
    """Answer RAFFI_CHILDREN=1 for a live pull requests or issues row."""
    name = os.environ.get("REPO_NAME", "").strip()
    path = os.environ.get("LIVE_PATH", "").strip()
    label = os.environ.get("LIVE_LABEL") or PAGES.get(path, path.title())
    if not name or path not in LIVE_PATHS:
        return {"items": [], "cache": {"seconds": 0}}
    url = page_url(name, path)
    variables = repo_variables({"nameWithOwner": name, "url": page_url(name, "")})
    whole_page = link_leaf(
        name, url, f"All {label.lower()} on GitHub", variables, label
    )
    try:
        entry = fetch_live(name, options.live_items)
    except GhError as error:
        return {
            "items": error_rows("GitHub error", str(error)) + [whole_page],
            "cache": {"seconds": 0},
        }
    section = entry.get(LIVE_KEYS[path]) or {}
    nodes = (section.get("nodes") or [])[: max(options.live_items, 0)]
    build = pr_leaf if path == "pulls" else issue_leaf
    items = [build(name, node, variables) for node in nodes if node] + [whole_page]
    return with_cache({}, items, CACHE_LIVE)


def page_children(name: str, options: Options) -> list[dict]:
    """What Enter lists under a repository: itself, then each --pages page.

    A live page becomes a menu of its own, and a live page only --actions
    names is listed after the --pages ones.
    """
    variables = repo_variables({"nameWithOwner": name, "url": page_url(name, "")})
    children = [link_leaf(name, page_url(name, ""), f"Open {name}", variables)]
    listed = {path for path, _ in options.pages}
    for path, label in options.pages:
        if options.live and path in LIVE_PATHS:
            children.append(live_menu(name, path, label, options))
        else:
            children.append(
                link_leaf(name, page_url(name, path), label, variables, label)
            )
    for path, label in live_paths(options):
        if path not in listed:
            children.append(live_menu(name, path, label, options))
    return children


def with_menus(item: dict, name: str, options: Options) -> dict:
    """Give a repository row its action menu; attach_children adds its pages."""
    item["actions"] = row_actions(name, options)
    return item


def wants_children(options: Options) -> bool:
    """Whether repository rows list pages under them."""
    return bool(options.pages or live_paths(options))


def attach_children(items: list[dict], options: Options) -> None:
    """Give the drawn repository rows their pages, in the order they show.

    The children carry the live pull request and issue lists, so they are
    built once the ranking has kept its rows: only a row the user can see
    queues a fetch, and the fetches follow the order of the list.
    """
    if not wants_children(options):
        return
    for item in items:
        name = (item.get("variables") or {}).get("REPO_NAME")
        if name and "children" not in item:
            item["children"] = page_children(name, options)


def build_item(
    repo: dict, options: Options = DEFAULTS, owner: bool = False, menus: bool = True
) -> dict:
    """A repository row.

    menus leaves the pages to attach_children, for the rows ranked() builds
    for every candidate rather than for the ones it keeps.
    """
    details = [repo["nameWithOwner"].split("/", 1)[0]] if owner else []
    if description := repo.get("description"):
        details.append(shorten(description))
    if language(repo):
        details.append(language(repo))
    if repo["stargazerCount"]:
        details.append(f"\u2605 {count(repo['stargazerCount'])}")
    if repo["isArchived"]:
        details.append(colored("archived", RED))
    if repo["isPrivate"]:
        details.append(colored("private", YELLOW))
    if repo["isFork"]:
        details.append(colored("fork", GREY))
    if not TYPED_TIME and (pushed := ago(repo.get("pushedAt"))):
        details.append(colored(f"pushed {pushed}", GREY))
    if options.path:
        details.append(colored(PAGES.get(options.path) or options.path, GREY))
    name = repo["name"]
    item = {
        "title": colored(name, GREY) if repo["isArchived"] else name,
        "subtitle": " \u00b7 ".join(details),
        "arg": page_url(repo["nameWithOwner"], options.path),
        "variables": repo_variables(repo),
        "preview": build_preview(repo),
    }
    if TYPED_TIME and (pushed := time_field(repo.get("pushedAt"), "pushed")):
        item["time"] = pushed
    item = with_menus(item, repo["nameWithOwner"], options)
    if menus:
        attach_children([item], options)
    return item


def account_item(account: dict) -> dict:
    url = f"{account['url']}?tab=repositories"
    total = count(account["total"])
    return {
        "title": f"{BOLD}{account['login']}{RESET}",
        "subtitle": f"{total} repositories \u00b7 open the account on {API_HOST}",
        "arg": url,
        "variables": {
            "REPO_URL": url,
            "REPO_OWNER": account["login"],
        },
        "preview": {
            "title": account["login"],
            "subtitle": url,
            "icon": ICON,
            "text": f"Enter opens {url}\nCtrl+Enter copies it.",
            "dynamic": False,
        },
    }


def repo_item(name: str, options: Options = DEFAULTS) -> dict:
    """A single repository typed as owner/repo, without asking GitHub."""
    url = page_url(name, options.path)
    root = f"https://{API_HOST}/{name}"
    item = {
        "title": name,
        "subtitle": f"open {url}",
        "arg": url,
        "variables": repo_variables({"nameWithOwner": name, "url": root}),
        "preview": {
            "title": name,
            "subtitle": url,
            "icon": ICON,
            "text": f"Enter opens {url}\nCtrl+Enter copies it.",
            "dynamic": False,
        },
    }
    item = with_menus(item, name, options)
    attach_children([item], options)
    if "children" in item:
        # Enter opens the children, not the page the preview would promise.
        item["preview"]["text"] = (
            f"Enter lists the pages of {name}.\n"
            f"Right \u2192 Copy URL copies {url}."
        )
    return item


def initials(name: str) -> str:
    """The first letter of each hyphen, dot or underscore separated part."""
    return "".join(part[0] for part in re.split(r"[-_.]", name) if part).casefold()


def rank(repo: dict, item: dict, words: list[str]) -> int | None:
    """How well a repository matches, 0 first, or None when it does not.

    Names answer to their initials too, so `pac` finds pipelines-as-code.
    """
    if not words:
        return 0
    name = repo["name"].casefold()
    short = initials(repo["name"])
    shown = ANSI.sub("", f"{item['title']} {item['subtitle']} {time_text(item)}")
    # The row shows a shortened description, so search the whole one.
    rest = f"{repo.get('description') or ''} {' '.join(topics(repo))}"
    text = f"{shown} {rest}".casefold()

    def abbreviated(word: str) -> bool:
        return bool(word) and word in short

    if all(word in name for word in words):
        return 0
    if all(word in name or abbreviated(word) for word in words):
        return 1
    if all(word in text or abbreviated(word) for word in words):
        return 2
    return None


def list_items(
    query: str, view: str, owners: list[str], options: Options = DEFAULTS
) -> tuple[str, list[dict]]:
    """The repositories for the query, and the account they come from.

    A first word that is a GitHub account lists that account, so `repo
    acme` and `repo acme api` both work. Without --any-owner only the
    configured accounts and aliases are looked up. A word that names one of
    your repositories, or the listed account itself, filters instead, and `me
    word` filters it whatever the word is.
    """
    all_view = len(owners) > 1 and view in ("", ALL_VIEW)
    if not options.any_owner and not is_owner(view, owners):
        view = ""
    owner, needle, direct, named_owner = resolve(query, view, owners)
    if direct:
        return owner, [repo_item(direct, options)]

    word, _, rest = query.strip().partition(" ")
    rest = rest.strip()
    same = word.casefold() == owner.casefold()
    # Look up the typed account before listing the current one, so `repo
    # acme` only asks GitHub about acme.
    declared = is_owner(word, owners)
    if (
        word
        and not named_owner
        and not same
        and (declared or options.any_owner)
        and (found := as_account(word, options.limit))
    ):
        # The All view lists every configured account, so a repository of
        # any of them makes the word a filter.
        listed = owners if all_view else [owner]
        if declared or not any(repo_named(name, word) for name in listed):
            return found["login"], items_for(found, rest.casefold().split(), options)

    if all_view and not named_owner and not same:
        return ALL_VIEW, mixed_items(owners, needle.casefold().split(), options)

    account = account_repos(owner, options.limit)
    if not account:
        return owner, error_rows(
            "Account not found", f"No GitHub account named {owner!r}"
        )
    if same:
        return owner, items_for(account, rest.casefold().split(), options)
    return owner, items_for(account, needle.casefold().split(), options)


def is_owner(word: str, owners: list[str]) -> bool:
    return any(word.casefold() == name.casefold() for name in owners)


def repo_named(owner: str, name: str) -> bool:
    """Whether owner has a repository called name, in one small cached call.

    Looking at the account's repositories instead would fetch them all before
    a query that lists another account, which is the common case. The answer
    is kept for the run, so the second pass of a Ctrl+R resolves the same
    owner: it runs without RAFFI_REFRESH and could otherwise see a repository
    that only the gh cache knew.
    """
    key = (owner.casefold(), name.casefold())
    if key not in NAMED:
        try:
            NAMED[key] = bool(
                graphql(REPO_EXISTS_QUERY, owner=owner, name=name)["repository"]
            )
        except GhError:
            NAMED[key] = False
    return NAMED[key]


def no_match_item(account: dict, needle: str) -> dict:
    total = count(account["total"])
    return {
        "title": f"No repository matches {needle!r}",
        "subtitle": f"{account['login']} has {total} repositories",
        "valid": False,
        "preview": {
            "title": "No match",
            "text": f"{account['login']} has {total} repositories and none of "
            f"them matches {needle!r}.",
            "dynamic": False,
        },
    }


def ranked(
    repos: list[dict], words: list[str], options: Options, owner: bool = False
) -> list[dict]:
    """The repositories matching words, forks last.

    Name matches come first, then initials, then anything shown or described;
    each group keeps the most recently pushed order of repos.
    """
    found: dict[tuple[bool, int], list[dict]] = {}
    for repo in repos:
        item = build_item(repo, options, owner, menus=False)
        if (place := rank(repo, item, words)) is not None:
            found.setdefault((bool(repo["isFork"]), place), []).append(item)
    return [item for key in sorted(found) for item in found[key]]


def mixed_items(owners: list[str], words: list[str], options: Options) -> list[dict]:
    """The repositories of every account, by last push, each row naming its owner."""
    names = list({name.casefold(): name for name in owners}.values())
    # A refresh skips the gh cache, so fetch the accounts side by side.
    with ThreadPoolExecutor(max_workers=len(names)) as pool:
        fetched = list(pool.map(lambda name: account_repos(name, options.limit), names))
    accounts = [found for found in fetched if found]
    if not accounts:
        listed = ", ".join(owners)
        return error_rows("Account not found", f"No GitHub account among {listed}")
    repos = sorted(
        (repo for account in accounts for repo in account["repos"]),
        key=lambda repo: repo.get("pushedAt") or "",
        reverse=True,
    )
    if matches := ranked(repos, words, options, owner=True):
        return matches
    combined = {
        "login": ", ".join(account["login"] for account in accounts),
        "total": sum(account["total"] for account in accounts),
    }
    return [no_match_item(combined, " ".join(words))] if words else []


def items_for(
    account: dict, words: list[str], options: Options = DEFAULTS
) -> list[dict]:
    """The account's repositories matching words, then the account itself.

    The account row comes last so it does not push real results down, but
    first when nothing matched and it is the only row worth selecting.
    """
    matches = ranked(account["repos"], words, options)
    if not matches:
        items = [account_item(account)]
        if words:
            items.append(no_match_item(account, " ".join(words)))
        return items
    return matches + [account_item(account)]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--owners",
        default=os.environ.get("OWNERS", "me"),
        help="comma separated accounts or aliases listed as views; "
        "with several, the default view is All, mixing them",
    )
    parser.add_argument(
        "--limit", type=int, default=LIMIT, help="maximum repositories to list"
    )
    parser.add_argument(
        "--path",
        default=os.environ.get("REPO_PATH", ""),
        help="GitHub page each row opens, such as pulls or releases",
    )
    parser.add_argument(
        "--actions",
        default=os.environ.get("REPO_ACTIONS", ""),
        help="pages listed in each row's action menu (Right), as a comma "
        "separated list of paths, such as pulls,issues,releases or "
        "graphs/traffic:Traffic",
    )
    parser.add_argument(
        "--pages",
        default=os.environ.get("REPO_PAGES", ""),
        help="pages Enter lists under each row, same syntax as --actions",
    )
    parser.add_argument(
        "--no-usage-sort",
        action="store_true",
        default=os.environ.get("USAGE_SORT") == "0",
        help="keep the script's order instead of asking Raffi to list the "
        "repositories you picked first",
    )
    parser.add_argument(
        "--any-owner",
        action="store_true",
        default=os.environ.get("ANY_OWNER") == "1",
        help="let a typed word list any GitHub account, not only --owners "
        "and aliases",
    )
    parser.add_argument(
        "--live-items",
        type=int,
        default=env_int("LIVE_ITEMS", LIVE_ITEMS),
        help="how many pull requests or issues a live page lists, and how "
        "many of each list the fetch asks GitHub for",
    )
    parser.add_argument(
        "--no-live",
        action="store_true",
        default=os.environ.get("LIVE") == "0",
        help="link the pull requests and issues pages instead of listing "
        "the open ones",
    )
    parser.add_argument("query", nargs="*", help="text typed after the Raffi keyword")
    args = parser.parse_args(argv)
    NAMED.clear()
    query = " ".join(args.query).strip()
    options = Options(
        limit=args.limit,
        path=args.path.strip().strip("/"),
        actions=parse_actions(args.actions),
        pages=parse_actions(args.pages),
        any_owner=args.any_owner,
        live=not args.no_live,
        live_items=args.live_items,
    )

    if os.environ.get("RAFFI_CHILDREN") == "1":
        print(json.dumps(live_children(options), ensure_ascii=False))
        return 0

    # No views here: Raffi would replace the ones it knows with this single
    # row, losing the view Ctrl+Tab switched to.
    if direct := direct_repository(query):
        items = [repo_item(direct, options)]
        attach_children(items, options)
        print(json.dumps(with_cache({}, items, CACHE_REPOS), ensure_ascii=False))
        return 0

    try:
        owners = [alias_target(word) or word for word in args.owners.split(",") if word]
        owners = owners or [login()]
        view = os.environ.get("RAFFI_VIEW", "")
        owner, items = list_items(query, view, owners, options)
        attach_children(items, options)
    except GhError as error:
        items = error_rows("GitHub error", str(error))
        print(json.dumps(with_cache({}, items, CACHE_REPOS), ensure_ascii=False))
        return 0

    views = [{"id": name, "title": name} for name in owners]
    if len(owners) > 1:
        views.insert(0, {"id": ALL_VIEW, "title": "All"})
    if owner != ALL_VIEW and all(entry["id"] != owner for entry in views):
        views.append({"id": owner, "title": owner})
    output = {"views": views, "view": owner}
    if not args.no_usage_sort:
        output["sort"] = "usage"
    print(json.dumps(with_cache(output, items, CACHE_REPOS), ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
