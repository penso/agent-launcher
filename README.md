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

The default backend is `auto`: use Superset when the current repository is registered there, then
Herdr when its server is running and compatible, otherwise create a native Git worktree and run
OpenCode. Conductor Cloud is also available explicitly. Native workspaces can run on a named pool
of SSH targets, optionally waking Daytona, Coder, or an Azure VM before connecting.

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

The PRs tab shows GitHub PR numbers, titles, authors, a four-square diff indicator, and lifecycle
status on one line. One to four filled squares mean 1-10, 11-100, 101-1000, or over 1000 changed
lines; unused squares are dim outlines. Green/red squares approximate the addition/deletion ratio,
with both colors shown for mixed changes when at least two squares are filled. A single square
uses the dominant color (green on ties). Zero changes show four outlines; either count unknown
shows `?` followed by three outlines. PR details retain exact `+/-` counts.
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
| `d` | | Dispatch issue / review PR |
| `r` | | Refresh |
| `i` | | Send input |
| `o` | | Open workspace/session |
| `s` | | Stop run |
| `Esc` | Quit | Return to inbox |
| `Ctrl-C` | Quit | Quit |

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
