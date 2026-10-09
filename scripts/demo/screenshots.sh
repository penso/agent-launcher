#!/usr/bin/env bash
# Runs the launcher against a seeded Beads demo repository in a private tmux
# server, walks through the main screens, and writes colour captures plus an
# HTML page of them.
#
#   scripts/demo/screenshots.sh [output-directory]   (default: target/demo)
#
# The launcher runs with a throwaway HOME and a minimal environment, so it
# never touches your real launcher state, config, Herdr session or tmux.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
out=$(mkdir -p "${1:-$root/target/demo}" && cd "${1:-$root/target/demo}" && pwd)
home="$out/home"
# Outside the checkout, so captures show a neutral path rather than yours.
repo="${DEMO_REPO:-/tmp/agent-launcher-demo/orbit}"
shots="$out/shots"
socket=agent-launcher-demo
tmux=(tmux -L "$socket" -f /dev/null)

cargo build -q -p agent-launcher-cli --manifest-path "$root/Cargo.toml"
binary="$root/target/debug/agent-launcher"

rm -rf "$home" "$shots"
mkdir -p "$home" "$shots"
HOME="$home" "$root/scripts/demo/seed-beads.sh" "$repo"

"${tmux[@]}" kill-server 2>/dev/null || true
trap '"${tmux[@]}" kill-server 2>/dev/null || true' EXIT
"${tmux[@]}" new-session -d -s demo -x 180 -y 48 -c "$repo" \
  env -i HOME="$home" PATH="$PATH" USER="${USER:-demo}" LANG=en_US.UTF-8 \
  TERM=xterm-256color COLORTERM=truecolor AGENT_LAUNCHER_DEMO_ACTIVITY=1 \
  "$binary"
"${tmux[@]}" set -g default-terminal tmux-256color >/dev/null

capture() { "${tmux[@]}" capture-pane -p -e -t demo; }

# Wait until the seeded issues are listed.
for _ in $(seq 1 100); do
  capture | grep -q "Persist the outgoing" && break
  sleep 0.2
done
sleep 1

order=()
# shot NAME TITLE [KEYS...]: sends the keys, waits for the redraw, captures.
shot() {
  local name=$1 title=$2
  shift 2
  for key in "$@"; do
    "${tmux[@]}" send-keys -t demo "$key"
    sleep 0.15
  done
  sleep 0.8
  capture > "$shots/$name.ansi"
  order+=("$title" "$shots/$name.ansi")
}

shot 01-wide-list "Wide (180×48): list with preview"

# mouse BUTTON COLUMN ROW [m]: sends one SGR mouse event (1-based cells).
mouse() { "${tmux[@]}" send-keys -t demo -l $'\e'"[<$1;$2;$3${4:-M}"; sleep 0.1; }
# grip: the divider's 1-based column and row, from its ┃ grip on screen.
grip() {
  capture | sed 's/\x1b\[[0-9;]*m//g' | python3 -c '
import sys
for row, line in enumerate(sys.stdin.read().split("\n"), 1):
    if "┃" in line:
        print(line.index("┃") + 1, row)
        break'
}
# drag TO: drags the divider, wherever it is, to a column.
drag() {
  local column row
  read -r column row < <(grip)
  mouse 0 "$column" "$row"
  mouse 32 $(( (column + $1) / 2 )) "$row"
  mouse 32 "$1" "$row"
  mouse 0 "$1" "$row" m
}
drag 70
shot 01b-dragged-wide "Dragged the divider left: a wider preview"
drag 178
shot 01c-dragged-closed "Dragged to the right edge: preview closed, handle left"
drag 99

# The README screenshot: the wide split on the P0 crash report. Rows share a
# creation second, so their order varies; step down until it is selected.
for _ in $(seq 1 14); do
  capture | sed 's/\x1b\[[0-9;]*m//g' | grep -q "│  Editor crashes when pasting" && break
  "${tmux[@]}" send-keys -t demo Down
  sleep 0.3
done
# Past the launch logo's fade, so the activity graph shows on its own.
sleep 3
chrome="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
# png NAME: captures the screen as a frame-only PNG for the README.
png() {
  capture > "$shots/$1.ansi"
  python3 "$root/scripts/demo/ansi2html.py" --bare "$out/$1.html" "$shots/$1.ansi"
  if [ -x "$chrome" ]; then
    # 180 columns of 13px Menlo, 48 rows of 1.25em, at 2x for a crisp image.
    "$chrome" --headless=new --disable-gpu --hide-scrollbars \
      --force-device-scale-factor=2 --window-size=1412,780 \
      --screenshot="$out/$1.png" "file://$out/$1.html" >/dev/null 2>&1 || true
  fi
}
png readme
# The full view on its Description tab.
"${tmux[@]}" send-keys -t demo Enter
sleep 0.4
"${tmux[@]}" send-keys -t demo 2
sleep 0.8
png readme-detail
# Launch settings: d, the built-in prompt, a one-off instruction, then the
# harness picker.
"${tmux[@]}" send-keys -t demo 1 d
sleep 1.5
"${tmux[@]}" send-keys -t demo Enter
sleep 1
"${tmux[@]}" send-keys -t demo -l "Add a regression test that pastes a 25 MB PNG."
sleep 0.3
"${tmux[@]}" send-keys -t demo Tab h
sleep 1
png readme-dispatch
for _ in 1 2 3 4; do "${tmux[@]}" send-keys -t demo Escape; sleep 0.2; done
"${tmux[@]}" send-keys -t demo Home
# Search for the crash report: it has steps, a log block and a list. Search
# is fuzzy over descriptions too, so the query must be distinctive.
shot 02-wide-preview "Wide: the P0 crash previewed" 2 0 Space M B
detail_start=${#order[@]}
shot 03-overview "Full view (Enter): Overview" Enter
shot 04-description "Full view: Description" Tab
shot 05-agent "Full view: Agent" Tab
shot 06-details "Full view: Details" Tab
detail=("${order[@]:$detail_start}")
"${tmux[@]}" send-keys -t demo Escape
for _ in 1 2 3 4 5; do "${tmux[@]}" send-keys -t demo BSpace; done
"${tmux[@]}" resize-window -t demo -x 120 -y 40
shot 07-narrow-list "Narrow (120×40): list without preview"
shot 08-narrow-detail "Narrow: detail Overview" Enter
"${tmux[@]}" send-keys -t demo Escape
"${tmux[@]}" resize-window -t demo -x 120 -y 20
# The epic has the longest description, so it overflows a short window.
shot 09-scrollbar "Short (120×20): Description scrolls, with a scrollbar" O f f l i n e Enter Tab
detail+=("${order[@]: -2}")

python3 "$root/scripts/demo/ansi2html.py" "$out/screenshots.html" "${order[@]}"
python3 "$root/scripts/demo/ansi2html.py" "$out/detail.html" "${detail[@]}"
echo "$out/screenshots.html"
echo "$out/detail.html"
