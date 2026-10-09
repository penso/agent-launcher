#!/usr/bin/env bash
# Packages one release executable as agent-launcher-VERSION-TARGET.tar.gz:
# bin/agent-launcher plus the README, LICENSE, NOTICE, third-party notices and
# example configuration.
set -euo pipefail
source "$(dirname -- "$0")/common.sh"
[[ $# == 5 ]] || fail 'Usage: bash package.sh VERSION TARGET EXECUTABLE OUTPUT_DIR THIRD_PARTY_NOTICES'
version_check "$1"
[[ $2 =~ ^[a-z0-9_]+-[a-z0-9_-]+$ ]] || fail 'Malformed target triple'
[[ -f $3 && -x $3 && ! -L $3 ]] || fail 'Executable must be a regular executable file'
[[ -d $4 ]] || fail 'Existing output directory required'
[[ -f $5 && ! -L $5 && -s $5 ]] || fail 'Third-party notices must be a nonempty regular file'

name="agent-launcher-$1-$2"
archive="$(cd -- "$4" && pwd)/$name.tar.gz"
new_output "$archive"
stage=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/agent-launcher-package.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT

mkdir -p "$stage/$name/bin"
install -m 0755 "$3" "$stage/$name/bin/agent-launcher"
install -m 0644 "$release_root/README.md" "$release_root/LICENSE" "$release_root/NOTICE" \
    "$release_root/config.example.toml" "$stage/$name/"
install -m 0644 "$5" "$stage/$name/THIRD-PARTY-NOTICES.txt"
# Fixed owner and order so the archive does not leak the builder's account.
tar -C "$stage" --owner=0 --group=0 --numeric-owner -czf "$archive" "$name" 2>/dev/null ||
    tar -C "$stage" --uid 0 --gid 0 -czf "$archive" "$name"
printf '%s\n' "$archive"
