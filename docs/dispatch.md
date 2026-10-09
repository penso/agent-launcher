# Dispatching agents

Prompt profiles, launch settings, PR reviews, and where to look when a dispatch fails.

## Prompt profiles

On first launch, agent-launcher creates editable prompt profiles at:

```text
~/.config/agent-launcher/agents/designer/prompt.md
~/.config/agent-launcher/agents/implementer/prompt.md
~/.config/agent-launcher/agents/reviewer/prompt.md
```

All dispatches open the prompt chooser, including PR reviews and private security advisories.
PR and security choosers include their built-in default alongside saved profiles.
Use Up/Down or 1-9 to select a template. Ordinary issues show raw source, including literal
placeholders such as `{{ issue_text }}` rather than the issue body;
PgUp/PgDn scroll the preview. Wide terminals show list and preview side by side, narrow ones
stack them. Enter advances to compute target selection (when applicable), then launch settings, and is blocked
while the template is loading or has a read error. `r` re-reads the template source; Esc closes
the chooser. PR and security previews expand the selected profile within their contextual
review prompt. Private previews stay local and use placeholders for the private checkout
and branch that will be prepared only after consent. Editing always loads raw template
source, never the rendered advisory or PR preview.

## Dispatch Selection

Issues, PR reviews, and private security dispatches follow **Prompt -> optional compute
target -> Launch settings**, with harness/model selection and optional appended instructions.
Private security never offers a compute-target chooser and requires a final privacy confirmation.
The summary shows the captured item, profile, backend, target, harness, and
model before an explicit Enter launches. Esc goes back without losing per-dispatch choices
(or closes the first stage); PgUp/PgDn scroll the settings summary on smaller terminals.
Selecting a target never launches immediately.

For every dispatch, **Additional instructions (this dispatch only)** is automatically focused
on first entering settings. Type or paste multiline text (including Unicode and code blocks);
it is appended after the rendered agent prompt, without editing the shared profile. Enter
inserts a newline; arrows, Home/End, Backspace and Delete edit the text. Tab, Esc, Ctrl+S or
Ctrl+Enter leave the field without launching and preserve its draft. From settings controls,
Tab returns to instructions and an explicit Enter launches. Harness/model shortcuts only
apply outside the field. Back navigation and refresh retain the draft; a new dispatch starts
empty. Text is appended literally after the composed prompt, including for PRs and security;
it is not interpreted as MiniJinja or saved into the shared profile. The limit is 16 KiB.

Press `h` to cycle the configured default and supported harness kinds: Herdr offers
`opencode`, `claude`, and `pi`; Native offers only `opencode`; Conductor offers `claude`,
`codex`, `cursor`, and `acp`; Superset keeps its configured preset only. The configured
default preserves custom configuration, including Native's OpenCode custom agent. These
are backend-supported kinds, **not an installed harness or model catalog**. Private Herdr
dispatch restricts harness choices to `opencode`, `claude`, and `codex`; private Native
still permits only `opencode`. Herdr's Codex adapter requires **Harness default** for
the model; overrides are rejected before private preparation. Runtime model/backend
validation remains authoritative.

Choose `1` **Configured default**, `2` **Harness default**, or `3` / `m` **Custom model**.
Custom model entry supports Unicode, arrows, Home/End, Backspace/Delete, and paste.
Enter confirms the field without launching; Esc cancels the field, retaining its previous
value. For example, select the issue `reviewer` profile and Claude with `sonnet`, or select
OpenCode/Pi with `openai/gpt-5.4`. Model IDs are entered manually; actual availability depends
on the harness on the selected host and runtime validation remains authoritative.

Changing harness resets the model to Harness default to avoid reusing another provider's
model. Configured default inherits the configured model only for the configured harness;
an unset model is shown as "uses harness default". Choices apply to this dispatch only,
without changing global configuration, installing anything, or changing permissions.
Refreshes retain the draft and selected identities. If backend/harness/model defaults change
while the chooser is open, launch is blocked with a request to reopen it rather than silently
using different defaults.

Press `a` to name a new profile, then Enter to edit its starter template; `e` loads the selected
profile's raw MiniJinja source. The multiline editor supports Unicode, arrows, Home/End,
Backspace/Delete, Enter for newlines, and bracketed paste. Ctrl+S saves explicitly; Esc cancels
with a discard confirmation for changed drafts (`y` discard, `n` or Esc keep editing). Ctrl+N
returns to name entry for a new profile; Tab/Shift+Tab toggles between its name and body.
Ctrl+S also saves from the name field. Save errors, including conflicts, retain the draft;
PgUp/PgDn scroll long editor errors. Input is locked while saving so the submitted draft
cannot be replaced or discarded before the save completes. A source refresh removing the
issue does not discard an open editor.
To reload a conflicting edit, cancel/discard and press `e` again. Saves are create-only for new
profiles and compare the exact previously loaded source for edits; duplicate saves are blocked.

These files are shared user configuration affecting **all repositories**, not project-local
settings. All reads, validation, previews, and saves go through the runtime; read-only files,
unsafe names, and symlink errors are shown in the modal. Templates use MiniJinja and are read
from disk for every dispatch, so edits take effect without rebuilding or restarting.

Available variables are `issue_text`, `issue_title`, `issue_link`, `issue_identifier`,
`issue_repository`, and `issue_provider`. MiniJinja filters and control flow are also available.
Undefined variables are rejected before an agent is started.

## Review Dispatch

Use **Review PR** on the selected PR to launch the configured agent and, for native workspaces,
choose a compute target. Select the built-in review or a saved profile, choose the
harness/model, and optionally append instructions. The built-in PR context is retained:
repository identity, base/head refs and SHAs, and explicit GitHub CLI/Git diff instructions.
Selected profiles supplement that context; additional text is appended literally afterward.
The agent must verify the PR revision rather than review the workspace's default branch. It is
instructed to report findings only, without implementing changes, posting comments/reviews,
approving, merging, committing, or pushing. These are prompt constraints, not a read-only sandbox.
The agent environment needs authenticated GitHub CLI or Git access to the PR and base objects.

- GitHub: `GH_TOKEN`, then `GITHUB_TOKEN`, then stored `gh auth` credentials for the remote host
- GitLab: `PRIVATE_TOKEN`, then `GITLAB_TOKEN`
- Conductor: `CONDUCTOR_API_TOKEN`, then `CONDUCTOR_API_KEY`
- Superset, Herdr, OpenCode, SSH, Daytona, Coder, and Azure use their installed CLI authentication

## PR Reviews

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

## Diagnostics

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
[`config.example.toml`](../config.example.toml) for all current options. Provider credentials remain
in their standard environments. Existing configuration in the platform config directory is used
as a fallback.

[← README](../README.md)
