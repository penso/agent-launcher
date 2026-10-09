#!/usr/bin/env bash
set -euo pipefail
source "$(dirname -- "$0")/common.sh"
[[ $# == 2 ]] || fail 'Usage: bash generate-changelog.sh VERSION OUTPUT_DIR'
version_check "$1"
[[ -d $2 ]] || fail 'Existing output directory required'
command -v git-cliff >/dev/null || fail 'git-cliff is required: cargo install --locked git-cliff'
out=$(cd -- "$2" && pwd)
new_output "$out/CHANGELOG.md"
new_output "$out/RELEASE_NOTES.md"

# The tag is created only when the release publishes, so the commits being cut
# are still "unreleased" here and `--tag` names the version they belong to.
tag=v$1
cd -- "$release_root"
source_sha=$(git rev-parse HEAD)

git-cliff --config cliff.toml --tag "$tag" --output "$out/CHANGELOG.md"

cat > "$out/RELEASE_NOTES.md" <<NOTES
Universal macOS binary, and x86_64 and ARM64 Linux binaries (glibc). Install with \`brew install penso/tap/agent-launcher\` or unpack a tarball and put \`bin/agent-launcher\` on your PATH. Source: $source_sha.

NOTES
git-cliff --config cliff.toml --tag "$tag" --unreleased --strip header >> "$out/RELEASE_NOTES.md"

[[ -s $out/CHANGELOG.md && -s $out/RELEASE_NOTES.md ]] || fail 'Generated changelog is empty'
