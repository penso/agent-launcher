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
OpenCode. Conductor Cloud is also available explicitly. Native workspaces can run on an SSH host,
optionally waking Daytona, Coder, or an Azure VM before connecting.

Configuration is loaded from `~/.config/agent-launcher/config.toml`. See
[`config.example.toml`](config.example.toml) for all current options. Provider credentials remain
in their standard environments. Existing configuration in the platform config directory is used
as a fallback.

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
