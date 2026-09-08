# agent-launcher

A repository-local issue inbox for dispatching coding agents into isolated local or remote
workspaces.

Run `agent-launcher` from inside a Git repository. It detects the current repository's GitHub or
GitLab remote from Git configuration and also enables Beads when `.beads` exists. Pull requests
and merge requests are intentionally excluded.

Override the detected remote to fetch issues for a specific GitHub or GitLab repository:

```sh
agent-launcher --remote https://github.com/owner/repository.git
agent-launcher --remote git@gitlab.com:group/repository.git
```

HTTPS, `git://`, `ssh://`, and SCP-style Git URLs are supported. The override is also used as the
repository identity when selecting or creating a workspace backend.

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

- GitHub: `GH_TOKEN`, then `GITHUB_TOKEN`
- GitLab: `PRIVATE_TOKEN`, then `GITLAB_TOKEN`
- Conductor: `CONDUCTOR_API_TOKEN`, then `CONDUCTOR_API_KEY`
- Superset, Herdr, OpenCode, SSH, Daytona, Coder, and Azure use their installed CLI authentication

## Keys

| Key | Inbox | Detail |
| --- | --- | --- |
| `↑` / `↓` | Navigate | Scroll |
| Type / Backspace | Filter | |
| `Enter` | Open issue | |
| `Ctrl+G`, then `d` | Dispatch | |
| `Ctrl+G`, then `r` | Refresh | |
| `Ctrl+G`, then `s` | Choose sorting | |
| `d` | | Dispatch |
| `r` | | Refresh |
| `i` | | Send input |
| `o` | | Open workspace/session |
| `s` | | Stop run |
| `Esc` | Quit | Return to inbox |
| `Ctrl-C` | Quit | Quit |

## Development

```sh
just run
just format-check
just lint
just test
```
