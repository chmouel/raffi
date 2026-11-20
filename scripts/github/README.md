# GitHub Script Filters

Two scripts browse GitHub from Raffi, with the
[GitHub CLI](https://cli.github.com) logged in with `gh auth login`. They share
`ghcommon.py`, which runs `gh`, sends GraphQL requests through its cache and
builds error rows, so it must be installed next to whichever one you use.
`pr.py` lists pull requests, with their CI status, review decision,
conflicts, unresolved review threads, size and last update, and a preview
pane showing reviewers, merge state, checks, linked issues, description,
latest comments and changed files. `repos.py` lists the repositories of an
account, yours by default, or of the one you name.

## Pull requests

### Requirements

- Raffi native mode
- Python 3.11 or newer
- The [GitHub CLI](https://cli.github.com), logged in with `gh auth login`

### Install

```bash
mkdir -p ~/.config/raffi/scripts/github
cp scripts/github/pr.py scripts/github/ghcommon.py ~/.config/raffi/scripts/github/
chmod +x ~/.config/raffi/scripts/github/pr.py
```

Keep `ghcommon.py` in the same directory as the script. A symlink to the
checked-out script works too, as Python looks for the module beside the real
file. A script copied alone fails with `No module named 'ghcommon'`, which
Raffi shows as an empty list; run the script in a terminal to see the error.

### Configure

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
`raffi --server`, in every view, so those queries are already fresh
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

### Usage

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

### Aliases

Set `ALIAS_<name>` in the entry's `env` to use a short name for a repository,
as the first word of the query or as the `--repo` value:

```yaml
      env:
        ALIAS_linoux: "linus/linux"
```

`pr linoux` then lists the pull requests of `linus/linux`.

### Settings

These environment variables can also be set in the entry's `env`:

| Variable | Default | Description |
|----------|---------|-------------|
| `API_HOST` | `github.com` | GitHub host, for GitHub Enterprise |
| `CACHE_PULLS` | `10m` | How long `gh` caches the pull request list |
| `TYPED_TIME` | | Set to `1` to send `updated` and `pushed` as a `time` field that Raffi renders ("3h ago") instead of text that freezes in its cache. Needs a Raffi that knows the field |

Raffi sets `RAFFI_REFRESH=1` when you press Ctrl+R or when the server
refreshes a filter. The script then skips `gh api --cache` so it reads live
GitHub state instead of the `gh` CLI's own cache.

### Live preview

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
RAFFI_PREVIEW=1 RAFFI_VALUE=https://github.com/owner/repo/pull/1 \
  ~/.config/raffi/scripts/github/pr.py
```

Add `dynamic_preview: false` to the entry to turn this off.

### Coming from alfred-pr-workflow

This script replaces `pr.py` from
[alfred-pr-workflow](https://gitlab.com/chmouel/alfred-pr-workflow). Colours
are always on, so remove `--color` from `args`.

## Repositories

`repos.py`, in the same directory, browses the repositories of a GitHub
account: yours by default, or the one you name. It needs the same
[GitHub CLI](https://cli.github.com) setup as `pr.py`.

### Install

```bash
mkdir -p ~/.config/raffi/scripts/github
cp scripts/github/repos.py scripts/github/ghcommon.py ~/.config/raffi/scripts/github/
chmod +x ~/.config/raffi/scripts/github/repos.py
```

### Configure

```yaml
addons:
  script_filters:
    - name: "Repositories"
      keyword: "repo"
      command: "~/.config/raffi/scripts/github/repos.py"
      args: ["--owners", "me,a,labs"]
      env:
        ALIAS_a: acme
        ALIAS_labs: acme-labs
      icon: "github"
      action: "xdg-open {value}"
      secondary_action: "copy"
      cache_seconds: 300
      warm_queries: [""]
```

`--owners` takes a comma separated list of accounts or aliases. `me` is the
account `gh` is logged in as, and every other alias is an `ALIAS_<name>`
variable in the filter's `env`, as `a` and `labs` are above. Each one is a
Raffi view, so Ctrl+Tab switches between them and keeps what you typed. With
more than one account, the default view is All: the repositories of every
account mixed together by last push, each row starting with its owner. Without
`--owners` the only account is `me`.

```text
repo                      every account's repositories, most recently pushed first
repo cli                  the same, filtered by "cli"
repo a                    the repositories of the acme organization
repo acme api             the same, filtered by "api"
repo owner/repo           opens that repository
```

The first word is an account from `--owners` or an alias. Any other word
filters the current list, so a typo never lists a stranger's repositories.
Pass `--any-owner`, or set `ANY_OWNER=1`, to let the first word be any GitHub
account, as in `repo some-org`. A word that names one of the listed
repositories, or the listed account itself, filters instead, and `me word`
filters your repositories whatever the word is. The remaining words must all
appear in a repository's description, topics, name, language or flags, which
are shown with its stars and last push. Names answer to their initials too, so
`a gs` finds `go-server`, and name matches are listed before the rest.
Forks come after every other repository, in all views.
A filter that matches nothing says so instead of showing an empty list.

Enter opens the selected repository and Ctrl+Enter copies its URL. The last
row opens the account itself, and comes first instead when nothing matched; it
exports `REPO_URL` and `REPO_OWNER`.

The output asks Raffi to list the repositories you picked first, with a
top-level `"sort": "usage"`, since you go back to the same few again and again.
The rows you never picked keep the order the script prints. Pass
`--no-usage-sort`, or set `USAGE_SORT=0` in the filter's `env`, to keep that
order instead, and `sort_by_usage: false` in `raffi.yaml` does the same from
the config side.

Repository rows also export `REPO_NAME` (owner/name), `REPO_SSH` and
`REPO_CLONE`, so a clone action can be:

```yaml
      secondary_action: 'test -n "$REPO_SSH" && cd ~/git && git clone "$REPO_SSH"'
```

Accounts already looked up as missing are remembered for a day in
`~/.cache/raffi/github-repos/<host>/`, so filtering does not ask GitHub again
for words that are not accounts. The entry's `dynamic_preview` can stay `false`:
each row carries its own preview, with the description, stars, forks,
visibility and topics.

### Pages and action menus

A row opens its repository. Two options point rows at something else.

`--path` makes every row open a page of the repository, so one keyword per
page gives single-key access:

```yaml
    - name: "Repository pull requests"
      keyword: "rpr"
      command: "~/.config/raffi/scripts/github/repos.py"
      args: ["--owners", "me,a,labs", "--path", "pulls"]
      icon: "github"
      action: "xdg-open {value}"
      secondary_action: "copy"
```

`--pages` makes Enter list pages under each repository: the repository
itself first, then the pages you name, with Enter opening one and Ctrl+Enter
copying its URL. Escape goes back to the list.

```yaml
      args: ["--owners", "me,a,labs", "--pages", "pulls,issues,releases,actions"]
```

`--actions` puts pages in the row's action menu instead, opened with Right at
the end of the query or a right-click, so Enter keeps opening the repository,
unless a `pulls` or `issues` page is named: those are live menus (below), so
naming one gives the row children too, as `--pages` would. Pass `--no-live` to
keep the plain links and Enter opening the repository.
The two options combine. Both need a Raffi that supports `actions` on script
filter items for the menu, and `--pages` works on older ones.

Every row also has copy entries in its action menu: the URL, a Markdown link,
the SSH and HTTPS clone URLs and `owner/name` for repositories, and the URL and
a Markdown link for each page listed by `--pages`.

#### Live pull requests and issues

`pulls` and `issues` are live pages: rather than a link, each becomes a menu
whose children are the repository's open pull requests, or open issues, newest
updated first, with the `All … on GitHub` row last. Its title carries the
total count (`Pull requests · 34`) and its subtitle names the repository.
Enter on a row opens that pull request or issue and Ctrl+Enter copies its URL,
and the last row, `All pull requests on GitHub` or `All issues on GitHub`,
opens the page itself. A list whose settled cache holds no row is not a menu
at all: it shows the plain link to that page that `--no-live` draws.

The `"sort": "usage"` below still applies: Raffi floats the rows you picked
first, inside a menu too, so the newest-updated order above holds between the
rows you never picked. Pass `--no-usage-sort`, or `USAGE_SORT=0`, to keep the
emitted order throughout.

A pull request row shows its number and title, its author, whether it is a
draft, its review decision (`approved`, `changes requested`, `review required`)
and when it was last updated. It exports `PR_URL`, `PR_REPO`, `PR_NUMBER` and
`PR_BRANCH` on top of the repository's `REPO_*` variables, so actions written
for `pr.py` work on it. An issue row shows its author and its first three
labels, and exports `ISSUE_URL`, `ISSUE_REPO` and `ISSUE_NUMBER`.

`Pull requests` and `Issues` set `children_lazy: true`, so pressing Enter
runs `repos.py` with `RAFFI_CHILDREN=1` to fetch that repository's open pull
requests or issues on demand. Raffi caches the expanded list in memory for
`CACHE_LIVE` (default `10m`), and `gh` caches the GraphQL response for the
same window. Pressing Ctrl+R inside the expanded list sends `RAFFI_REFRESH=1`
to bypass both caches and fetch fresh rows from GitHub. On a GitHub error, the
menu shows the error row followed by the `All … on GitHub` link so it is never
a dead end.

`--live-items` (default 20) caps how many rows a live menu asks GitHub for.

Pass `--no-live`, or set `LIVE=0`, to keep linking the two pages as before.

The filter's own `actions` in `raffi.yaml` add entries a script cannot know,
such as cloning with the row's `$REPO_SSH`:

```yaml
      actions:
        - title: "Clone"
          action: 'git clone "$REPO_SSH" ~/src/"$REPO_NAME"'
```

Pages are written by path: `pulls`, `issues`, `releases`, `actions`, `tags`,
`branches`, `commits`, `discussions`, `wiki`, `security`, `settings`, `pulse`,
`graphs/traffic`, `graphs/contributors`, or any other. `path:Label` sets the
label, and the row subtitle shows the page `--path` points at. The options
also read `REPO_PATH`, `REPO_PAGES` and `REPO_ACTIONS` from `env`.

### Settings

| Variable | Default | Description |
|----------|---------|-------------|
| `API_HOST` | `github.com` | GitHub host, for GitHub Enterprise |
| `CACHE_REPOS` | `10m` | How long `gh` caches the repository list |
| `CACHE_LIVE` | `10m` | How long the live pull requests and issues are cached |
| `LIVE_ITEMS` | `20` | How many rows a live menu asks GitHub for; `--live-items` sets this |
| `LIVE` | | `0` is the same as `--no-live` |
| `TYPED_TIME` | | Same as for pull requests, for the `pushed` time |
| `REPO_PATH` | | Default for `--path` |
| `REPO_PAGES` | | Default for `--pages` |
| `REPO_ACTIONS` | | Default for `--actions` |
| `CACHE_LOGIN` | `1h` | How long `gh` caches the logged in account |
| `ANY_OWNER` | | `1` is the same as `--any-owner` |
| `USAGE_SORT` | | `0` is the same as `--no-usage-sort` |
| `CACHE_MISSING` | `24h` | How long a word is remembered as not being an account, with `--any-owner` |
| `GITHUB_USER` | | Skip the lookup of the logged in account |

`--limit` (default 500) caps how many repositories are listed per account.

### Caching

Four layers keep typing cheap:

- Raffi caches each query and view's result for `cache_seconds` and shows it
  at once while refreshing it in the background; `raffi --server` warms
  `warm_queries` in every view.
- `gh api --cache` holds each account's pages, and the logged in account is
  kept in `login.json` beside the misses, so answering a keystroke starts no
  request: about 100 ms for `repo acme` in the warmed case.
- Typed filters never reach GitHub. The listed account is fetched once per
  cache window and filtered inside the script, so adding a letter costs a
  process start, not a request.
- Listing another account asks GitHub about that account only, plus one
  one-repository check when the word is not one of the `--owners`.

Both scripts tell Raffi how long their output stays fresh, using the `gh`
cache window (`CACHE_REPOS`, `CACHE_PULLS`), and Raffi applies it up to your
`cache_max_seconds`. For repositories, which change slowly, a day is a
reasonable window:

```yaml
      env:
        CACHE_REPOS: 24h
      cache_seconds: 300
      cache_max_seconds: 86400
      warm_queries: [""]
```

Without `cache_max_seconds` the hint is clamped to `cache_seconds`, which is
the old behaviour. The server refreshes a list once its result has outlived
that window, so `warm_interval_seconds` is only a minimum gap and can be left
out: the 24h above means one background refresh a day per list, not one every
five minutes. A failed refresh is retried after a minute and then at doubling
delays, while the cached list keeps being served.

When `gh` fails, the scripts ask Raffi not to keep the error row
(`"cache": {"seconds": 0}`), so an outage leaves the cached list in place
instead of replacing it for a day.

The cost of a long window is that a new repository, or a changed star count,
shows up after the next refresh. The "pushed 3h ago" text is frozen at that
moment too, unless you set `TYPED_TIME: 1` in `env`: the script then sends the
date as data and Raffi works out the age each time it draws the row. Ctrl+R
refreshes the list you are looking at straight away.

Ctrl+R, and the server's warming runs, set `RAFFI_REFRESH=1`, which skips the
`gh` cache: an uncached page of 100 repositories takes one to three seconds,
so `repo` against a large account is slower there. With `--any-owner`, words
that are not accounts
are remembered in `~/.cache/raffi/github-repos/<host>/missing.json` for
`CACHE_MISSING`, which keeps a filter that looks like a login from costing a
request per keystroke, and the logged in account is remembered for
`CACHE_LOGIN` in `login.json` next to it. The open pull requests and issues of
each repository are kept in `~/.cache/raffi/github-repos/<host>/live/`, one
JSON file per repository, written by the background fetches and reused for
`CACHE_LIVE`; `queue.json` in the same directory lists the repositories still
waiting for one.

## Layout

| File | Holds |
|------|-------|
| `ghcommon.py` | Running `gh`, GraphQL through its cache (`RAFFI_REFRESH` bypasses it), error rows, relative dates and terminal colours |
| `pr.py` | Pull request queries, rows, markdown rendering and the live preview |
| `repos.py` | Repository listing, account and alias resolution, filtering, pages, action menus and the live pull request and issue menus |

New GitHub scripts should import from `ghcommon.py` rather than copy it.
`test_ghcommon.py` covers the shared parts, so the other two test files only
patch `ghcommon.run_gh` to fake GitHub.

## Tests

```bash
cd scripts/github && python3 -m unittest test_ghcommon test_pr test_repos
```
