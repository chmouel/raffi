#!/usr/bin/env python3
"""Raffi script filter for GitHub pull requests.

Lists the open pull requests of a repository, or your own open pull requests
everywhere. When Raffi sets RAFFI_PREVIEW=1, it prints a preview of the pull
request whose URL is in RAFFI_VALUE instead.
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
ICON = "github"
# Raffi gives the preview command 3 seconds in total.
PREVIEW_TIMEOUT = 2.5
PREVIEW_REFRESH_SECONDS = 20

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

PR_FRAGMENT = """
fragment pr on PullRequest {
  number title url isDraft
  repository { nameWithOwner owner { login avatarUrl(size: 128) } }
  author { login }
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
MINE_QUERY = (
    """
query {
  search(query: "is:pr is:open author:@me", type: ISSUE, first: 50) {
    nodes { ...pr }
  }
}
"""
    + PR_FRAGMENT
)

PREVIEW_FIELDS = (
    "number,title,url,state,isDraft,author,headRefName,baseRefName,reviewDecision,"
    "labels,additions,deletions,changedFiles,createdAt,updatedAt,body,statusCheckRollup"
)


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


def graphql(query: str, **variables: str) -> dict:
    args = ["api", "graphql", "--hostname", API_HOST, "--cache", CACHE_PULLS]
    for name, value in {"query": query, **variables}.items():
        args += ["-f", f"{name}={value}"]
    response = run_gh(*args)
    if errors := response.get("errors"):
        raise GhError(errors[0].get("message", "GraphQL error"))
    return response["data"]


def parse_query(query: str, repo: str | None = None) -> tuple[str | None, str]:
    """Split the typed text into (repository, or None for your own PRs, filter)."""
    query = query.strip()
    if repo:
        return os.environ.get(f"ALIAS_{repo}", repo), query
    word, _, rest = query.partition(" ")
    rest = rest.strip()
    if word == "myprs":
        return None, rest
    if alias := os.environ.get(f"ALIAS_{word}"):
        return alias, rest
    if "/" in word:
        return word, rest
    return None, query


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


def build_item(pr: dict, show_repo: bool) -> dict:
    url = pr["url"]
    suffix = pr["repository"]["nameWithOwner"] if show_repo else author_login(pr)
    status = "Draft" if pr["isDraft"] else "Open"
    commits = pr["commits"]["nodes"]
    rollup = commits[0]["commit"]["statusCheckRollup"] if commits else None
    if rollup:
        icon, color, label = CI_STYLES[ci_group(rollup["state"])]
        status = f"{icon} {colored(f'CI: {label}', color)} - {status}"
    return {
        "title": f"PR #{pr['number']}: {pr['title']} - {suffix}",
        "subtitle": f"{status} \u2014 {url}",
        "arg": url,
    }


def avatar_path(owner: str) -> Path:
    return AVATAR_DIR / f"{owner}.svg"


def owner_icon(repository: dict) -> str | None:
    """Path of the repository owner's avatar, downloaded on first use."""
    owner = repository.get("owner") or {}
    if not owner.get("avatarUrl"):
        return None
    path = avatar_path(owner["login"])
    if path.exists():
        return str(path)
    try:
        with urllib.request.urlopen(
            owner["avatarUrl"], timeout=AVATAR_TIMEOUT
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
    repo, needle = parse_query(query, repo_arg)
    if repo is not None and not REPO.fullmatch(repo):
        return [error_item("Invalid repository", f"Expected owner/repo, got {repo!r}")]

    try:
        if repo:
            owner, name = repo.split("/")
            data = graphql(REPO_QUERY, owner=owner, name=name)
            pulls = data["repository"]["pullRequests"]["nodes"]
            url = f"https://{API_HOST}/{repo}/pulls"
            items = [link_item("Open all pull requests page", url)]
        else:
            pulls = graphql(MINE_QUERY)["search"]["nodes"]
            url = f"https://{API_HOST}/pulls"
            items = [link_item("Open your pull requests dashboard", url)]
    except GhError as error:
        return [error_item("GitHub error", str(error))]

    needle = needle.casefold()
    for pr in filter(None, pulls):
        item = build_item(pr, show_repo=repo is None)
        text = f"{item['title']} {item['subtitle']} {author_login(pr)}"
        if needle in ANSI.sub("", text).casefold():
            if icon := owner_icon(pr["repository"]):
                item["icon"] = {"path": icon}
            items.append(item)
    return items


def check_name(check: dict) -> str:
    name = check.get("name") or check.get("context") or "?"
    if workflow := check.get("workflowName"):
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
    for unit, size in (("d", 86400), ("h", 3600), ("m", 60)):
        if seconds >= size:
            return f"{int(seconds // size)}{unit} ago"
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


def build_preview(pr: dict) -> dict:
    repo = "/".join(pr["url"].split("/")[3:5])
    author = author_login(pr)
    metadata = [
        ["Repository", repo],
        ["Author", author],
        ["Branch", f"{pr['headRefName']} \u2192 {pr['baseRefName']}"],
    ]
    if review := REVIEW.get(pr["reviewDecision"]):
        metadata.append(["Review", review])
    if labels := ", ".join(label["name"] for label in pr["labels"]):
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

    checks = pr["statusCheckRollup"]
    sections = [checks_section(checks)] if checks else []
    sections.append(
        {
            "type": "text",
            "heading": "Description",
            "content": clean_body(pr["body"]) or "(no description)",
        }
    )

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
    try:
        pr = run_gh(
            "pr", "view", url, "--json", PREVIEW_FIELDS, timeout=PREVIEW_TIMEOUT
        )
    except GhError as error:
        return {
            "title": url or "Pull request",
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
