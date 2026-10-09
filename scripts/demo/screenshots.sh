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
repo="$out/orbit"
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
