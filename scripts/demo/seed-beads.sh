#!/usr/bin/env bash
# Creates a throwaway Git repository with a Beads database full of realistic
# issues, for demos and UI screenshots of the launcher.
#
#   scripts/demo/seed-beads.sh /tmp/launcher-demo
#
# The directory is recreated from scratch. Pass a HOME as BD_HOME to keep bd's
# own state out of your real home directory.
set -euo pipefail

repo=${1:?usage: seed-beads.sh <directory>}
case "$repo" in
/ | "$HOME" | "$HOME/") echo "refusing to recreate $repo" >&2; exit 1 ;;
esac

rm -rf "$repo"
mkdir -p "$repo"
cd "$repo"
git init -q -b main
git config user.name "Demo User"
git config user.email "demo@example.com"
cat > README.md <<'EOF'
# Orbit

A small sync service used to demo agent-launcher.
EOF
git add README.md
git commit -q -m "Initial commit"

export BD_NON_INTERACTIVE=1
bd init -q --prefix orb >/dev/null 2>&1 || bd init --prefix orb >/dev/null

# create TYPE PRIORITY LABELS TITLE <<BODY ; prints the new ID.
create() {
  local type=$1 priority=$2 labels=$3 title=$4
  bd create --silent -t "$type" -p "$priority" -l "$labels" --title "$title" --body-file -
}

epic=$(create epic 1 sync,roadmap "Offline-first sync for the desktop client" <<'EOF'
## Goal

Let people keep working when the network drops, and reconcile cleanly once it comes back. Today every edit is a round trip, so a flaky connection freezes the editor for seconds at a time.

## Scope

- A local write-ahead queue that survives restarts
- Conflict resolution for concurrent edits to the same document
- A status indicator so people know what has not synced yet

## Out of scope

Real-time collaboration cursors; that is tracked separately.
EOF
)

queue=$(create feature 1 sync,storage "Persist the outgoing change queue in SQLite" <<'EOF'
## Summary

Outgoing edits live in memory today and are lost when the app quits while offline. Store them in a `pending_changes` table instead and replay them in order on reconnect.

## Design

- One row per change: `id`, `document_id`, `op` (JSON), `created_at`, `attempts`
- Writes go through a single `ChangeQueue` actor so ordering is preserved
- Replay stops at the first permanent failure and surfaces it in the UI

```sql
CREATE TABLE pending_changes (
  id INTEGER PRIMARY KEY,
  document_id TEXT NOT NULL,
  op TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0
);
```

## Acceptance

- Quit while offline, relaunch, go online: every edit reaches the server once
- `attempts` is capped at 8 with exponential backoff
EOF
)
bd dep add "$queue" "$epic" --type parent-child >/dev/null 2>&1 || true

conflicts=$(create feature 1 sync "Three-way merge for concurrent paragraph edits" <<'EOF'
## Problem

When two devices edit the same paragraph offline, the last write wins and the other edit silently disappears. Users only notice days later.

## Proposal

Keep the base revision with each queued change and run a three-way merge on replay. Fall back to keeping both versions, marked as a conflict, when the merge is ambiguous.

## Notes

- The diff library already supports `merge3`; we only need to keep the base
- Conflicts should be visible in the document list, not only in a log
EOF
)
bd dep add "$conflicts" "$queue" >/dev/null 2>&1 || true
bd dep add "$conflicts" "$epic" --type parent-child >/dev/null 2>&1 || true

create feature 2 sync,ui "Show unsynced changes in the status bar" <<'EOF' >/dev/null
Add a small indicator next to the account name: a dot while changes are queued, a spinner while replaying, and a warning when replay failed.

Clicking it opens a popover listing the queued documents with their age.
EOF

create bug 0 crash,editor "Editor crashes when pasting an image larger than 20 MB" <<'EOF' >/dev/null
## What happens

Pasting a large PNG from the clipboard freezes the editor for a few seconds and then the app quits. Smaller images paste fine.

## Steps to reproduce

1. Copy a 25 MB screenshot to the clipboard
2. Paste it into any document
3. The window freezes, then closes without an error dialog

## Logs

```
thread 'main' panicked at 'capacity overflow', src/clipboard/image.rs:142:18
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
```

## Environment

- Orbit 2.14.1 on macOS 15.3, Apple M3
- Also reproduced on Windows 11 with a 30 MB BMP
EOF

create bug 1 auth "Session expires every hour even with \"Keep me signed in\"" <<'EOF' >/dev/null
Since 2.14 the refresh token is not stored when "Keep me signed in" is checked, so the session ends after the access token's one-hour lifetime.

The keychain entry is written, but with the access token instead of the refresh token. See `auth/session.rs`, `persist_tokens`.
EOF

create bug 2 search "Search ignores accented characters" <<'EOF' >/dev/null
Searching for `cafe` does not find documents containing `café`, and the reverse. We should normalise both the index and the query with Unicode NFKD and strip combining marks before comparing.
EOF

create task 2 docs "Document the sync protocol for third-party clients" <<'EOF' >/dev/null
Write `docs/sync-protocol.md` covering the handshake, change format, cursor semantics and error codes. Two community clients reverse-engineered it and both get cursor resets wrong.
EOF

create feature 3 editor,ui "Keyboard shortcut to move a block up or down" <<'EOF' >/dev/null
Alt+Up / Alt+Down should move the current block (paragraph, list item, or code block) past its neighbour, like most outliners do.
EOF

create chore 3 ci "Cache the Rust toolchain in CI" <<'EOF' >/dev/null
Every CI run downloads the nightly toolchain from scratch, which adds about 90 seconds. Cache `~/.rustup` keyed on `rust-toolchain.toml`.
EOF

create bug 1 sync,storage "Duplicate documents after restoring from a backup" <<'EOF' >/dev/null
Restoring a backup on a second device creates a copy of every document instead of matching them by ID, because the restore path generates fresh IDs.
EOF

create task 2 perf "Profile cold start on Windows" <<'EOF' >/dev/null
Cold start takes 4.8 s on Windows versus 1.2 s on macOS with the same data. Capture an ETW trace and find where the time goes before guessing.
EOF

done1=$(create bug 1 editor "Undo after paste removes two steps" <<'EOF'
Fixed by grouping the paste and its formatting pass into one undo step.
EOF
)
done2=$(create feature 2 ui "Dark mode follows the system setting" <<'EOF'
Shipped in 2.13.
EOF
)
bd close "$done1" "$done2" >/dev/null 2>&1 || true

echo "Seeded $(bd count 2>/dev/null || echo some) issues in $repo"
