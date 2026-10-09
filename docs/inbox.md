# The inbox

How the issue, PR and advisory inbox lays out, renders descriptions, and takes the mouse.

## Layout

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

## Preview and mouse

On terminals wide enough (a list area of 140+ columns), the selected row's Overview shows beside
the list. Drag the divider between them to resize both panels, or drag it to the right edge to
close the preview; drag the remaining handle back to reopen it. The size lasts for the session.
Detail tabs take clicks too.

Click a tab to switch between Issues and PRs. Hover a row to select it, then click to open its details. Mouse-wheel
scrolling over the list moves three items at a time; over details it moves three lines. Overlays
block background mouse actions. Opening a PR does not launch a review.

## Description Markdown

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

## Item Activity

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

## Terminal Title

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

## Debug

Debug shows the live snapshot's selected backend and agent, detected worktree manager,
backend availability, compute targets, local repository path and effective host/repository,
source connection messages (including throttling), and last refresh. It does not read
configuration or environment variables or display the raw remote URL. Use arrows,
Page Up/Down, Home/End, or the mouse wheel over the pane to scroll; Esc closes it
without changing the inbox search or selection.

## Deleting source issues

Source issue deletion is separate from worktree deletion. The confirmation captures the exact
issue key, identifier, title, provider, host, and repository, even across refreshes. Review the
full target and warnings, then press `Enter` once to submit or `Esc` to cancel. If the terminal
cannot display all warnings, Enter is disabled until resized. While deletion is pending,
the modal blocks further input and duplicate submissions until the runtime replies.
Deletion is permanent: it removes dependency links, updates references, and orphans dependents.
Worktrees and run history are **not deleted**. Active or resumable runs block source deletion;
resolve those runs first. GitHub, GitLab, and PR deletion are unsupported; use the provider's
own tools instead. The launcher never substitutes closing an issue for deletion.

[← README](../README.md)
