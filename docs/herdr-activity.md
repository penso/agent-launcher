# Herdr activity

The activity graph above the inbox, and the Braille widgets that draw it.

## Herdr Activity

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

## Reusable Braille Widgets

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

[← README](../README.md)
