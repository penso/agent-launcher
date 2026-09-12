# agent-launcher

A repository-local issue and PR inbox for dispatching coding agents into isolated local or remote
workspaces.

Run `agent-launcher` from inside a Git repository. It detects the current repository's GitHub or
GitLab remote from Git configuration and also enables Beads when `.beads` exists. GitHub pull
requests appear in a separate PRs tab; GitLab merge requests are not yet supported.

Override the detected remote to fetch issues for a specific GitHub or GitLab repository:

```sh
agent-launcher --remote https://github.com/owner/repository.git
agent-launcher --remote git@gitlab.com:group/repository.git
```

HTTPS, `git://`, `ssh://`, and SCP-style Git URLs are supported. The override is also used as the
repository identity when selecting or creating a workspace backend.
It does not clone that repository or change the local checkout. Herdr and local native dispatch
still use the repository where you launched `agent-launcher`; run from the intended repository's
checkout to dispatch work there, even when `--remote` points the inbox at another repository.

### Layout

`--layout flexible` (the default) fills the available width and height with side
padding. Taller terminals show more Issues/PR rows, with a compact activity panel
above the normally sized, horizontally centered logo. Legends and command hints
stay above the global footer at the bottom. Extra table width goes primarily to
issue and PR titles; the activity panel and detail view also use the available width.
Use `agent-launcher --layout fixed` for the original centered, 104-column-capped,
fixed-height inbox. Detail views use the available height in both modes. Command
and debug overlays retain their existing sizing. The flag can be combined
with `--remote` and applies only to the current launch.

### Terminal Title

The interactive TUI sets its own terminal title to `launcher` using Crossterm's OSC title
command. It saves and restores the previous title with xterm title-stack sequences on exit
(including errors and handled termination signals), where the terminal supports them.
Forced termination or terminals without title-stack support cannot guarantee restoration.
Herdr 0.9.0 captures OSC titles as terminal metadata, separately from its tab labels.
When `HERDR_ENV=1` and `HERDR_TAB_ID` is nonempty, startup also invokes
`herdr tab rename -- <HERDR_TAB_ID> launcher` with a one-second timeout and suppressed output.
This targets only the inherited tab ID, never UI focus or a list of other terminals.
Failure is nonfatal. The custom tab label persists after exit: Herdr has no reset-to-auto API.
Its OSC tracker does not track title-stack restoration either; a later shell OSC title update
replaces that metadata. If a pane is moved between tabs, restart from a fresh shell context
before launching: an inherited tab ID may refer to its original tab. Do not forward Herdr
context variables to unrelated terminals.
See Herdr's [title synchronization](https://github.com/herdrdev/herdr/blob/v0.9.0/src/app/terminal_titles.rs)
and [tab rename implementation](https://github.com/herdrdev/herdr/blob/v0.9.0/src/app/api/tabs.rs).

## Dispatch

### Diagnostics

Press `Ctrl+G`, then `g` to open Debug, including the persistent diagnostic log path,
the latest diagnostic failure, and any logging warning. `diagnostics.log` lives beside
the repository's `state.sqlite3` in its existing per-repository user data directory.
The log appends across launches, rotating at 1 MiB to `diagnostics.log.1` (one backup).
New log files are owner-only on Unix. Logging failures are nonfatal and fall back to
stderr with a warning also shown in Debug.

Records contain UTC timestamps, levels, fixed operation/outcome labels, backend kinds,
and hashed identity references. They deliberately exclude raw errors, prompts, input,
terminal output, credentials, URLs, and command arguments. Failures are categorized
by operation rather than attempting unreliable secret-pattern redaction. Use the
existing on-screen error and source/backend status for actionable details. Dispatch,
review, input, stop, open, and deletion show pending and success/failure UI statuses;
their runtime starts and outcomes persist even after a later successful operation.
Source refresh/backoff, backend detection, and runtime/store failures are recorded too.
CLI failures after the repository data path is established are recorded as session
failures; earlier repository discovery/argument errors remain stderr-only. This is
an operation log, not a terminal transcript or a replacement for backend server logs.

The default backend is `auto`: use Superset when the current repository is registered there, then
Herdr when its server is running and compatible, otherwise create a native Git worktree and run
OpenCode. Conductor Cloud is also available explicitly. Native workspaces can run on a named pool
of SSH targets, optionally waking Daytona, Coder, or an Azure VM before connecting.

Herdr dispatch from a linked worktree uses the repository's primary checkout for worktree creation.
Unless an explicit base is supplied, the new worktree starts at the original workspace's current
`HEAD` commit, not the primary checkout's branch. Uncommitted changes are not copied. Repositories
with a bare primary worktree are unsupported by this backend; use a non-bare clone instead.
For a separately located Git directory, linked-worktree dispatch requires the primary checkout's
`core.worktree` to be configured. Git does not otherwise record its location; launch from that
primary checkout if it is not configured. The launcher never writes this configuration for you.

For SSH workspaces, agent-launcher starts one detached OpenCode server per worktree and connects to
it through a disposable local SSH tunnel. If the tunnel or launcher exits, the next refresh or input
action recreates the tunnel and verifies the persisted OpenCode session. After a remote host
restart, the server is relaunched in the same worktree so OpenCode can recover its stored session.
Each server receives a generated HTTP Basic Auth password; local session metadata is restricted to
the current user. Dispatch shows `Automatic` and every configured target with availability, active
run capacity, CPU load, and memory load. Automatic placement prefers already-online targets, then
the target with the fewest active launcher runs and lowest system load; `placement = "random"` is
also available.
Browser opening is intentionally disabled for authenticated remote sessions so credentials are not
placed in process arguments or browser history.
The remote host must provide a Unix-like shell, Git, OpenCode, `nohup`, `od`, `tr`, `readlink`,
`awk`, `uname`, and `sleep`, plus `ps` with `-o lstart=` and `xargs` with `-0` support. macOS load
sampling also requires `top`, `vm_stat`, and `sysctl`. Detection verifies these requirements.

Configure multiple targets with `[[compute.targets]]` entries. Target IDs are stable routing
identities; the ID, host, and workspace root must not change while runs remain on a target.
`max_active_runs` is enforced across runs in the current repository's launcher registry; separate
repositories or launcher processes do not share reservations. Full and offline targets remain
visible but cannot be chosen. The legacy singular `[ssh]` table remains supported, but it cannot
be combined with `[compute]`.

Configuration is loaded from `~/.config/agent-launcher/config.toml`. See
[`config.example.toml`](config.example.toml) for all current options. Provider credentials remain
in their standard environments. Existing configuration in the platform config directory is used
as a fallback.

### Prompt profiles

On first launch, agent-launcher creates editable prompt profiles at:

```text
~/.config/agent-launcher/agents/designer/prompt.md
~/.config/agent-launcher/agents/implementer/prompt.md
~/.config/agent-launcher/agents/reviewer/prompt.md
```

Add another profile by creating `agents/<name>/prompt.md`. When more than one profile is available,
dispatch opens a chooser; a single profile is selected automatically. Templates use MiniJinja and
are read from disk for every dispatch, so edits take effect without rebuilding or restarting.

Available variables are `issue_text`, `issue_title`, `issue_link`, `issue_identifier`,
`issue_repository`, and `issue_provider`. MiniJinja filters and control flow are also available.
Undefined variables are rejected before an agent is started.

### PR Reviews

The PRs tab shows GitHub PR numbers, titles, authors, activity, a diff indicator, and lifecycle
status on one line. The indicator uses four squares below 120 table columns, six at
120-159, and eight at 160 or more, based on rendered table width after padding and
scrollbar space. Fixed layout retains four squares. Totals of 1-10, 11-100, 101-1000,
or over 1000 changed lines fill 25%, 50%, 75%, or 100% of the indicator, rounded to
the nearest cell (ties up, at least one). Unused squares are dim outlines.
Green/red squares approximate the addition/deletion ratio,
with both colors shown for mixed changes when at least two squares are filled. A single square
uses the dominant color (green on ties). Zero changes show all outlines; either count unknown
shows `?` followed by outlines to fill the indicator. PR details retain exact `+/-` counts.
Each tab retains its own filter and selection. Full refreshes
retrieve open and draft PRs separately from issues; incremental updates also show closed/merged
transitions until the next full reconciliation. Failed list requests preserve the cached inbox.
Issues and PRs share the persistent SQLite cache. Every GitHub sync paginates the complete open
PR list; issue deltas overlap by five minutes, with full reconciliation every six hours. PR
details are reused across refreshes and restarts while the update timestamp, head/base revisions,
and state match. Missing or changed details are fetched in rotating batches of at most ten per
sync, so later PRs are not starved. Optional failures retain known metadata and counts (which may
be stale until enrichment succeeds); never-fetched counts remain unknown.

GitHub throttling pauses all requests for that source, including manual refreshes, while keeping
cached lists visible. Source status reports the retry deadline rather than going offline. The
launcher honors `Retry-After` (seconds or HTTP date) and `X-RateLimit-Reset`, using the later valid
deadline; without one, retries back off from 60 seconds exponentially to a one-hour maximum.
Backoff resets after a successful sync and lasts for the source's runtime lifetime, not across
restarts. No command-dispatch thread sleeps; the next refresh after expiry resumes syncing.
Use `gh auth login --hostname github.com` to authenticate if needed, then restart the launcher
so it picks up the credentials.

### Item Activity

The aligned `activity` column shows absolute engagement, not activity within a recent
time window and not completion progress. `≡` means discussion comments; `○` means PR
commits. Counts below ten use warm muted text; larger counts use the warm accent.
Zero means the API reported zero; `?` means unavailable or not yet enriched.
For PRs, discussion combines regular and inline review comments when both are known.
If only one is known, `+` marks a partial total (for example `≡5+`, even `≡0+`).
Large counts are truncated to whole units (`1k`, `2M`, etc.); details show exact separate
comments, review comments, and commit counts, with `unknown` for missing fields.

GitHub issues use REST `comments`; PRs use `comments`, `review_comments`, and `commits`
from the existing PR detail fetches, plus comments from issue-list PR summaries when
available. Omitted list/detail fields remain unknown or retain previously known counts.
GitLab issues use `user_notes_count` when supplied; Beads has no available activity counts.
Review comments count inline comments, not review submissions, approvals, or events.
No review/event/commit counting endpoints are called. Enrichment keeps the existing
ten-detail-per-sync budget, including a one-time refresh of older cached PR revisions.
Changed update timestamps invalidate detail revisions; counts can remain stale while
waiting for enrichment or during API failures. Missing activity fields alone do not
cause repeated detail requests for an otherwise successfully fetched revision.

Issues show discussion at 72 or more rendered table columns. PRs show discussion at
76 columns and add commits at 100; below those breakpoints activity hides before
core ID, status, or diff columns are sacrificed. Padding and scrollbars are excluded
from these widths. Titles retain the flexible space beyond the bounded metadata columns.

### Review Dispatch

Use **Review PR** on the selected PR to launch the configured agent and, for native workspaces,
choose a compute target. Reviews bypass issue prompt profiles and use a dedicated prompt with
repository identity, base/head refs and SHAs, and explicit GitHub CLI/Git diff instructions.
The agent must verify the PR revision rather than review the workspace's default branch. It is
instructed to report findings only, without implementing changes, posting comments/reviews,
approving, merging, committing, or pushing. These are prompt constraints, not a read-only sandbox.
The agent environment needs authenticated GitHub CLI or Git access to the PR and base objects.

- GitHub: `GH_TOKEN`, then `GITHUB_TOKEN`, then stored `gh auth` credentials for the remote host
- GitLab: `PRIVATE_TOKEN`, then `GITLAB_TOKEN`
- Conductor: `CONDUCTOR_API_TOKEN`, then `CONDUCTOR_API_KEY`
- Superset, Herdr, OpenCode, SSH, Daytona, Coder, and Azure use their installed CLI authentication

## Keys

| Key | Inbox | Detail |
| --- | --- | --- |
| `↑` / `↓` | Navigate | Scroll |
| Type / Backspace | Filter | |
| `Tab` / `Shift+Tab` | Switch Issues / PRs | |
| `Enter` | Open issue or PR | |
| `Ctrl+G`, then `d` | Dispatch issue / review PR | |
| `Ctrl+G`, then `r` | Refresh | |
| `Ctrl+G`, then `s` | Choose sorting | |
| `Ctrl+G`, then `g` | Open Debug runtime status | |
| `d` | | Dispatch issue / review PR |
| `r` | | Refresh |
| `i` | | Send input |
| `o` | | Open workspace/session |
| `s` | | Stop run |
| `Esc` | Quit | Return to inbox |
| `Ctrl-C` | Quit | Quit |

Debug shows the live snapshot's selected backend and agent, detected worktree manager,
backend availability, compute targets, local repository path and effective host/repository,
source connection messages (including throttling), and last refresh. It does not read
configuration or environment variables or display the raw remote URL. Use arrows,
Page Up/Down, Home/End, or the mouse wheel over the pane to scroll; Esc closes it
without changing the inbox search or selection.

Click a tab to switch between Issues and PRs. Hover a row to select it, then click to open its details. Mouse-wheel
scrolling over the list moves three items at a time; over details it moves three lines. Overlays
block background mouse actions. Opening a PR does not launch a review.

## Development

```sh
just run
just format-check
just lint
just test
```
