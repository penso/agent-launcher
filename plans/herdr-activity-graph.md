# Herdr Activity Graph: Research and Implementation Handoff

## Implementation Status (2026-09-12)

The user subsequently authorized implementation. The polling rollout is implemented:
separate core telemetry/configuration types, runtime-owned read-only local/SSH
discovery and bounded polling, aggregate SQLite history, and a freshness-aware
working-count graph. Launcher run controls and explicit demo behavior remain separate.
See README's Herdr Activity section and `config.example.toml` for shipped coverage.

Verification: all 274 workspace tests, `just format-check`, `just lockfile-check`,
`just lint`, and `git diff --check` passed. Tests use sanitized fixtures/fake
executables for discovery, SSH safety, scheduling, failure recovery, persistence,
clock rollback, shutdown, and rendering. A read-only local smoke check confirmed
the discovery and agent-list schemas (one running session, three recognized agents).
No live remote SSH or interactive GUI verification was performed.

Remote host-wide discovery remains opt-in. Explicit XDG namespaces are supported;
arbitrary custom socket paths are not. There is no always-on collector, streaming,
lossless transition history, or automatic physical-host alias detection. Clock
rollback invalidation becomes durable when its database reset commits; a crash
before that point cannot retroactively establish the correct clock history.

The original research and authorization record follows below.

## Scope and Authorization

This document records read-only research and a proposed implementation plan. The
user authorized this plan file only, NOT telemetry implementation. Layout changes
were developed alongside this plan: do not overwrite or revert unrelated changes.
Re-read relevant files before implementing because the references below describe
the research-time worktree, not a frozen commit.

Requested outcome: replace the non-demo top activity graph's launcher-only,
synthetic score with useful real Herdr activity across local and remote hosts,
including agents not launched by launcher. Preserve the existing explicit demo
mode. Do not mix demo data into live telemetry or silently fall back to demo.

## Decision

A trustworthy sampled graph of all configured, reachable Herdr sessions is
achievable on Herdr 0.9.0 without modifying Herdr. An unconditional, lossless,
accurately timed history of ALL agents on ALL hosts is not available from the
existing API.

The implementation should promise:

- Explicitly defined host/session coverage, independent of the selected issue,
  repository, pane, or currently presented remote machine.
- Current working-agent counts and separate attention/idle/done/unknown counts.
- Freshness, stale endpoints, exclusions, and observation gaps made visible.
- Persisted observations, not fabricated historical agent transitions.
- Read-only collection with no attach, server startup, upgrade, or bootstrap.

It must not promise complete discovery of unconfigured hosts, inaccessible users,
custom socket namespaces, undetected processes, or transitions during outages.

## Evidence and Verification Level

Verified locally:

- Installed `herdr --version` printed `herdr 0.9.0`.
- Read-only CLI help was inspected for API, agent, session, and machine commands,
  including `agent list`, `api snapshot`, `session list`, and `machine list`.
- Launcher source was read in the current workspace.
- `/Users/penso/code/herdr` was not available; a targeted search did not locate
  the requested checkout.

Verified by public source inspection:

- Herdr source and documentation linked below are pinned to the `v0.9.0` tag.
- Documentation under `docs/next` is still the file at that tag, not today's
  moving website. Implementation source is the stronger evidence where they
  differ.

NOT live tested:

- No live agent, machine, session, snapshot, terminal, or credential inventory
  was printed or queried for this research.
- No SSH connection, remote discovery, socket subscription, disconnection test,
  authentication test, or performance benchmark was performed.
- Local installed version does not establish versions of running local or
  remote servers. Remote CLI command composition below is a proposed transport
  approach, not a verified installed remote feature.

Research performed on 2026-09-11. No application code was changed for this plan.

## Pinned Herdr Sources

Use these IDs throughout the document:

- H1: [Socket API documentation](https://github.com/herdrdev/herdr/blob/v0.9.0/docs/next/website/src/content/docs/socket-api.mdx)
- H2: [App session snapshot](https://github.com/herdrdev/herdr/blob/v0.9.0/src/app/api/session.rs)
- H3: [Agent schema](https://github.com/herdrdev/herdr/blob/v0.9.0/src/api/schema/agents.rs)
- H4: [Event schema](https://github.com/herdrdev/herdr/blob/v0.9.0/src/api/schema/events.rs)
- H5: [Internal event hub](https://github.com/herdrdev/herdr/blob/v0.9.0/src/api/event_hub.rs)
- H6: [Subscription implementation](https://github.com/herdrdev/herdr/blob/v0.9.0/src/api/subscriptions.rs)
- H7: [Socket server](https://github.com/herdrdev/herdr/blob/v0.9.0/src/api/server.rs)
- H8: [Client endpoint agent aggregation](https://github.com/herdrdev/herdr/blob/v0.9.0/src/client/shell/endpoint_agents.rs)
- H9: [Saved endpoint catalog](https://github.com/herdrdev/herdr/blob/v0.9.0/src/client/endpoint/catalog.rs)
- H10: [Client endpoint health](https://github.com/herdrdev/herdr/blob/v0.9.0/src/client/endpoint/health.rs)
- H11: [Terminal title synchronization](https://github.com/herdrdev/herdr/blob/v0.9.0/src/app/terminal_titles.rs)

## Current Launcher Findings

All paths here are repository-relative. Research workspace:
`/Users/penso/.herdr/worktrees/agent-launcher/feat-pr-tabs-reviews`.

| Reference | Research-time behavior |
| --- | --- |
| `crates/runner/src/herdr.rs:29-54` | Backend config contains executable only; commands use the existing command runner. |
| `crates/runner/src/herdr.rs:149-161` | Detection returns no compute targets; no all-agent or remote discovery. |
| `crates/runner/src/herdr.rs:204-230` | Dispatch registers launcher-owned runs; workspace host is `None`. |
| `crates/runner/src/herdr.rs:240-280` | Refresh gets a registered agent by name and reads its visible output; no explicit host/session routing. Get errors become disconnected; successful refresh uses `Utc::now()` as `updated_at`. |
| `crates/runner/src/herdr.rs:418-447` | Existing dispatch adapter requires compatible matching client/server versions and protocols. This is stricter than the documented JSON API compatibility policy. |
| `crates/runner/src/herdr.rs:496-503` | `working -> Running`, `blocked -> NeedsInput`, `idle -> Idle`, `done -> Completed`, all other statuses -> `Disconnected`. |
| `crates/runner/src/herdr.rs:590-599` | Reads up to 240 visible terminal lines. |
| `crates/runtime/src/service.rs:29` | Launcher run polling interval is two seconds. |
| `crates/runtime/src/service.rs:397` | Runtime loads launcher runs from its store. |
| `crates/runtime/src/service.rs:909-955` | Generates launcher events for state/message changes and changed output snapshots. These are not original Herdr events. |
| `crates/tui/src/activity.rs:23-66` | Reconstructs partial 15-minute history from persisted launcher events and run completion timestamps. |
| `crates/tui/src/activity.rs:78-172` | Counts snapshot runs; scores working, attention, idle, new events, and completions; caps at 100; peak-downsamples a 15-minute window. |
| `crates/tui/src/event_loop.rs:142-169` | Samples the runtime snapshot into activity history. |
| `crates/tui/src/render.rs:210-269,315` | `AGENT_LAUNCHER_DEMO_ACTIVITY` presence explicitly selects generated demo data and demo labeling. Preserve this opt-in behavior. |
| `crates/store/src/lib.rs` | Existing SQLite store is the natural place to investigate minimal sample persistence, without inserting external agents into the run tables. |
| `crates/core/src/runtime.rs`, `crates/core/src/config.rs` | Inspect for a separate telemetry snapshot/config extension. |

The score adds 45 per working run, 12 per attention run, 8 per needs-input run,
4 per idle run, up to 40 for new events, and up to 40 for completions, capped at
100. Three working agents saturate the scale. It is not work rate, utilization,
token throughput, or a count graph. Reading visible terminal redraws can create
activity noise. Refresh timestamps do not establish actual completion times.

Do not feed external agents through this adapter's `RunState` mapping merely to
reuse its graph. In particular, Herdr `done` and `unknown` have different meanings
from launcher `Completed` and `Disconnected`.

## API Scope: One Session, Not the Federated UI

H2 builds `session.snapshot` from one `App`'s state: focused IDs, workspaces,
tabs, panes, layouts, and `collect_agent_infos()`. It does not traverse SSH
endpoint caches. H8 aggregates multiple endpoint snapshots in the CLIENT UI,
using endpoint identity and marking stale rows. That federation is not exposed
as an all-endpoints collection in the inspected public socket API.

H9 saved SSH profiles contain `id`, `label`, `target`, `session`, and `enabled`.
One profile means one target/session, not every session on that host. Duplicate
target/session profiles with different opaque IDs are explicitly allowed. The
catalog permits at most 64 profiles; that is not a global agent limit.

Therefore launcher needs its own endpoint inventory and per-endpoint collector.
Do not scrape Herdr's UI or read its private client cache as the primary API.
Do not assume whichever endpoint the user has selected determines API routing.

## Exact Read Interfaces

These help/version commands are safe local metadata checks and were inspected:

```sh
herdr --version
herdr --help
herdr api --help
herdr agent --help
herdr session --help
herdr machine --help
herdr agent list --help
herdr api snapshot --help
herdr session list --help
herdr machine list --help
```

Documented collector commands below return potentially sensitive live inventory.
They are examples for a future authorized implementation, NOT instructions to
print live output during this plan-only task:

```sh
herdr session list --json
herdr machine list --json
herdr --session <name> agent list
herdr --session <name> api snapshot
herdr --session <name> status --json
herdr api schema --json
```

`agent list` and `api snapshot` already print JSON; installed help exposes no
`--json` option for them. `session list` and `machine list` do expose `--json`.
Use bundled schema output, not `--output`, when inspecting without writing files.
Inspect actual version-specific session/machine response shapes before coding
their parsers; this research did not inspect live responses or establish all
their JSON fields. In particular, do not guess a session-list running-state field.

Raw transport is newline-delimited JSON on a Unix domain socket (named pipe on
Windows). Use a new connection for each ordinary request; H7 reads one initial
request, responds, and finishes. Subscriptions retain their connection.

```json
{"id":"health","method":"ping","params":{}}
{"id":"agents","method":"agent.list","params":{}}
{"id":"snapshot","method":"session.snapshot","params":{}}
{"id":"agent","method":"agent.get","params":{"target":"w1:p1"}}
{"id":"explain","method":"agent.explain","params":{"target":"w1:p1"}}
```

Successful responses have `id` and `result`; errors have `id` and
`error: {code, message}`. Snapshot data is under `result.snapshot`; agent list
uses its collection response, and agent get uses `result.agent`. Confirm exact
list discriminator/shape against the bundled schema when implementing. Treat
an API error object as failure even if the transport process exits successfully.

H1 socket resolution order:

1. Explicit CLI `--session <name>`.
2. `HERDR_SOCKET_PATH`.
3. `HERDR_SESSION`.
4. Default session socket.

Documented Unix examples:

```text
~/.config/herdr/herdr.sock
~/.config/herdr/sessions/<name>/herdr.sock
```

Resolve paths using the applicable installation/config context, not hardcoded
home paths. `HERDR_CONFIG_PATH` is also a documented config override. Discover
the default session as well as named sessions; custom config/socket namespaces
need explicit configuration and are otherwise out of coverage.

When invoking collectors, clear inherited `HERDR_SOCKET_PATH` and
`HERDR_SESSION`, then explicitly select the intended session, or use an explicit
validated socket path for raw requests. Deliberately choose the config namespace
instead of inheriting an unrelated pane's `HERDR_CONFIG_PATH`. Preserve ordinary
SSH authentication environment where appropriate; do not indiscriminately clear
`SSH_AUTH_SOCK`. Never use active-pane defaults for inventory.

For already-running remote sessions, the proposed transport is authenticated
OpenSSH remote execution of the same CLI commands, or a protected SSH Unix-socket
forward for raw API access. For a validated SSH alias, a conceptual query is:

```sh
ssh -o BatchMode=yes -o ConnectTimeout=5 <validated-host-alias> \
  'env -u HERDR_SOCKET_PATH -u HERDR_SESSION herdr --session <validated-session> agent list'
```

This is a template, not copy/paste with untrusted substitutions. The implementation
must validate targets and correctly quote remote-shell arguments. Local argv
construction alone does not prevent SSH remote-command shell injection. Support
Herdr's target syntax deliberately, including user/port/URI forms; do not assume
a Herdr profile target is directly usable as an arbitrary `ssh` argument.
Validate remote config namespace and executable discovery separately.

`herdr --remote <target>` is documented as app attach, NOT a general read-only
API forwarding switch. Never use it for the collector. Never call `machine add`,
session attach, server start, update, handoff, or integration installation.

## State and Identity Semantics

H3 `AgentInfo` includes:

- `terminal_id`, optional `name`, optional `agent`.
- `workspace_id`, `tab_id`, `pane_id`, `focused`.
- `agent_status`, `launch_pending`, `interactive_ready`.
- `state_change_seq`, `revision`.
- Optional `agent_session` with source, agent, kind, and value.
- Optional title/presentation fields, tokens, cwd, and foreground cwd.

There is no authoritative start, finish, or last-state-change wall-clock timestamp
in `AgentInfo`. `state_change_seq` is not a public global replay cursor, timestamp,
or quantity of completed work. `revision` is not a semantic activity count.

H1 effective statuses:

| Herdr status | Collector meaning |
| --- | --- |
| `working` | Herdr currently classifies this agent as working. |
| `blocked` | Agent needs interaction; not endpoint health failure. |
| `idle` | Agent is idle. |
| `done` | Idle and not yet seen; not task success or permanent run completion. |
| `unknown` | Herdr cannot classify semantic state; not disconnected. |

Keep `done` separate, optionally labeling it `unseen done`; if attention is
defined as blocked plus unseen done, say so and expose the components. An agent
may move from done to idle because the result was seen, not because new work ran.
Future status strings must not break the whole endpoint: retain the raw status
or classify as unknown without inventing transport failure.

Detection depends on supported agent integrations and/or detection rules.
`agent.explain` exposes source/rule evidence, but may include sensitive metadata;
use only during explicit diagnostics. An inventory covers Herdr-recognized
agents, not every arbitrary process on the host. H1 explicitly excludes popup
terminals from pane and agent APIs.

Use canonical endpoint/session plus `terminal_id` for current-record identity.
Public pane IDs can change on cross-workspace moves (H1/H4); names are optional
and mutable. Track pane location separately. Terminal identity also does not
prove a single task or uninterrupted agent process: replacement/restart must not
be interpreted as continuation of historical work. Native agent-session IDs are
optional and need not be collected for count telemetry.

Do not use titles as a clock: H11 stores raw OSC title changes but emits
`pane.updated` only when stripped title changes. Spinner-only raw title changes
can emit nothing. Title metadata is independent of semantic status and ephemeral
across cold restart.

## Polling, Subscriptions, and History Limits

`agent.list` and `session.snapshot` return full current collections. The inspected
interfaces do not provide pagination/cursors for these inventories. Large sessions
therefore require bounded full-response handling; a truncated or oversized response
must be a failed sample, never an apparently successful partial inventory.

Streaming exists, but is not a lossless audit log:

- H4 `events.subscribe` takes `{subscriptions: [...]}`. Global lifecycle
  subscriptions include pane created/updated/closed/moved/exited/detected and
  workspace/tab lifecycle events.
- `pane.agent_status_changed` requires a specific `pane_id`; the inspected
  schema does not provide a wildcard all-pane status subscription.
- Example with a known pane:

```json
{"id":"sub","method":"events.subscribe","params":{"subscriptions":[{"type":"pane.created"},{"type":"pane.closed"},{"type":"pane.moved"},{"type":"pane.agent_detected"},{"type":"pane.agent_status_changed","pane_id":"w1:p1"}]}}
```

- Acknowledgement has `result.type = "subscription_started"`; later lines are
  pushed events, not request responses.
- H4 lifecycle `EventEnvelope` contains `event` and `data`, without an exposed
  timestamp or sequence. Its event kind uses snake_case; special subscription
  envelopes use dot-name kinds such as `pane.agent_status_changed`. Parse both
  documented shapes, not a single guessed universal event-name convention.
- H5 retains only 512 events in memory with an INTERNAL sequence.
- Lifecycle subscriptions do not replay events before acceptance. No public
  resumable cursor, event-history pagination, or durable historical query was
  found in these interfaces.
- H7 checks subscriptions every 100 ms; H6 returns at most one matching event
  per subscription per pass. Bursts, slow readers, and the finite hub can lose
  history. Do not advertise exactly-once or lossless delivery.
- Different subscriptions advance independently and are polled in subscription
  order. Wire order across event kinds is not a guaranteed original global order.
- Status subscriptions can emit presentation-only changes. Compare semantic
  fields; do not count every notification as work or a status transition.
- `pane.output_matched` reports pattern matches, not every byte or agent action.
  `events.wait`/`agent.wait` are one-shot coordination helpers, not historical
  queries. Agent waits observe semantic state, not arbitrary task completion.

H1 bootstrap guidance is subscribe, await acknowledgement, buffer stream, request
snapshot on another connection, install snapshot, reconcile buffered events, then
continue. Re-snapshot after reconnect. This reduces setup gaps, but absence of a
shared public cursor prevents a simple lossless snapshot/event boundary proof.
Do not blindly replay buffered updates over newer snapshot facts. Use available
identity/revisions, idempotent reconciliation, and a follow-up snapshot if order
is ambiguous. Never manufacture historical spikes from bootstrap replay.

Per-pane status subscriptions require discovery first. If streaming is later
added, establish global lifecycle subscriptions before bootstrap, discover panes,
install/rebuild status subscriptions, and perform a reconciliation snapshot after
setup. Reconcile moved/new/removed panes continuously; do not assume a static
subscription set covers future agents. Polling remains the authoritative repair
path even when streaming improves latency.

Terminal scrollback is not timestamped semantic history. Launcher must persist
observed samples for restart continuity. Offline periods and the pre-collector
past cannot be backfilled as actual working counts from a present-day snapshot.

## Health, Compatibility, and Security

H10 client federation probes after five quiet seconds, with a ten-second probe
timeout; an initial snapshot must also arrive within ten seconds. H8 dims stale
cached agents. This is client endpoint health, not `agent_status` returned by the
local app API. A collector must implement its own liveness/freshness model.

H7 `ping` can reply in the socket server without dispatching through the App.
Therefore a successful ping is not enough to prove the agent inventory is fresh
or the App responsive. Successful inventory responses establish observation
freshness; transport probes are supplemental. Use client-side deadlines because
ordinary App dispatch is not universally protected by a response timeout.

H7 restricts Unix sockets to mode 0600. The inspected protocol has no token login
or read-only permission scope: access rests on OS/socket permissions. Anyone
given socket access can potentially issue control methods. SSH must authenticate
as an account allowed to access the remote socket. Preserve host-key verification;
never use `StrictHostKeyChecking=no` or unauthenticated TCP socket exposure.

Saved profiles contain configuration, not passwords/private keys or proof of
reachability. Authentication failures, host-key failures, disabled profiles,
missing executable, stopped server, malformed data, and unsupported API methods
must remain visible as distinct endpoint reasons with sanitized messages.

Use an allowlist of read methods/commands. Parse required telemetry fields and
discard unnecessary titles, cwd, tokens, native session IDs, and terminal text.
Do not log full JSON, remote command output, credentials, or user prompts. A
snapshot still transmits extra metadata even if discarded; prefer agent list
where sufficient and avoid output reads entirely for the graph.

H1 says JSON clients should ignore unknown fields and handle unsupported methods
normally. The federated binary endpoint protocol negotiates codecs/capabilities;
saved federation requires `surface_interest` and `health_check`. Do not mistake
those UI requirements for a mandatory version-equality rule for read-only JSON
collection. Check per-endpoint capabilities and required response fields. Do not
change existing dispatch validation merely to implement a read-only collector.

## Recommended Minimal Architecture

### Discovery and Coverage Contract

Default coverage should include the default and discovered named local sessions,
plus enabled saved remote target/session profiles and explicit user-configured
endpoints. To fulfill the broader all-sessions-on-known-hosts goal, add read-only
remote `session list --json` discovery once per canonical authorized host, then
query each discovered running session. A saved profile alone does not establish
that all sessions on its host are covered.

Make remote host-wide discovery an explicit documented setting/consent boundary:
querying another session is broader than querying a saved profile's session.
Report `configured sessions` coverage if it is off; only claim discovered-session
coverage when enumeration succeeded. Disabled profiles must not be re-enabled
indirectly by another discovery pass without a clear inclusion policy. A failed
remote session enumeration means completeness is unknown even if one known
session remains reachable.

Deduplicate identical normalized target/account/port/session/config namespaces.
Do not key solely by profile ID, label, host name, or pane ID. H9 permits duplicate
profiles. SSH aliases can identify the same physical endpoint, and no stable
global server identity was established by this research. Support explicit
canonical endpoint IDs/alias grouping; do not claim perfect automatic alias
deduplication from host strings or equal terminal IDs. Local and SSH loopback
aliases likewise require canonicalization. Distinct users or config namespaces
may legitimately be different endpoints on one machine.

Reconcile discovery periodically (suggest 60 seconds) and on explicit refresh.
An enumeration failure must not delete all previously known endpoints. Mark
inventory completeness unknown. Endpoint removals or disabled settings are
configuration changes, not agent completions.

### Collector and Scheduling

Add a read-only telemetry path separate from `Backend::refresh` and launcher
`RunSummary`. A small runtime-owned collector plus separate telemetry snapshot is
sufficient; avoid a new daemon, broad adapter rewrite, or fake issue/run records.

Suggested initial defaults, to validate with fixture/load tests:

- Poll each enabled endpoint's full `agent.list` every 2 seconds.
- Use `session.snapshot` for bootstrap/repair where richer identity/location data
  is needed; prefer the lighter full agent list for count-only reconciliation.
- Limit concurrent endpoint requests to 4, configurable if justified. Never
  overlap requests for the same endpoint; skip missed ticks instead of catching up.
- Use 5-second SSH connection and 10-second end-to-end request deadlines. Ensure
  timeout/cancellation terminates child processes rather than leaking SSH clients.
- Use retry delays of 2, 4, 8, 16, 32, then 60 seconds with jitter after failures;
  reset on successful inventory. Explicit refresh may request an earlier retry,
  but must not bypass concurrency limits or pile up duplicate work.
- Mark never-observed endpoints unknown immediately. Treat observations older
  than 10 seconds as stale by default. Record transport failure immediately even
  while a recent observation remains within its age budget; show both freshness
  and connection state rather than labeling it unconditionally online.
- Sample graph buckets on a fixed 2-second collector schedule, not redraw rate.
  Store actual observation/sample times; queue delay is not agent activity time.
- Bound response sizes and collection memory. Reject over-limit responses in full
  and show an explicit reason; never silently truncate the agent population.

These are proposed defaults, not measured Herdr guarantees. With many endpoints,
four requests and slow transports can exceed the freshness budget. Fair scheduling
and the coverage label must reveal that; benchmark before tuning. A dead host
must not block healthy hosts, rendering, or launcher dispatch.

### Suggested Minimal Data Model

The following is a design sketch, not an instruction to add all fields/tables
without reviewing existing store conventions. Keep aggregate samples durable;
raw per-agent observations can remain memory-only to reduce privacy and schema
cost. Do not persist terminal output for this feature.

```text
EndpointDescriptor (configured/discovered inventory)
  endpoint_id              canonical collector ID, not a display label
  host_id                  explicit canonical account/host identity
  session                  explicit session name
  config_namespace         explicit logical namespace/socket context
  enabled, discovery_source

EndpointObservation (memory)
  endpoint_id
  connection_generation    local generation; not claimed server restart identity
  attempted_at, last_success_at
  transport_state          connecting | reachable | failed | disabled
  freshness                never_observed | fresh | stale
  error_kind               sanitized reason code
  discovery_complete       enumeration coverage, not just query success
  agents[terminal_id]       raw status, pane/workspace/tab IDs as needed,
                           state_change_seq, revision, observed_at

ActivitySample (persisted, one aggregate row per scheduled bucket)
  sampled_at_unix_ms        primary key or unique bucket key
  working_count
  blocked_count
  idle_count
  unseen_done_count
  unknown_agent_count
  expected_endpoints
  fresh_endpoints
  stale_endpoints
  never_observed_endpoints
  inventory_complete
  completeness             complete | partial | missing
```

An existing SQLite migration can add one aggregate sample table. Add a small
per-endpoint sample table only if per-host history/debugging is actually needed;
it is not necessary for the top aggregate graph. Store only non-sensitive counts
and coverage. Define version/migration behavior using existing store conventions.
No external telemetry records belong in tables used for launcher stop/delete.

Counts include only fresh successful endpoint observations. Partial samples carry
known counts plus incomplete coverage; they are a lower bound, not the global
total. Missing samples have no valid count (nullable values or an explicit
validity flag), NOT a synthetic count of zero. An actually complete empty
inventory is a valid zero. Distinguish no configured endpoints from a verified
empty set of successfully enumerated sessions.

Persist at least the 15-minute graph window, with a small retention margin
(suggest 30 minutes) and bounded pruning. Load persisted samples at startup,
retaining their original timestamps and completeness. No TUI history rebuild
from `RunSummary.updated_at`, output events, or current agent counts. If the
collector only runs while launcher runs, launcher downtime is an honest gap;
an always-on service is a separate product decision, not part of this minimal plan.

Use monotonic time for in-process deadlines/freshness and UTC for persistence.
Handle wall-clock jumps and future-dated rows without filling the past or
extending live freshness. Remote event receipt time is observation time; it is
not a remote transition timestamp. On connection generation changes, reset
ephemeral event assumptions and replace inventory after a successful full query.

### Non-Demo Graph

Plot working-agent COUNT over the existing 15-minute horizon, not the 0-100
synthetic score. Label units and scale. Use a sensible count axis (at least one;
based on visible-window maximum, with stable behavior) so more than two working
agents remain distinguishable. Peak downsampling is acceptable only when labeled
as a peak-per-bucket display; do not present it as an average or integral.

Show current counters and coverage, for example:

```text
7 working | 2 blocked | 1 unseen done | 3 idle | 1 unknown
4/5 sessions fresh | 1 stale | configured-session coverage
```

Do not let failed endpoints keep contributing stale `working` counts forever.
Do not render their removal from confirmed totals as a confident drop in all-host
activity. Draw partial/missing buckets differently (gap, muted/hatched marker,
or other accessible distinction), and annotate incomplete coverage. If existing
sparkline widgets cannot represent missing data, extend the rendering model rather
than coercing `None` to zero. Do not interpolate across missing intervals.

The collector aggregates all included Herdr agents independent of launcher
ownership. If other backends also appear in the UI, label this graph's scope
explicitly as Herdr rather than silently implying all backends are covered.
Keep launcher run controls and their current behavior separate.

Preserve `AGENT_LAUNCHER_DEMO_ACTIVITY` as the explicit demo opt-in. Existing
implementation selects demo when the variable is PRESENT, including an empty
value; do not accidentally alter that shipped behavior while replacing real
data. Keep the demo label and existing demo rendering tests. Demo must never
write fake samples to the real store, count toward coverage, or replace a failed
collector automatically. Unset the variable for live-mode tests.

## Rollout Tasks

1. Re-read the concurrent layout work and current runtime/store/config interfaces.
   Agree on the telemetry snapshot boundary with that agent; preserve all unrelated
   modifications. Confirm graph units/coverage placement without taking over layout.
2. Inspect bundled 0.9.0 schema and pinned session/machine CLI source to finalize
   exact discovery JSON parsers, default-session naming, config resolution, and
   raw response shapes. Create sanitized fixture data; do not commit live dumps.
3. Introduce explicit endpoint descriptors and configuration policy for local
   sessions, saved profiles, host-wide remote enumeration, exclusions, and aliases.
   Document the precise meaning of the coverage label.
4. Implement read-only local discovery and agent-list polling behind the telemetry
   path. Parse full snapshots atomically; test unknown fields and future statuses.
5. Add secure SSH transport and remote session discovery with deadlines, bounded
   concurrency, fair scheduling, backoff, cancellation, and sanitized errors.
   Treat unavailable/stopped servers as unavailable; never start them.
6. Add freshness-aware aggregation and deterministic sample bucketing. Keep
   disconnected endpoints, semantic unknowns, and unseen-done agents distinct.
7. Add minimal sample persistence and pruning. Recover old samples without
   fabricating downtime or completion history. Keep database writes out of the
   render loop.
8. Wire the non-demo count graph and coverage counters into the agreed layout.
   Remove the real graph's dependence on synthetic run-event scores without
   changing launcher run actions. Preserve explicit demo behavior.
9. Run fixture-based end-to-end tests and repository checks. Perform authorized
   manual local/remote verification without exposing live payloads in logs.
10. Only if needed after polling is measured, add subscriptions as a latency
    optimization with snapshot repair. Do not require streaming for first rollout.

## Tests and Acceptance Criteria

### Discovery and Routing

- Default plus multiple named local sessions are included even if launcher was
  launched inside a different Herdr pane/session. Selected UI machine, issue
  filters, and repository filters do not change global telemetry coverage.
- Duplicate saved profiles for the same canonical target/session count once.
  Explicit SSH alias grouping and local/loopback aliases do not double-count.
  Distinct users, sessions, and config namespaces remain distinct.
- Host-wide discovery includes additional authorized remote sessions when enabled;
  when disabled, label configured-session coverage, never all-host coverage.
- Disabled/excluded endpoints do not receive queries. Discovery failure retains
  previous inventory with incomplete coverage instead of deleting it.
- Environment poisoning tests set `HERDR_SOCKET_PATH`, `HERDR_SESSION`, and an
  unrelated config override; every request still reaches the explicit fixture
  endpoint. Preserve intended SSH agent access.

### Transport and Safety

- Fake CLI/SSH executables assert only allowlisted read commands are invoked.
  No attach/start/update/bootstrap/focus/input/read-terminal command is called.
- Spaces, quotes, shell metacharacters, option-like targets, URI ports, and IPv6
  are handled or rejected safely. No shell injection; host-key checks stay on.
- Timeout, cancellation, process exit, malformed JSON, error response, oversized
  response, missing method, and permission/authentication failures produce
  sanitized endpoint errors with no partial-success inventory.
- Four-request bound and no per-endpoint overlap hold under load. One hung host
  cannot starve healthy endpoints or freeze the TUI. Backoff resets after success.
- No fixture secrets, titles, cwd, session reference values, or terminal text are
  persisted/logged by the count collector.

### Semantics and Aggregation

- Agents created outside launcher appear; no issue/run record is manufactured.
- All five Herdr statuses remain distinct. `done -> idle` is not a task completion
  event; `unknown` is not disconnected. Presentation-only changes do not raise
  working counts or event scores.
- Identical terminal IDs on different canonical endpoints do not collide. Pane
  moves preserve current identity; replacement/restart does not invent task
  continuity or duplicate current counts.
- Healthy agent lists returning zero agents are valid zero samples. Failed lists,
  never-observed endpoints, and collector downtime are not valid zero history.
- Counts include only fresh observations; stale cached working agents are not
  silently counted as still working. Partial totals clearly show missing coverage.
- Ping success with a stuck inventory response does not mark inventory fresh.
- Polling can miss a short working burst; tests/documentation explicitly avoid
  promising transition completeness. No historical interpolation hides that limit.

### Persistence and Rendering

- Restart restores original 15-minute sample times/counts/coverage; downtime stays
  a gap. Retention pruning is bounded and does not delete unrelated run history.
- Duplicate bucket writes are deterministic. Clock jumps and future timestamps
  do not panic, invent backfill, or retain stale agents as fresh indefinitely.
- Counts of 0, 1, 3, and large fleets produce interpretable count-scale output,
  not a capped 100-point score. Narrow widths and empty history remain legible.
- Missing and partial buckets are visually distinguishable from valid zero; labels
  retain coverage even when counters must be abbreviated.
- Demo remains explicit and labeled. With the environment variable unset,
  fixture-based live telemetry is used. With it present, existing demo behavior
  works and no synthetic sample is stored as real telemetry.
- Existing layout, keyboard/mouse behavior, launcher dispatch/stop/delete, and
  unrelated backend behavior regressions are covered by the existing suite.

### Optional Streaming

- Test acceptance/acknowledgement before bootstrap, events during snapshot setup,
  duplicate/presentation events, out-of-order event kinds, pane creation/move/close,
  and rebuilding per-pane subscriptions.
- Simulate more than 512 retained events, a slow consumer, and reconnect. Repair
  current state by snapshot; never claim recovered missing historical transitions.
- Periodic inventory reconciliation survives a silent stream and a responsive ping
  with an unresponsive App. Streaming errors cannot disable fallback polling.

## Verification Commands for the Implementing Agent

Repository checks from the workspace root (these implementer checks were NOT run
for this documentation-only task):

```sh
git status --short
git diff --check
just format-check
just lockfile-check
just lint
just test
```

The current `justfile` pins `nightly-2025-11-30`. Equivalent full test command:

```sh
cargo +nightly-2025-11-30 test --workspace
```

Add deterministic fixture tests to the appropriate crates and run their focused
test names before the full suite. Use fake transports or temporary test sockets;
normal CI must not depend on the user's live Herdr installation or SSH access.

For an authorized manual verification, first inspect schema/help and server
compatibility without dumping live inventory. Run the application in non-demo mode:

```sh
env -u AGENT_LAUNCHER_DEMO_ACTIVITY \
  cargo +nightly-2025-11-30 run -p agent-launcher-cli
```

Then verify counts against privately observed local and remote Herdr sessions,
including an agent not launched by launcher, a named session, and a disconnected
host. Report only aggregate counts/coverage and sanitized outcomes. Do not change
live agents merely to exercise a graph; use disposable authorized sessions for
state transitions. A planned SSH query failing due to access is a coverage gap,
not justification to bootstrap or relax security.

Documentation-only verification for this handoff:

```sh
git diff --check -- plans/herdr-activity-graph.md
git status --short -- plans/herdr-activity-graph.md
```

For an untracked new file, `git diff --check` alone does not inspect its contents;
also read the file or check its lines directly before handing off. Do not stage,
commit, or modify other agents' files as part of this plan request.

## Remaining Gaps and Future Upstream Work

Before promising more than observed current-state counts, Herdr would need a
stronger telemetry contract: authoritative timestamps, durable replay/cursors,
explicit stream gap signaling, stable server/session incarnation identity, and
possibly a public federated inventory/health API. Complete unknown-host discovery
and access across users are separate authorization and deployment problems.

Polling plus persisted observations and honest coverage is the recommended first
implementation. Streaming is optional. Lossless cross-host history and an
always-on collector are explicitly outside this minimal scope.
