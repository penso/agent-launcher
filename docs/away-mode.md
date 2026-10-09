# Manual and Away modes

Dispatch by hand, or let Away work through a prioritized queue while you are gone.

## Manual / Away

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

[← README](../README.md)
