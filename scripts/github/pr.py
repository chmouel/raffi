#!/usr/bin/env python3
"""Raffi script filter for GitHub pull requests.

Lists the open pull requests of a repository, or the open pull requests you
wrote, were asked to review, are assigned to, or are involved in. When Raffi
sets RAFFI_PREVIEW=1, it prints a preview of the pull request whose URL is in
RAFFI_VALUE instead.
"""

import argparse
import base64
import json
import os
import re
import subprocess
import urllib.request
from collections import Counter
from datetime import UTC, datetime
from pathlib import Path

API_HOST = os.environ.get("API_HOST", "github.com")
CACHE_PULLS = os.environ.get("CACHE_PULLS", "10m")
REFRESH = os.environ.get("RAFFI_REFRESH") == "1"
ICON = "github"
# Raffi gives the preview command 3 seconds in total.
PREVIEW_TIMEOUT = 2.5
PREVIEW_REFRESH_SECONDS = 20
PREVIEW_COMMENTS = 3
PREVIEW_FILES = 50
# Accounts whose comments the preview skips, besides GitHub Apps.
BOT_SUFFIXES = ("[bot]", "-bot", "-robot")

CACHE_HOME = Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache")
AVATAR_DIR = CACHE_HOME / "raffi" / "github-avatars" / API_HOST
AVATAR_TIMEOUT = 3
# Raffi loads PNG and SVG icons only and GitHub avatars can be JPEG, so they
# are embedded in an SVG, clipped to a circle.
AVATAR_SVG = (
    '<svg xmlns="http://www.w3.org/2000/svg" '
    'xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 128 128">'
    '<clipPath id="round"><circle cx="64" cy="64" r="64"/></clipPath>'
    '<image width="128" height="128" clip-path="url(#round)" '
    'xlink:href="data:{mime};base64,{data}"/></svg>'
)

REPO = re.compile(r"[\w.-]+/[\w.-]+")
PR_URL = re.compile(r"https?://[^/]+/([\w.-]+)/([\w.-]+)/pull/(\d+)")
ANSI = re.compile(r"\x1b\[[0-9;]*m")
HTML_COMMENT = re.compile(r"<!--.*?-->", re.DOTALL)
BLANK_LINES = re.compile(r"\n{3,}")

# Markdown, rendered with the ANSI codes Raffi supports: colours, bold, underline.
FENCE = re.compile(r"\s*(```|~~~)")
HEADING = re.compile(r"#{1,6}\s+(.*?)[\s#]*$")
RULE = re.compile(r"\s*([-*_])(\s*\1){2,}\s*$")
LIST_ITEM = re.compile(r"(\s*)[-*+]\s+(?:\[([ xX])\]\s+)?(.*)")
QUOTE = re.compile(r"\s*>\s?(.*)")
ALERT = re.compile(r"\[!(\w+)\]$")
TABLE_RULE = re.compile(r"\s*\|?(\s*:?-+:?\s*\|)+\s*(:?-+:?)?\s*$")
CODE_SPAN = re.compile(r"(`+)(.+?)\1")
HTML_TAG = re.compile(r"</?[a-zA-Z][^>]*>")
IMAGE = re.compile(r"!\[([^\]]*)\]\([^)]*\)")
LINK = re.compile(r"\[([^\]]+)\]\([^)]*\)")
STRONG = re.compile(r"(\*\*|__)(.+?)\1")
EMPHASIS = re.compile(r"(?<![\w*])([*_])(?!\s)(.+?)(?<!\s)\1(?![\w*])")

GREEN = "\033[32m"
RED = "\033[31m"
YELLOW = "\033[33m"
GREY = "\033[90m"
RESET = "\033[0m"

# In the order checks are listed in the preview.
CI_STYLES = {
    "failed": ("\uf057", RED, "Failed"),
    "pending": ("\uf017", YELLOW, "InProgress"),
    "passed": ("\uf058", GREEN, "Passing"),
    "skipped": ("\uf056", GREY, "Skipped"),
}
FAILED = {
    "FAILURE",
    "ERROR",
    "TIMED_OUT",
    "CANCELLED",
    "ACTION_REQUIRED",
    "STARTUP_FAILURE",
}
PENDING = {"PENDING", "EXPECTED"}

REVIEW = {
    "APPROVED": "Approved",
    "CHANGES_REQUESTED": "Changes requested",
    "REVIEW_REQUIRED": "Review required",
}
REVIEW_COLORS = {"APPROVED": GREEN, "CHANGES_REQUESTED": RED, "REVIEW_REQUIRED": YELLOW}
# Verdicts of individual reviewers, in the order they are listed.
REVIEWER_MARKS = {
    "APPROVED": "\u2713",
    "CHANGES_REQUESTED": "\u2717",
    "COMMENTED": "\U0001f4ac",
}

MERGE_STATE = {
    "BEHIND": "Behind base",
    "BLOCKED": "Blocked",
    "CLEAN": "Ready",
    "DIRTY": "Conflicts",
    "DRAFT": "Draft",
    "HAS_HOOKS": "Ready",
    "UNKNOWN": "Checking",
    "UNSTABLE": "Ready, with failing checks",
}

CHANGE_STYLES = {
    "ADDED": ("A", GREEN),
    "DELETED": ("D", RED),
    "RENAMED": ("R", "\033[36m"),
    "COPIED": ("C", "\033[36m"),
}

# First query words listing pull requests across every repository: the search
# query, then the title and GitHub page of the first row.
MODES = {
    "myprs": ("author:@me", "Open your pull requests dashboard", "pulls"),
    "review": (
        "review-requested:@me",
        "Open your review requests",
        "pulls/review-requested",
    ),
    "assigned": ("assignee:@me", "Open your assigned pull requests", "pulls/assigned"),
    "involved": (
        "involves:@me",
        "Open the pull requests involving you",
        "pulls?q=is%3Aopen+is%3Apr+involves%3A%40me",
    ),
}

PR_FRAGMENT = """
fragment pr on PullRequest {
  number title url isDraft headRefName reviewDecision mergeable
  additions deletions updatedAt
  repository { nameWithOwner owner { login avatarUrl(size: 128) } }
  author { login avatarUrl(size: 128) }
  reviewThreads(first: 100) { nodes { isResolved } }
  commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
}
"""
REPO_QUERY = (
    """
query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    pullRequests(states: OPEN, first: 50, orderBy: {field: CREATED_AT, direction: DESC}) {
      nodes { ...pr }
    }
  }
}
"""
    + PR_FRAGMENT
)
SEARCH_QUERY = (
    """
query($q: String!) {
  search(query: $q, type: ISSUE, first: 50) {
    nodes { ...pr }
  }
}
"""
    + PR_FRAGMENT
)

# Everything the preview shows, in one request: each request costs about a
# second and Raffi gives the preview 3.
PREVIEW_QUERY = """
query($owner: String!, $name: String!, $number: Int!, $files: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      number title url state isDraft body createdAt updatedAt mergedAt
      headRefName baseRefName reviewDecision mergeable mergeStateStatus
      additions deletions changedFiles
      author { login }
      repository { nameWithOwner }
      autoMergeRequest { mergeMethod enabledBy { login } }
      milestone { title }
      assignees(first: 10) { nodes { login } }
      labels(first: 20) { nodes { name } }
      reviewRequests(first: 20) {
        nodes { requestedReviewer { ... on User { login } ... on Team { name } } }
      }
      latestReviews(first: 20) { nodes { author { login } state } }
      reviewThreads(first: 100) { nodes { isResolved } }
      comments(last: 10) {
        totalCount
        nodes { author { __typename login } createdAt body }
      }
      closingIssuesReferences(first: 10) {
        nodes { number title state repository { nameWithOwner } }
      }
      files(first: $files) {
        totalCount
        nodes { path additions deletions changeType }
      }
      commits(last: 1) {
        totalCount
        nodes {
          commit {
            messageHeadline
            statusCheckRollup {
              contexts(first: 100) {
                nodes {
                  __typename
                  ... on CheckRun {
                    name status conclusion
                    checkSuite { workflowRun { workflow { name } } }
                  }
                  ... on StatusContext { context state }
                }
              }
            }
          }
        }
      }
    }
  }
}
"""


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
    query: str, cache: str | None = CACHE_PULLS, timeout: float = 30, **variables
) -> dict:
    args = ["api", "graphql", "--hostname", API_HOST]
    if cache and not REFRESH:
        args += ["--cache", cache]
    for name, value in {"query": query, **variables}.items():
        # -F sends integers as numbers, -f keeps everything else a string.
        args += ["-F" if isinstance(value, int) else "-f", f"{name}={value}"]
    response = run_gh(*args, timeout=timeout)
    if errors := response.get("errors"):
        raise GhError(errors[0].get("message", "GraphQL error"))
    return response["data"]


def parse_query(query: str, repo: str | None = None) -> tuple[str, str]:
    """Split the typed text into (repository or a MODES key, filter)."""
    query = query.strip()
    if repo:
        return os.environ.get(f"ALIAS_{repo}", repo), query
    word, _, rest = query.partition(" ")
    rest = rest.strip()
    if word in MODES:
        return word, rest
    if alias := os.environ.get(f"ALIAS_{word}"):
        return alias, rest
    if "/" in word:
        return word, rest
    return "myprs", query


def ci_group(state: str) -> str:
    if state in FAILED:
        return "failed"
    if state in PENDING:
        return "pending"
    if state == "SUCCESS":
        return "passed"
    return "skipped"


def colored(text: str, color: str) -> str:
    return f"{color}{text}{RESET}"


def author_login(pr: dict) -> str:
    return (pr.get("author") or {}).get("login", "ghost")


def nodes(pr: dict, field: str) -> list:
    return [node for node in (pr.get(field) or {}).get("nodes") or [] if node]


def unresolved_threads(pr: dict) -> int:
    return sum(not thread["isResolved"] for thread in nodes(pr, "reviewThreads"))


def size(change: dict) -> str:
    added = colored("+{}".format(change["additions"]), GREEN)
    deleted = colored("-{}".format(change["deletions"]), RED)
    return f"{added} {deleted}"


def build_item(pr: dict, show_repo: bool) -> dict:
    url = pr["url"]
    repo = pr["repository"]["nameWithOwner"]
    suffix = repo if show_repo else author_login(pr)
    status = "Draft" if pr["isDraft"] else "Open"
    commits = pr["commits"]["nodes"]
    rollup = commits[0]["commit"]["statusCheckRollup"] if commits else None
    if rollup:
        icon, color, label = CI_STYLES[ci_group(rollup["state"])]
        status = f"{icon} {colored(f'CI: {label}', color)} \u00b7 {status}"
    details = [status]
    if review := REVIEW.get(pr.get("reviewDecision")):
        details.append(colored(review, REVIEW_COLORS[pr["reviewDecision"]]))
    if pr.get("mergeable") == "CONFLICTING":
        details.append(colored("Conflicts", RED))
    if threads := unresolved_threads(pr):
        details.append(f"\U0001f4ac {threads} unresolved")
    if "additions" in pr:
        details.append(size(pr))
    if updated := pr.get("updatedAt"):
        details.append(colored(f"updated {ago(updated)}", GREY))
    return {
        "title": f"PR #{pr['number']}: {pr['title']} - {suffix}",
        "subtitle": " \u00b7 ".join(details),
        "arg": url,
        "variables": {
            "PR_URL": url,
            "PR_REPO": repo,
            "PR_NUMBER": str(pr["number"]),
            "PR_BRANCH": pr.get("headRefName", ""),
        },
    }


def avatar_path(owner: str) -> Path:
    return AVATAR_DIR / f"{owner}.svg"


def account_icon(account: dict | None) -> str | None:
    """Path of a GitHub account's avatar, downloaded on first use."""
    account = account or {}
    if not account.get("avatarUrl"):
        return None
    path = avatar_path(account["login"])
    if path.exists():
        return str(path)
    try:
        with urllib.request.urlopen(
            account["avatarUrl"], timeout=AVATAR_TIMEOUT
        ) as response:
            mime = response.headers.get_content_type()
            data = base64.b64encode(response.read()).decode()
    except OSError:
        return None
    if not mime.startswith("image/"):
        return None
    path.parent.mkdir(parents=True, exist_ok=True)
    partial = path.with_suffix(".part")
    partial.write_text(AVATAR_SVG.format(mime=mime, data=data))
    partial.replace(path)
    return str(path)


def link_item(title: str, url: str) -> dict:
    return {
        "title": title,
        "subtitle": url,
        "arg": url,
        "preview": {"title": title, "text": f"Opens {url}", "dynamic": False},
    }


def error_item(title: str, message: str) -> dict:
    return {
        "title": title,
        "subtitle": message,
        "preview": {"title": title, "text": message, "dynamic": False},
    }


def list_items(query: str, repo_arg: str | None = None) -> list[dict]:
    target, needle = parse_query(query, repo_arg)
    if target not in MODES and not REPO.fullmatch(target):
        return [
            error_item("Invalid repository", f"Expected owner/repo, got {target!r}")
        ]

    try:
        if target in MODES:
            search, title, page = MODES[target]
            data = graphql(SEARCH_QUERY, q=f"is:pr is:open {search}")
            pulls = data["search"]["nodes"]
            items = [link_item(title, f"https://{API_HOST}/{page}")]
        else:
            owner, name = target.split("/")
            data = graphql(REPO_QUERY, owner=owner, name=name)
            pulls = data["repository"]["pullRequests"]["nodes"]
            url = f"https://{API_HOST}/{target}/pulls"
            items = [link_item("Open all pull requests page", url)]
    except GhError as error:
        return [error_item("GitHub error", str(error))]

    needle = needle.casefold()
    matches: list[tuple[dict, dict]] = []
    for pr in filter(None, pulls):
        item = build_item(pr, show_repo=target in MODES)
        text = f"{item['title']} {item['subtitle']} {author_login(pr)}"
        if needle in ANSI.sub("", text).casefold():
            matches.append((pr, item))

    # Within one repository every owner avatar is the same, so show who
    # opened each pull request instead.
    single_repo = len({pr["repository"]["nameWithOwner"] for pr, _ in matches}) == 1
    for pr, item in matches:
        account = pr.get("author") if single_repo else pr["repository"].get("owner")
        if icon := account_icon(account):
            item["icon"] = {"path": icon}

    if target not in MODES:
        items.extend(item for _, item in matches)
        return items

    groups: dict[str, list[dict]] = {}
    for pr, item in matches:
        owner = pr["repository"]["nameWithOwner"].split("/", 1)[0]
        groups.setdefault(owner, []).append(item)

    if len(groups) < 2:
        items.extend(item for _, item in matches)
        return items

    for owner, owner_items in groups.items():
        items.append({"title": owner, "indent": 0, "valid": False})
        for item in owner_items:
            item["indent"] = 1
            items.append(item)
    return items


def check_name(check: dict) -> str:
    name = check.get("name") or check.get("context") or "?"
    run = ((check.get("checkSuite") or {}).get("workflowRun")) or {}
    if workflow := (run.get("workflow") or {}).get("name"):
        return f"{workflow} / {name}"
    return name


def check_state(check: dict) -> str:
    if check.get("__typename") == "StatusContext":
        return check.get("state", "")
    if check.get("status") != "COMPLETED":
        return "PENDING"
    return check.get("conclusion", "")


def checks_section(checks: list[dict]) -> dict:
    order = list(CI_STYLES)
    rows = sorted(
        ((ci_group(check_state(check)), check_name(check)) for check in checks),
        key=lambda row: (order.index(row[0]), row[1]),
    )
    counts = Counter(group for group, _ in rows)
    summary = ", ".join(f"{counts[group]} {group}" for group in order if counts[group])
    lines = []
    for group, name in rows:
        icon, color, _ = CI_STYLES[group]
        lines.append(f"{colored(icon, color)} {name}")
    return {
        "type": "text",
        "heading": f"Checks: {summary}",
        "content": "\n".join(lines),
    }


def ago(timestamp: str) -> str:
    seconds = (datetime.now(UTC) - datetime.fromisoformat(timestamp)).total_seconds()
    for unit, length in (
        ("y", 31536000),
        ("mo", 2592000),
        ("d", 86400),
        ("h", 3600),
        ("m", 60),
    ):
        if seconds >= length:
            return f"{int(seconds // length)}{unit} ago"
    return "just now"


def pr_state(pr: dict) -> str:
    if pr["state"] == "OPEN" and pr["isDraft"]:
        return "Draft"
    return pr["state"].capitalize()


def sgr(text: str, on: str, off: str) -> str:
    return f"\033[{on}m{text}\033[{off}m"


def render_text(text: str) -> str:
    text = HTML_TAG.sub("", text)
    text = IMAGE.sub(lambda m: sgr(f"[image: {m[1] or 'image'}]", "90", "39"), text)
    text = LINK.sub(lambda m: sgr(m[1], "4;34", "24;39"), text)
    text = STRONG.sub(lambda m: sgr(m[2], "1", "22"), text)
    # Raffi has no italics, so emphasis only loses its markers.
    return EMPHASIS.sub(r"\2", text)


def render_inline(line: str) -> str:
    """Render inline markdown, leaving code spans untouched."""
    parts, pos = [], 0
    for match in CODE_SPAN.finditer(line):
        parts.append(render_text(line[pos : match.start()]))
        parts.append(sgr(match[2], "33", "39"))
        pos = match.end()
    parts.append(render_text(line[pos:]))
    return "".join(parts)


def render_markdown(text: str) -> str:
    lines, in_code = [], False
    for line in text.splitlines():
        if FENCE.match(line):
            in_code = not in_code
        elif in_code:
            lines.append(sgr(f"  {line}", "33", "39"))
        elif match := HEADING.match(line):
            lines.append(sgr(render_inline(match[1]), "1;35", "0"))
        elif RULE.match(line):
            lines.append(sgr("\u2500" * 40, "90", "39"))
        elif TABLE_RULE.match(line):
            continue
        elif match := LIST_ITEM.match(line):
            indent, box, item = match.groups()
            if box is None:
                marker = "\u2022"
            elif box == " ":
                marker = "\u2610"
            else:
                marker = sgr("\u2611", "32", "39")
            lines.append(f"{indent}{marker} {render_inline(item)}")
        elif match := QUOTE.match(line):
            quote = match[1]
            if alert := ALERT.match(quote):
                quote = sgr(alert[1].capitalize(), "1;33", "0")
            else:
                quote = render_inline(quote)
            lines.append(sgr("\u2502 ", "90", "39") + quote)
        else:
            lines.append(render_inline(line))
    return "\n".join(line.rstrip() for line in lines)


def clean_body(body: str) -> str:
    body = HTML_COMMENT.sub("", body.replace("\r\n", "\n"))
    return BLANK_LINES.sub("\n\n", render_markdown(body)).strip()


def is_bot(author: dict | None) -> bool:
    author = author or {}
    login = author.get("login", "")
    return author.get("__typename") == "Bot" or login.endswith(BOT_SUFFIXES)


def reviewers(pr: dict) -> str:
    """Each reviewer's latest verdict, then the reviews still requested."""
    verdicts = []
    for review in nodes(pr, "latestReviews"):
        if mark := REVIEWER_MARKS.get(review["state"]):
            verdicts.append(f"{mark} {author_login(review)}")
    for request in nodes(pr, "reviewRequests"):
        reviewer = request.get("requestedReviewer") or {}
        if who := reviewer.get("login") or reviewer.get("name"):
            verdicts.append(f"\u23f3 {who}")
    return ", ".join(verdicts)


def merge_state(pr: dict) -> str:
    if pr["state"] == "MERGED":
        return f"Merged {ago(pr['mergedAt'])}" if pr.get("mergedAt") else "Merged"
    if pr["state"] != "OPEN":
        return ""
    if pr.get("mergeable") == "CONFLICTING":
        state = "Conflicts"
    else:
        state = MERGE_STATE.get(pr.get("mergeStateStatus"), "")
    if auto := pr.get("autoMergeRequest"):
        by = (auto.get("enabledBy") or {}).get("login")
        method = (auto.get("mergeMethod") or "").lower()
        state += f", auto-merge ({method})" + (f" by {by}" if by else "")
    return state.strip(", ")


def closes_section(pr: dict) -> dict | None:
    issues = nodes(pr, "closingIssuesReferences")
    if not issues:
        return None
    repo = pr["repository"]["nameWithOwner"]
    lines = []
    for issue in issues:
        state = issue["state"].lower()
        color = GREEN if state == "open" else "\033[35m"
        where = issue["repository"]["nameWithOwner"]
        ref = f"#{issue['number']}" if where == repo else f"{where}#{issue['number']}"
        lines.append(
            f"{sgr(ref, '1', '22')} {colored(f'[{state}]', color)} {issue['title']}"
        )
    return {"type": "text", "heading": "Closes", "content": "\n".join(lines)}


def comment_sections(pr: dict) -> list[dict]:
    comments = [c for c in nodes(pr, "comments") if not is_bot(c.get("author"))]
    return [
        {
            "type": "text",
            "heading": f"\U0001f4ac {author_login(comment)} \u00b7 {ago(comment['createdAt'])}",
            "content": clean_body(comment["body"]) or "(empty)",
        }
        for comment in reversed(comments[-PREVIEW_COMMENTS:])
    ]


def files_section(pr: dict) -> dict | None:
    files = nodes(pr, "files")
    if not files:
        return None
    lines = []
    for file in files:
        letter, color = CHANGE_STYLES.get(file["changeType"], ("M", YELLOW))
        lines.append(f"{colored(letter, color)} {file['path']} {size(file)}")
    total = (pr.get("files") or {}).get("totalCount", len(files))
    if total > len(files):
        lines.append(colored(f"\u2026 and {total - len(files)} more", GREY))
    return {"type": "text", "heading": f"Files ({total})", "content": "\n".join(lines)}


def build_preview(pr: dict) -> dict:
    repo = pr["repository"]["nameWithOwner"]
    author = author_login(pr)
    metadata = [
        ["Repository", repo],
        ["Author", author],
        ["Branch", f"{pr['headRefName']} \u2192 {pr['baseRefName']}"],
    ]
    if review := REVIEW.get(pr["reviewDecision"]):
        metadata.append(["Review", review])
    if who := reviewers(pr):
        metadata.append(["Reviewers", who])
    if merge := merge_state(pr):
        metadata.append(["Merge", merge])
    if threads := unresolved_threads(pr):
        metadata.append(["Threads", f"{threads} unresolved"])
    commit_nodes = nodes(pr, "commits")
    if commit_nodes:
        total = pr["commits"]["totalCount"]
        headline = commit_nodes[0]["commit"]["messageHeadline"]
        label = f"{total} commit{'s' * (total != 1)}"
        metadata.append(["Commits", f"{label}, last: {headline}"])
    if assignees := ", ".join(a["login"] for a in nodes(pr, "assignees")):
        metadata.append(["Assignees", assignees])
    if milestone := (pr.get("milestone") or {}).get("title"):
        metadata.append(["Milestone", milestone])
    if labels := ", ".join(label["name"] for label in nodes(pr, "labels")):
        metadata.append(["Labels", labels])
    files = pr["changedFiles"]
    changes = (
        f"+{pr['additions']} -{pr['deletions']} in {files} file{'s' * (files != 1)}"
    )
    metadata += [
        ["Changes", changes],
        ["Created", ago(pr["createdAt"])],
        ["Updated", ago(pr["updatedAt"])],
    ]

    rollup = commit_nodes[0]["commit"]["statusCheckRollup"] if commit_nodes else None
    checks = nodes(rollup or {}, "contexts")
    sections = [checks_section(checks)] if checks else []
    if closes := closes_section(pr):
        sections.append(closes)
    sections.append(
        {
            "type": "text",
            "heading": "Description",
            "content": clean_body(pr["body"]) or "(no description)",
        }
    )
    sections += comment_sections(pr)
    if changed := files_section(pr):
        sections.append(changed)

    avatar = avatar_path(repo.split("/")[0])
    preview = {
        "title": f"#{pr['number']} {pr['title']}",
        "subtitle": f"{repo} by {author}",
        "badge": pr_state(pr),
        "icon": str(avatar) if avatar.exists() else ICON,
        "sections": sections,
        "metadata": metadata,
    }
    if any(ci_group(check_state(check)) == "pending" for check in checks):
        preview["refresh"] = PREVIEW_REFRESH_SECONDS
    return preview


def preview(url: str) -> dict:
    match = PR_URL.match(url)
    if not match:
        return {
            "title": url or "Pull request",
            "icon": ICON,
            "text": "Not a pull request URL.",
        }
    owner, name, number = match.groups()
    try:
        data = graphql(
            PREVIEW_QUERY,
            cache=None,
            timeout=PREVIEW_TIMEOUT,
            owner=owner,
            name=name,
            number=int(number),
            files=PREVIEW_FILES,
        )
        pr = (data.get("repository") or {}).get("pullRequest")
        if not pr:
            raise GhError("pull request not found")
    except GhError as error:
        return {
            "title": url,
            "icon": ICON,
            "text": f"Cannot load this pull request: {error}",
        }
    return build_preview(pr)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--repo", help="owner/repo or alias to list; the query only filters"
    )
    parser.add_argument("query", nargs="*", help="text typed after the Raffi keyword")
    args = parser.parse_args(argv)

    if os.environ.get("RAFFI_PREVIEW") == "1":
        output = preview(os.environ.get("RAFFI_VALUE", ""))
    else:
        output = {"items": list_items(" ".join(args.query), args.repo)}
    print(json.dumps(output, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
