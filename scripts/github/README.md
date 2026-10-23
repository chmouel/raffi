# GitHub Pull Requests Script Filter

This script lets Raffi browse open GitHub pull requests: your own, the ones
waiting for your review, assigned to you or involving you, or the ones of a
given repository. Each row shows CI status, review decision, merge conflicts,
unresolved review threads, size and last update. The preview pane shows the
full picture: reviewers, merge state, checks, linked issues, description,
latest comments and changed files.

## Requirements

- Raffi native mode
- Python 3.11 or newer
- The [GitHub CLI](https://cli.github.com), logged in with `gh auth login`

## Install

```bash
mkdir -p ~/.config/raffi/scripts/github
cp scripts/github/pr.py ~/.config/raffi/scripts/github/pr.py
chmod +x ~/.config/raffi/scripts/github/pr.py
```

## Configure

Add this entry under `addons.script_filters` in `raffi.yaml`:

```yaml
addons:
  script_filters:
    - name: "Pull Requests"
      keyword: "pr"
      command: "~/.config/raffi/scripts/github/pr.py"
      icon: "github"
      action: "xdg-open {value}"
      secondary_action: "copy"
      cache_seconds: 300
      warm_queries: [""]
```

`cache_seconds` shows the last result for a query immediately and refreshes
it in the background. `warm_queries` are warmed periodically by
`raffi --cache-daemon`, in every view, so those queries are already fresh
when Raffi opens.
Set `warm_interval_seconds` to change the warming period (default:
`cache_seconds`). Without `cache_seconds` nothing is cached.

To give a repository its own keyword, pin it with `--repo`. Everything typed
after the keyword then filters that repository's pull requests:

```yaml
    - name: "Linus Pull Requests"
      keyword: "linoux"
      command: "~/.config/raffi/scripts/github/pr.py"
      args: ["--repo", "linus/linux"]
      icon: "github"
      action: "xdg-open {value}"
      secondary_action: "copy"
```

## Usage

```text
pr                         your open pull requests
pr raffi                   your open pull requests matching "raffi"
pr review                  pull requests waiting for your review
pr assigned                pull requests assigned to you
pr involved                pull requests you authored, review, were mentioned in
                           or commented on
pr owner/repo              open pull requests of owner/repo
pr owner/repo login fix    the same, filtered by "login fix"
pr myprs failed            your open pull requests with failing CI
pr review approved         review requests already approved by someone
```

The filter matches everything shown on the row: number, title, repository or
author, state, CI status, review decision, "Conflicts", unresolved threads and
size. The first row opens the matching pull request list on GitHub. Enter
opens the selected pull request, Ctrl+Enter copies its URL.

Without a repository, Ctrl+Tab switches between your pull requests, review
requests, assigned and involved, and Ctrl+Shift+Tab goes back, keeping what
you typed as the filter. A mode word you type yourself, such as `pr review`,
takes precedence over the view.

Cross-repository modes group results under non-actionable repository-owner
headings when the filtered results span multiple owners. Pull requests are
indented beneath each heading; single-owner results and `owner/repo` queries
stay flat.

Each row also passes these variables to `action` and `secondary_action`:
`PR_URL`, `PR_REPO` (owner/repo), `PR_NUMBER` and `PR_BRANCH`. For example,
to copy the branch name with Ctrl+Enter:

```yaml
      secondary_action: 'echo -n "$PR_BRANCH" | wl-copy'
```

Each pull request shows the avatar of the repository owner, or of its author
when every listed pull request comes from the same repository. Avatars are
downloaded once into `~/.cache/raffi/github-avatars/` (or under
`$XDG_CACHE_HOME`); delete that directory to refresh them. When an avatar
cannot be downloaded, the entry's icon is used instead.

## Aliases

Set `ALIAS_<name>` in the entry's `env` to use a short name for a repository,
as the first word of the query or as the `--repo` value:

```yaml
      env:
        ALIAS_linoux: "linus/linux"
```

`pr linoux` then lists the pull requests of `linus/linux`.

## Settings

These environment variables can also be set in the entry's `env`:

| Variable | Default | Description |
|----------|---------|-------------|
| `API_HOST` | `github.com` | GitHub host, for GitHub Enterprise |
| `CACHE_PULLS` | `10m` | How long `gh` caches the pull request list |

Raffi sets `RAFFI_REFRESH=1` when you press Ctrl+R or when the cache daemon
refreshes a filter. The script then skips `gh api --cache` so it reads live
GitHub state instead of the `gh` CLI's own cache.

## Live preview

With the preview pane open (`Ctrl+P`), the selected pull request shows:

- its author, branches, review decision and each reviewer's verdict
  (✓ approved, ✗ changes requested, 💬 commented, ⏳ still requested)
- its merge state: ready, blocked, behind, conflicts, auto-merge, or when it
  was merged
- unresolved review threads, commit count and last commit, assignees,
  milestone, labels, size and dates
- its checks, failed ones first
- the issues it closes
- its description, with the markdown rendered in colour
- the last three comments from people, newest first (bot comments are hidden)
- the changed files with their size, up to 50

The preview makes a single GraphQL request, so it usually shows within a
second. While a check is still running, it refreshes every 20 seconds.

Raffi asks for the preview by running the script again with `RAFFI_PREVIEW=1`
and the pull request URL in `RAFFI_VALUE`. To try it by hand:

```bash
RAFFI_PREVIEW=1 RAFFI_VALUE=https://github.com/chmouel/raffi/pull/1 \
  ~/.config/raffi/scripts/github/pr.py
```

Add `dynamic_preview: false` to the entry to turn this off.

## Coming from alfred-pr-workflow

This script replaces `pr.py` from
[alfred-pr-workflow](https://gitlab.com/chmouel/alfred-pr-workflow). Colours
are always on, so remove `--color` from `args`.

## Tests

```bash
cd scripts/github && python3 -m unittest test_pr
```
