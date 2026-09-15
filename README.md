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
with the centered logo inside its uncollected history area. This reclaims the
separate logo's three rows for Issues/PRs without enlarging the top panel. Smaller
terminals without an activity panel retain the separate logo. Legends and command hints
stay above the global footer at the bottom. Extra table width goes primarily to
issue and PR titles; the activity panel and detail view also use the available width.
Use `agent-launcher --layout fixed` for the original centered, 104-column-capped,
fixed-height inbox. Detail views use the available height in both modes. Command
and debug overlays retain their existing sizing. The flag can be combined
with `--remote` and applies only to the current launch.

### Description Markdown

Issue, PR, and private security advisory detail descriptions use a local
`pulldown-cmark` 0.13 event-stream renderer. Headings, nested bold/italic/strike,
inline code, fenced code with language captions, nested numbered/bullet lists,
task checkboxes, quotes, GFM alerts, links, rules, and aligned GFM tables are styled
with the existing terminal theme. Code blocks have a distinct background; syntax
highlighting is not enabled. Tables wrap styled cells independently to the actual
body width and switch to stacked header-labeled cells when columns cannot fit.
Wrapping preserves whitespace and grapheme widths (including CJK and emoji).
A grapheme wider than the entire viewport is displayed as ASCII Unicode escapes.

Links display their destination as plain text, never OSC hyperlinks or automatic
navigation. Images show only static alt text; raw HTML (including `<br>`) stays
literal. Nothing is fetched or executed. ESC and other C0/C1 controls are removed
before parsing and from decoded text/URLs; newlines remain structural and tabs
become four spaces. Escape-sequence payloads may remain visible as inert text.
The renderer does not log content or write transformed descriptions to disk.

Only the detail Description section is Markdown. Metadata, persisted run output,
prompt chooser/editor templates, and runtime prompt interpolation remain unchanged;
`Issue.description` retains its raw source. A single in-memory description cache
is reused until the source or body width changes and cleared on leaving details.
Scrolling counts rendered lines and clamps after resize, including the Latest run
section below the description.

Display-only limits are 64 KiB of UTF-8 source, 65,536 parser events, 128 KiB of
expanded text, and the smaller of 16,000 lines or one million viewport cells.
Rendering width is capped at 4,096 columns. A visible notice marks truncated
descriptions; these limits never truncate the underlying issue or prompt source.
The reusable API is `widgets::markdown::render(source, width, MarkdownTheme)`,
returning owned Ratatui `Text` with pre-wrapped lines and configurable styles.

### Terminal Title

The interactive TUI sets its own terminal title to `launcher` using Crossterm's OSC title
command. It saves and restores the previous title with xterm title-stack sequences on exit
(including errors and handled termination signals), where the terminal supports them.
Forced termination or terminals without title-stack support cannot guarantee restoration.
Herdr 0.9.0 captures OSC titles as terminal metadata, separately from its tab labels.
When `HERDR_ENV=1`, startup resolves its live tab using `herdr pane current --current`,
then invokes `herdr tab rename -- <tab_id> launcher`. The entire operation has a
one-second timeout and suppressed output. This targets only the calling pane's tab,
never UI focus or a list of other terminals, and does not require `HERDR_TAB_ID`.
Failure is nonfatal. The custom tab label persists after exit: Herdr has no reset-to-auto API.
Its OSC tracker does not track title-stack restoration either; a later shell OSC title update
replaces that metadata. Do not forward Herdr context variables to unrelated terminals.
See Herdr's [title synchronization](https://github.com/herdrdev/herdr/blob/v0.9.0/src/app/terminal_titles.rs)
and [tab rename implementation](https://github.com/herdrdev/herdr/blob/v0.9.0/src/app/api/tabs.rs).

### Herdr Activity

The live top graph shows the **number of Herdr-recognized working agents**, not a
synthetic activity score. It includes agents launched outside launcher and is
independent of the selected repository, issue, backend, or Herdr UI machine.
The compact header shows working agents, nonzero blocked agents when space permits,
and coverage such as `1/1 sessions` or `1/2 partial`. Detailed state counters and
configured/discovered scope are in Debug (`Ctrl+G`, then `g`). Herdr's
`done` means an unseen result, not verified task success; `unknown` is a semantic
state, not a disconnected host.

Default coverage includes discovered running local sessions, enabled saved SSH
target/session profiles, and explicit `[herdr_activity]` endpoints. Other sessions
on remote hosts are **not** queried unless `discover_remote_sessions = true` is
configured. Disabled profiles and explicit exclusions win over discovery. Use
alias groups to deduplicate SSH aliases or local/loopback routes that identify the
same account and host; physical-host equivalence cannot be inferred automatically.
See `config.example.toml` for endpoints, exclusions, aliases, and XDG namespaces.
Set `enabled = false` in `[herdr_activity]` to turn collection off.

Collection uses read-only CLI commands and authenticated, noninteractive OpenSSH.
It never attaches, starts servers, installs integrations, or upgrades Herdr. SSH
host keys must already be trusted; agent/X11 forwarding is disabled. Inherited
Herdr routing and XDG overrides are cleared, with optional explicit XDG namespaces.
Arbitrary custom socket paths, unconfigured hosts, inaccessible accounts, and
processes Herdr does not recognize are outside coverage.

Inventories are polled every two seconds, with at most four concurrent requests,
ten-second deadlines, and bounded retry backoff. Discovery refreshes every minute
and on manual refresh. Observations expire after ten seconds; a recent observation
can remain fresh while its next request has failed, and Debug shows both conditions.
Partial counts are lower bounds. Debug (`Ctrl+G`, then `g`) shows sanitized discovery,
persistence, per-endpoint failure reasons, and latest-sample totals and coverage
(including stale, failed, never-observed, and excluded sessions) without terminal output.

The graph retains 15 minutes and shows the peak working count per display bucket,
with a count scale rather than a 0-100 score. The Braille trace uses two buckets per
character and four vertical dots per row. Adjacent complete buckets are connected;
incomplete buckets are dim, unconnected lower-bound dots without underlines.
The header's `partial` status and Debug coverage provide textual context (flat
partial and complete dots can have the same geometry). Gaps are blank, and observed zero sits visibly on the
bottom edge. A compact footer shows `-15m`, the peak agent count, and `now` when
space permits; the graph keeps most of the panel's height. Only aggregate counts
and coverage are stored in the existing local SQLite database, with 30-minute retention. Restart restores
original sample times; downtime and missed transitions are not reconstructed.
The muted AGENT LAUNCHER logo is centered inside the graph (short text below 62
graph columns or two graph rows). As history fills from right to left, whole
terminal columns of the logo disappear starting at the first sampled bucket.
Zero counts, missing samples, and subsequent outage gaps all reserve their timeline;
the logo never sits under recorded data. Full restored history hides it immediately.
The earliest nonfuture sample time is remembered for this launch, so pruning or an
empty snapshot cannot make the logo reappear in previously covered history.
Clock rollback invalidates affected stored history once the reset commits.

Polling is sampled observation, not a lossless transition log or work-throughput
measurement. Collection runs only while launcher runs. Setting
`AGENT_LAUNCHER_DEMO_ACTIVITY` to any value, including an empty value, explicitly
selects a labeled demo and disables real collection and sample writes. Renderer-only
synthetic two-second samples use the same panel, colors, partial dots, and gaps as
live activity. A dedicated 30 fps redraw timer uses monotonic elapsed time, leaving
the 80ms spinner tick unchanged. The demo starts empty and fills its 15-minute
virtual window in 18 seconds, then scrolls continuously without restarting the logo
reveal, even after delayed frames. Only the demo projects fractional-time waveform
heights at fixed-point precision; its counters remain integral and its stable scale
is 0-8 agents. Motion scales with graph width (at most one horizontal Braille dot
per scheduled frame up to 270 columns); terminal dot resolution still limits motion.
Live counts, peak bucketing, and missing observations are never interpolated. Failed telemetry
never falls back to demo data.

### Reusable Braille Widgets

`agent_launcher_tui::widgets` exports `BrailleSparkline`, `SparklineSample`, and
`SparklineVariant`. The widget borrows a slice of domain-independent observations
and implements Ratatui's `Widget`, without requiring a `Frame`, Herdr model, or theme.

```rust
use agent_launcher_tui::widgets::{BrailleSparkline, SparklineSample, SparklineVariant};
use ratatui::style::{Color, Style};

let samples = [
    SparklineSample { value: Some(0), partial: false },
    SparklineSample { value: Some(7), partial: false },
    SparklineSample { value: None, partial: false },
    SparklineSample { value: Some(4), partial: true },
];
let widget = BrailleSparkline::new(&samples)
    .max(10) // Omit, or call .auto_max(), to scale to visible observations.
    .style(Style::new().fg(Color::Cyan))
    .variant(SparklineVariant::Line);
// frame.render_widget(widget, area);
```

`Line` (default) connects only adjacent complete observations. `Dots` never
interpolates. `Filled` draws independent vertical Braille columns, two per cell;
partial observations remain isolated, dim lower-bound dots in every variant.
Gaps stay blank and real zero remains a bottom dot. Dimming applies to the
whole cell when a partial dot shares it with complete data. The widget clears old
dim/underline markers on redraw and suppresses underlines even in its supplied style.

Each sample occupies one horizontal dot, with four vertical dots per character row.
The first `area.width * 2` samples are displayed without stretching or resampling;
callers choose their own bucketing or history window. Auto-scaling includes visible
partial values, ignores gaps and offscreen samples, and uses one for empty/all-zero
data. Explicit maxima clamp larger values; `.max(0)` also uses one. Integer `u128`
scaling is safe through `u64::MAX`, and buffer clipping preserves the original geometry.
Live and demo activity use this same widget with `Line` and the existing panel style.

The original implementation is inspired by the builder/Widget API and Braille pixel
approach in [penso/ratatui-braille-bar](https://github.com/penso/ratatui-braille-bar).
No progress-bar dependency or upstream implementation code is included.

Preview all three variants on the same animated simulated dataset (side by side,
or stacked in narrow terminals; quit with `q`, `Esc`, or `Ctrl+C`):

```sh
cargo run -p agent-launcher-tui --example braille_sparklines
```

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

Regular issue dispatch always opens the prompt chooser, even with one profile or no saved
profiles (the latter offers the built-in default). PR reviews still bypass issue profiles.
Use Up/Down or 1-9 to select a template and see its raw source, including literal
placeholders such as `{{ issue_text }}` rather than the issue body;
PgUp/PgDn scroll the preview. Wide terminals show list and preview side by side, narrow ones
stack them. Enter advances to compute target selection (when applicable), then launch settings, and is blocked
while the template is loading or has a read error. `r` re-reads the template source; Esc closes
the chooser. MiniJinja variables are expanded only for dispatch, not template selection.

### Dispatch Selection

Every issue dispatch and PR review ends at **Launch settings**. Issues follow
Prompt -> optional compute target -> Launch settings; PR reviews skip the issue prompt.
The summary shows the captured issue, profile (or PR review), backend, target, harness, and
model before an explicit Enter launches. Esc goes back without losing per-dispatch choices
(or closes the first stage); PgUp/PgDn scroll the settings summary on smaller terminals.
Selecting a target never launches immediately.

For ordinary issues, **Additional instructions (this dispatch only)** is automatically focused
on first entering settings. Type or paste multiline text (including Unicode and code blocks);
it is appended after the rendered agent prompt, without editing the shared profile. Enter
inserts a newline; arrows, Home/End, Backspace and Delete edit the text. Tab, Esc, Ctrl+S or
Ctrl+Enter leave the field without launching and preserve its draft. From settings controls,
Tab returns to instructions and an explicit Enter launches. Harness/model shortcuts only
apply outside the field. Back navigation and refresh retain the draft; a new dispatch starts
empty. PR reviews and private security dispatches do not offer this field.

Press `h` to cycle the configured default and supported harness kinds: Herdr offers
`opencode`, `claude`, and `pi`; Native offers only `opencode`; Conductor offers `claude`,
`codex`, `cursor`, and `acp`; Superset keeps its configured preset only. The configured
default preserves custom configuration, including Native's OpenCode custom agent. These
are backend-supported kinds, **not an installed harness or model catalog**.

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

### Security Reviewer Profile

Select **security reviewer** in the issue dispatch prompt chooser for a focused,
read-only security audit rather than the implementation-oriented `reviewer` profile.
The bundled template is [`prompts/security-reviewer.md`](prompts/security-reviewer.md).
New installations seed it as the fourth default at
`~/.config/agent-launcher/agents/security reviewer/prompt.md`. Seeding happens only
when the `agents` directory is absent: existing, edited, deleted, or custom profiles
are never automatically replaced or replenished. Existing installations can add a
profile named exactly `security reviewer` using the bundled template. PR Review
dispatch still uses its separate prompt, not this issue profile.

The profile establishes issue/diff scope, researches full-file context and existing
controls, and traces attacker-controlled inputs to sensitive operations. It includes
conditional Rust checks for unsafe/FFI invariants, panics, deserialization bounds,
async locks, cancellation, and resource exhaustion alongside authorization, CLI/SSH,
filesystem, web/SSRF, secret persistence, and verified dependency-advisory checks.
Reports separate severity-ranked, evidence-backed findings from hypotheses and
suggested fixes/tests, and disclose coverage gaps and tests not run. No findings does
not certify security.

It instructs the agent not to edit, autofix, stash, commit, push, upload, dump secrets,
install tools, or probe live services. Tests require explicit authorization and a
reviewed, safe isolated environment; `cargo test` is not inherently harmless because
build scripts, proc macros, and tests execute code. These are **prompt constraints,
not a read-only sandbox**: the agent harness and tool permissions must enforce the
boundary. Selecting this profile does not change backend permissions or configuration.

This is an original synthesis, not a copied upstream prompt or an upstream Rust policy:

- [Anthropic security review](https://github.com/anthropics/claude-code-security-review/blob/main/claudecode/prompts.py)
  informed high-confidence exploitability, context research, comparative controls, and
  source-to-sink analysis. Its DoS/on-disk-secret exclusions and numeric confidence
  thresholds are deliberately not adopted.
- [OpenAI security best practices](https://github.com/openai/skills/blob/main/skills/.curated/security-best-practices/SKILL.md)
  informed stack identification, severity/location reporting, and separating fixes from
  review. Its supported-language scope does not include Rust; this checklist adds Rust
  coverage and does not adopt its implementation or report-file-writing modes.
- Local inspiration: Moltis
  `crates/skills/src/assets/software-development/requesting-code-review/SKILL.md`
  for the reviewer/untrusted-code boundary (not its autofix/commit workflow), OpenCode
  `packages/core/src/plugin/command/review.txt` for full-file context and avoiding
  theoretical false positives, and Moltis `CLAUDE.md` for SSRF, secret/Debug exposure,
  async lock scope, and provenance awareness. These are references, not runtime
  dependencies or instructions to fetch content.

### Manual / Away

Manual is the default. Away ranks eligible issues and starts implementation workers
in retained worktrees as capacity becomes available. It currently requires the
**selected local Herdr backend and OpenCode harness**, with Herdr running; there is
no fallback to another backend or harness. PRs, private security advisories, blocked
issues, and closed items are excluded. The queue is independent of inbox search/sort.

1. Click the compact mode button at the far right of the Issues/PRs/Security tabs row (in the footer on detail screens or narrow terminals), or press `Ctrl+G`, then `m` to open controls.
2. Choose the **Away** tab by clicking it or using `Tab` / left-right arrows.
   Tabs preview each mode's attributes without changing the active mode. Manual has
   no global settings; Away has its own worker settings and queue. Away drafts survive
   tab changes. Set **Max agents** by typing a number or using `+` / `-`: **1..64**, initially **5**.
   The first digit replaces the displayed value; Backspace edits it. Use `p` to
   cycle profiles. With no saved profile selection, the chooser prefers `implementer`,
   then the first available profile, then the built-in prompt. Ranking defaults to
   **Agent**; `r` toggles **Source priority** (numeric priority ascending, missing last;
   ties oldest first, then canonical issue key).
3. Press `Enter` (or `s`) to Start. This captures the profile selection, ranking, and configured
   model/effort; an unset model uses the harness default. While Away is enabled,
   `l` applies an edited limit, `Enter` / `a` pauses/resumes, and `o` explicitly reprioritizes
   using the selected ranking. `Esc` only closes controls. The button shows the active
   mode; the modal shows progress, queue reasons, and errors.

**Pause and drain:** Pause stops new automatic admissions, not existing workers or
already-submitted launches. Select the **Manual** tab and press `Enter` to switch modes
and let existing work drain while launcher continues recording results. Lowering the
limit also leaves workers running.
The limit covers launcher-managed/known runs, including the prioritizer, pending
reservations, active runs, and disconnected/unknown outcomes. Ordinary tracked Herdr
runs marked completed also count: Herdr's done status does not prove process exit.
It is **not a global machine cap** and does not count external agents or manually
resumed `opencode -s` processes. Pause or switch to Manual before resuming sessions
outside launcher if you need to preserve capacity. Manual dispatches while Away is
enabled share its limit; Manual does not release duplicate issue reservations.

**Keep the TUI running** for scheduling and result finalization. Quitting with active
or uncertain Away work, a prioritizer, or a pending mode request shows a warning:
`q` confirms, `Esc` cancels, and `m` switches to Manual so you can wait for draining.
There is no scheduling daemon: closing launcher stops scheduling without force-killing
workers or deleting their worktrees. Workers may continue. Previously enabled Away
restores **Paused**; refresh and inspect workers before resuming. The CLI acquires the
repository runtime-owner lock before loading the runner registry; if ownership is
unavailable, resolve the other launcher instance before starting another.

**Results and recovery:** Workers use `opencode run --format json --agent build`,
explicitly selecting the implementation primary agent while honoring OpenCode
user/project configuration and permissions. They are instructed to implement, test,
report, and exit, without pushing, opening PRs, merging, closing issues, or spawning
agents. Away has no automatic publication/closure step; these instructions are not
a sandbox. Finished requires confirmed process exit and a valid success report,
not Herdr idle/done. Review the retained worktree and tests yourself. Issue details
show the resume command once the **actual session ID** is captured:
`cd -- '<worktree>' && opencode -s <session-id>` (not the launcher run UUID).
Run output gives raw-log locations. To interrupt a worker, use its retained Herdr
pane; launcher Stop does not blindly interrupt uncertain processes or resumed sessions.
Known early failures before worker submission become **Failed** and release slots;
possibly submitted, unknown outcomes retain reservations and need inspection before
replacement. Resume/reprioritize does not automatically retry recorded attempts.

New admissions wait for successful refresh of **all ordinary issue sources**; failed,
in-flight, or throttled refreshes do not refill from cached issues. Fix the source
error and refresh (rate-limit deadlines still apply). Three distinct worker failures
trip an **Attention** circuit pause; inspect their results, then use `a` to Resume.
Launch/persistence errors can also stop scheduling and require inspection.

Agent ranking sends candidate issue content to the model and accepts at most **512
issues / 512 KiB of serialized inventory**, with a 64 KiB per-request input limit
(including each full issue) and a 256 KiB global-summary limit. Batches retain full
bodies before global summary ranking. Oversized input or ranking failures stop new
work, with **no silent truncation or fallback**. Fix the error and Resume, or select
Source priority with `r` and apply it with `o`.
The prioritizer uses provider API-key environment variables and the **authoritative
shared OpenCode data/auth store**, so OAuth refresh writes go to the same store as
other OpenCode processes, not a copied rotating-token file. Its HOME, working directory,
and configuration are separated from your project/user configuration, with a scratch
session database and tools denied by OpenCode configuration. This is **not OS isolation**.
Custom provider configuration and external plugins are unavailable; managed config
and `wellknown` remote-config authentication block ranking. For auth/model errors,
check the shared credentials and a built-in provider/model, or select Source priority.

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

### Security Tab

The third tab, **Security**, lists private GitHub repository advisories separately from
Issues and PRs. `Tab` moves forward and `Shift+Tab` moves backward through all three;
clicking a tab selects it directly. Each tab retains its own filter, selection, scroll,
and sort. Typing, including digits and `d`, remains search text in the inbox. Search
advisories by title, GHSA, CVE, or severity. Details show the exact identifiers, severity,
state, and **PRIVATE** label; advisory bodies are intended only for this local UI.

The advisory source uses the effective GitHub host/repository and GitHub authentication:
`GH_TOKEN`, then `GITHUB_TOKEN`, then stored `gh auth` credentials for that host. Private
advisory access requires an account with access to the repository's security advisories,
not merely access to public issues. A classic personal access token needs `repo`; a
fine-grained token needs repository **Repository security advisories: read** permission
to list advisories. Fork creation/preparation requires the corresponding write permission
and sufficient repository/advisory collaborator access. Organization policies and host
support can also restrict access. The Security empty state distinguishes no advisory
source, unavailable/unauthorized access, and a successful empty list. Its health is
independent of ordinary issue-source health.

The list includes **triage and draft** advisories, even when ordinary issue loading is
empty; closed/published advisory history is excluded. Triage reports are read-only here:
accept them manually on GitHub before preparing a draft. To dispatch a draft, open its
details and press `d`, or use `Ctrl+G`, then `d` from Security. Choose only the harness
and model, then press Enter to review the privacy warning. There is no custom-prompt
or compute-target chooser. **Enter never grants privacy consent.** The entire warning
and captured target must fit on screen before `y` can explicitly confirm. Resize or
cancel with Esc if it does not fit. Duplicate dispatches are blocked while preparation
is pending, and the runtime revalidates the captured backend and advisory.

Initially, private dispatch supports only **local Native with no configured compute
targets, or Herdr**. Other backends and Native configurations with any compute targets
are blocked, with **no automatic public fallback**. After consent, the request may
create the advisory's temporary private fork on GitHub and prepare an isolated local
clone with private remotes only. GitHub disables CI and integrations in temporary private
forks; do not rely on the usual repository automation to validate a fix there. The launcher
does not request automatic public PR publication, advisory publication, or merging, and
does not use the normal public worktree/PR workflow. These are workflow restrictions,
not a sandbox preventing an agent from using other tools. Private-clone cleanup is a
separate operation and is currently unavailable in this UI: `x` retains the clone and
explains the restriction, while `X` source deletion is unsupported. Normal runtime
worktree preview/deletion APIs also reject confidential runs before inspection or
cleanup. Existing private runs can still be opened, stopped, and sent input.

Advisory titles, descriptions, and metadata are intentionally cached locally in a dedicated
`security_advisory_cache` SQLite table, isolated by exact `security:github:host:repo` source
key from ordinary issues and checkpoints. The snapshot and its HTTP checkpoint are replaced
atomically only after a complete successful refresh. Persistent caching requires an owner-only
`0700` repository application directory and `0600` database file; new database files are
created with these permissions before content is written. Existing owned, single-link
`0644`/`0640` databases inside an owned `0700` application directory are hardened through
a no-follow, pinned file descriptor before SQLite opens them. Shared directories and
deliberately read-only files are not made writable. Other insecure or symlinked cache
paths are refused for private caching, with live data still available in memory.

Restored advisories are shown as cached and awaiting verification. Background advisory
refreshes use a five-minute TTL based on the persisted last successful full refresh, independently
of ordinary issue polling. Explicit Refresh bypasses both runtime and source-local TTLs,
retaining page ETags for HTTP verification, but never bypasses a server rate-limit deadline.
A local cache hit is not evidence of current access and remains awaiting verification.
Failed refreshes retain the last snapshot as stale and cancel pending dispatches;
loss of advisory access during refresh or either dispatch-time revalidation removes the
source's cached rows and cancels its pending jobs. Removed sources are pruned at startup.
Cached content never authorizes dispatch: live advisory/private-fork revalidation and explicit
model-provider consent remain required.

Revocation also writes an opaque, empty `0600` marker in the private application directory.
If SQLite deletion fails, the marker blocks cache restoration across restarts. Only a
successfully committed fresh live snapshot clears it. A changed database mode does not
prevent deletion through the already-open connection. If deletion and marker persistence
both fail, the launcher warns: it cannot guarantee durable revocation or local erasure.
These are logical cache protections, not secure erasure of SQLite free pages or backups.

The cache and private checkouts are **not encrypted**. Their confidentiality relies on local
account permissions and disk protection such as FileVault; backups may retain old data.
Private run output is not written to launcher SQLite or diagnostic logs, and raw private
output is not retained as runtime event history. Advisory bodies are not diagnostic log data.
This does not make the backend or harness
memory-only: its own transcripts, sessions, and history may persist. Private checkouts use `0700`
directory permissions; this is access control, **not encryption**. Private checkouts
remain on disk. The selected harness/model provider receives the confidential advisory
and private code and may be a cloud service. Review its privacy and retention policy
before consenting. Harnesses can write their own sessions/history and other local
copies. Git remote checks and hooks are defense in depth, **not an OS sandbox**; an
agent with the user's permissions can bypass them or disclose data through other tools.
Do not automatically publish this private worktree or treat these protections as a
guarantee against disclosure.

GitHub references:
- [List repository security advisories](https://docs.github.com/en/rest/security-advisories/repository-advisories#list-repository-security-advisories)
- [Create a temporary private fork](https://docs.github.com/en/rest/security-advisories/repository-advisories#create-a-temporary-private-fork)
- [Collaborating in a temporary private fork](https://docs.github.com/en/code-security/security-advisories/working-with-repository-security-advisories/collaborating-in-a-temporary-private-fork-to-resolve-a-security-vulnerability)

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
| `x` | | Confirm worktree deletion (`x worktree`) |
| `Shift+X` (`X`) | | Confirm permanent source issue deletion (`X issue`, Beads issues only) |
| `Esc` | Quit | Return to inbox |
| `Ctrl-C` | Quit | Quit |

Source issue deletion is separate from worktree deletion. The confirmation captures the exact
issue key, identifier, title, provider, host, and repository, even across refreshes. Review the
full target and warnings, then press `Enter` once to submit or `Esc` to cancel. If the terminal
cannot display all warnings, Enter is disabled until resized. While deletion is pending,
the modal blocks further input and duplicate submissions until the runtime replies.
Deletion is permanent: it removes dependency links, updates references, and orphans dependents.
Worktrees and run history are **not deleted**. Active or resumable runs block source deletion;
resolve those runs first. GitHub, GitLab, and PR deletion are unsupported; use the provider's
own tools instead. The launcher never substitutes closing an issue for deletion.

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
