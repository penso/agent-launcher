#!/usr/bin/env bash
# Renders the Homebrew formula for VERSION from a release's SHA256SUMS.
set -euo pipefail
source "$(dirname -- "$0")/common.sh"
[[ $# == 2 ]] || fail 'Usage: bash render-formula.sh VERSION SHA256SUMS > agent-launcher.rb'
version_check "$1"
[[ -f $2 && ! -L $2 ]] || fail 'SHA256SUMS must be a regular file'

checksum() {
    local sum
    sum=$(awk -v file="agent-launcher-$1-$2.tar.gz" '$2 == file || $2 == "*" file { print $1 }' "$3")
    [[ $sum =~ ^[0-9a-f]{64}$ ]] || fail "No checksum for agent-launcher-$1-$2.tar.gz"
    printf '%s' "$sum"
}
macos=$(checksum "$1" universal-apple-darwin "$2")
linux_x86_64=$(checksum "$1" x86_64-unknown-linux-gnu "$2")
linux_aarch64=$(checksum "$1" aarch64-unknown-linux-gnu "$2")
while IFS= read -r line; do
    line=${line//@VERSION@/$1}
    line=${line//@SHA256_MACOS@/$macos}
    line=${line//@SHA256_LINUX_X86_64@/$linux_x86_64}
    printf '%s\n' "${line//@SHA256_LINUX_AARCH64@/$linux_aarch64}"
done < "$release_root/scripts/release/homebrew/Formula/agent-launcher.rb"
