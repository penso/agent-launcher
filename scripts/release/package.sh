#!/usr/bin/env bash
# Packages one release executable as agent-launcher-VERSION-TARGET.tar.gz:
# bin/agent-launcher plus the README, LICENSE and example configuration.
set -euo pipefail
source "$(dirname -- "$0")/common.sh"
[[ $# == 4 ]] || fail 'Usage: bash package.sh VERSION TARGET EXECUTABLE OUTPUT_DIR'
version_check "$1"
[[ $2 =~ ^[a-z0-9_]+-[a-z0-9_-]+$ ]] || fail 'Malformed target triple'
[[ -f $3 && -x $3 && ! -L $3 ]] || fail 'Executable must be a regular executable file'
[[ -d $4 ]] || fail 'Existing output directory required'

name="agent-launcher-$1-$2"
archive="$(cd -- "$4" && pwd)/$name.tar.gz"
new_output "$archive"
stage=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/agent-launcher-package.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT

mkdir -p "$stage/$name/bin"
install -m 0755 "$3" "$stage/$name/bin/agent-launcher"
install -m 0644 "$release_root/README.md" "$release_root/LICENSE" \
    "$release_root/config.example.toml" "$stage/$name/"
# Fixed owner and order so the archive does not leak the builder's account.
tar -C "$stage" --owner=0 --group=0 --numeric-owner -czf "$archive" "$name" 2>/dev/null ||
    tar -C "$stage" --uid 0 --gid 0 -czf "$archive" "$name"
printf '%s\n' "$archive"
