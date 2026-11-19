#!/usr/bin/env python3
"""Raffi script filter for GitHub repositories.

Lists the repositories of a GitHub account: yours by default, or the account
you name or alias. The accounts given to --owners are Raffi views, switched
with Ctrl+Tab and passed in RAFFI_VIEW. Typing an account or an alias in front
of the query lists that account instead, and `owner/repo` opens one repository
directly.
"""

import argparse
import json
import os
import re
import subprocess
import time
from datetime import UTC, datetime
from pathlib import Path
from typing import NamedTuple

API_HOST = os.environ.get("API_HOST", "github.com")
CACHE_REPOS = os.environ.get("CACHE_REPOS", "10m")
CACHE_LOGIN = os.environ.get("CACHE_LOGIN", "1h")
CACHE_MISSING = os.environ.get("CACHE_MISSING", "24h")
REFRESH = os.environ.get("RAFFI_REFRESH") == "1"
ICON = "github"
PAGE_SIZE = 100
LIMIT = 500

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

OWNER = re.compile(r"[A-Za-z0-9][A-Za-z0-9-]{0,38}")
REPO = re.compile(r"[\w.-]+/[\w.-]+")
DURATION = re.compile(r"(\d+(?:\.\d+)?)([smhd])")
ANSI = re.compile(r"\x1b\[[0-9;]*m")

BOLD = "\033[1m"
GREY = "\033[90m"
RED = "\033[31m"
YELLOW = "\033[33m"
RESET = "\033[0m"

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


class Options(NamedTuple):
    """How the filter lists and links repositories."""

    limit: int = LIMIT
    # GitHub page each row opens, empty for the repository itself.
    path: str = ""
    # Pages offered in the action menu of each row, as (path, label) pairs.
    actions: tuple[tuple[str, str], ...] = ()


DEFAULTS = Options()


class GhError(Exception):
    pass


def run_gh(*args: str, timeout: float = 30) -> dict:
    try:
        result = subprocess.run(
            ["gh", *args], capture_output=True, text=True, timeout=timeout, check=False
        )
    except FileNotFoundError:
        raise GhError("GitHub CLI not found, see https://cli.github.com") from None
    except subprocess.TimeoutExpired:
        raise GhError(f"gh did not answer within {timeout} seconds") from None
    if result.returncode:
        raise GhError(result.stderr.strip() or f"gh exited with {result.returncode}")
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise GhError(f"gh returned invalid JSON: {error}") from None


def graphql(
    query: str, cache: str | None = CACHE_REPOS, timeout: float = 30, **variables
) -> dict:
    args = ["api", "graphql", "--hostname", API_HOST]
    if cache and not REFRESH:
        args += ["--cache", cache]
    for name, value in {"query": query, **variables}.items():
        if value is None:
            continue
        # -F sends integers as numbers, -f keeps everything else a string.
        args += ["-F" if isinstance(value, int) else "-f", f"{name}={value}"]
    response = run_gh(*args, timeout=timeout)
    if errors := response.get("errors"):
        raise GhError(errors[0].get("message", "GraphQL error"))
    return response["data"]


def login() -> str:
    """The account gh is logged in as, cached to spare a gh run."""
    if account := os.environ.get("GITHUB_USER"):
        return account
    entry = read_cache(login_file())
    if not REFRESH and fresh(entry.get("at"), CACHE_LOGIN):
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


def write_cache(file: Path, data: dict) -> None:
    try:
        CACHE_DIR.mkdir(parents=True, exist_ok=True)
        partial = file.with_suffix(".part")
        partial.write_text(json.dumps(data))
        partial.replace(file)
    except OSError:
        pass


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


def seconds(duration: str) -> float:
    """A gh-style duration such as 10m or 1h, in seconds."""
    match = DURATION.fullmatch(duration.strip())
    if not match:
        return 600
    units = {"s": 1, "m": 60, "h": 3600, "d": 86400}
    return float(match.group(1)) * units[match.group(2)]


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


def ago(timestamp: str | None) -> str:
    if not timestamp:
        return ""
    since = datetime.now(UTC) - datetime.fromisoformat(timestamp)
    elapsed = int(since.total_seconds())
    for unit, size in (
        ("y", 31536000),
        ("mo", 2592000),
        ("d", 86400),
        ("h", 3600),
        ("m", 60),
    ):
        if elapsed >= size:
            return f"{elapsed // size}{unit} ago"
    return "just now"


def colored(text: str, color: str) -> str:
    return f"{color}{text}{RESET}"


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
    if pushed := ago(repo.get("pushedAt")):
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


def child_item(title: str, url: str, repo: dict) -> dict:
    """One row of an action menu."""
    return {
        "title": title,
        "subtitle": url,
        "arg": url,
        "variables": repo_variables(repo),
        "preview": {
            "title": title,
            "subtitle": url,
            "icon": ICON,
            "text": f"Enter opens {url}\nCtrl+Enter copies it.",
            "dynamic": False,
        },
    }


def action_menu(repo: dict, options: Options) -> list[dict]:
    """The repository itself, then each page --actions offers for it."""
    menu = [child_item(f"Open {repo['nameWithOwner']}", repo["url"], repo)]
    menu += [
        child_item(label, page_url(repo["nameWithOwner"], path), repo)
        for path, label in options.actions
    ]
    return menu


def build_item(repo: dict, options: Options = DEFAULTS) -> dict:
    details = []
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
    if pushed := ago(repo.get("pushedAt")):
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
    if options.actions:
        item["children"] = action_menu(repo, options)
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
    if options.actions:
        item["children"] = action_menu(
            {"name": name.split("/", 1)[-1], "nameWithOwner": name, "url": root},
            options,
        )
    return item


def error_item(title: str, message: str) -> dict:
    return {
        "title": title,
        "subtitle": message,
        "valid": False,
        "preview": {"title": title, "text": message, "dynamic": False},
    }


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
    shown = ANSI.sub("", f"{item['title']} {item['subtitle']}")
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
    tektoncd` and `repo tektoncd pac` both work. A word that names one of your
    repositories, or the listed account itself, filters instead, and `me word`
    filters it whatever the word is.
    """
    owner, needle, direct, named_owner = resolve(query, view, owners)
    if direct:
        return owner, [repo_item(direct, options)]

    word, _, rest = query.strip().partition(" ")
    rest = rest.strip()
    same = word.casefold() == owner.casefold()
    # Look up the typed account before listing the current one, so `repo
    # tektoncd` only asks GitHub about tektoncd.
    if word and not named_owner and not same and (found := as_account(word, options.limit)):
        declared = any(word.casefold() == name.casefold() for name in owners)
        if declared or not repo_named(owner, word):
            return found["login"], items_for(found, rest.casefold().split(), options)

    account = account_repos(owner, options.limit)
    if not account:
        return owner, [
            error_item("Account not found", f"No GitHub account named {owner!r}")
        ]
    if same:
        return owner, items_for(account, rest.casefold().split(), options)
    return owner, items_for(account, needle.casefold().split(), options)


def repo_named(owner: str, name: str) -> bool:
    """Whether owner has a repository called name, in one small cached call.

    Looking at the account's repositories instead would fetch them all before
    a query that lists another account, which is the common case.
    """
    try:
        return bool(graphql(REPO_EXISTS_QUERY, owner=owner, name=name)["repository"])
    except GhError:
        return False


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


def items_for(
    account: dict, words: list[str], options: Options = DEFAULTS
) -> list[dict]:
    """The account's repositories matching words, then the account itself.

    Name matches come first, then initials, then anything shown or described;
    each group keeps the most recently pushed order GitHub returned. The
    account row comes last so it does not push real results down, but first
    when nothing matched and it is the only row worth selecting.
    """
    found: dict[int, list[dict]] = {}
    for repo in account["repos"]:
        item = build_item(repo, options)
        if (place := rank(repo, item, words)) is not None:
            found.setdefault(place, []).append(item)
    matches = [item for place in sorted(found) for item in found[place]]
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
        help="comma separated accounts or aliases listed as views, "
        "the first one being the default",
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
        help="pages offered in each row's action menu, as a comma separated "
        "list of paths, such as pulls,issues,releases or graphs/traffic:Traffic",
    )
    parser.add_argument("query", nargs="*", help="text typed after the Raffi keyword")
    args = parser.parse_args(argv)
    query = " ".join(args.query).strip()
    options = Options(
        limit=args.limit,
        path=args.path.strip().strip("/"),
        actions=parse_actions(args.actions),
    )

    # No views here: Raffi would replace the ones it knows with this single
    # row, losing the view Ctrl+Tab switched to.
    if direct := direct_repository(query):
        print(json.dumps({"items": [repo_item(direct, options)]}, ensure_ascii=False))
        return 0

    try:
        owners = [alias_target(word) or word for word in args.owners.split(",") if word]
        owners = owners or [login()]
        view = os.environ.get("RAFFI_VIEW", "")
        owner, items = list_items(query, view, owners, options)
    except GhError as error:
        print(json.dumps({"items": [error_item("GitHub error", str(error))]}))
        return 0

    views = [{"id": name, "title": name} for name in owners]
    if all(entry["id"] != owner for entry in views):
        views.append({"id": owner, "title": owner})
    print(
        json.dumps(
            {"views": views, "view": owner, "items": items}, ensure_ascii=False
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
