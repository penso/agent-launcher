#!/usr/bin/env bash
# Commits the rendered formula to penso/homebrew-tap with the tap's dedicated
# deploy key. Adapted from herdr-gpui's cask updater.
set +x
set -euo pipefail
source "$(dirname -- "$0")/common.sh"
[[ $# == 2 ]] || fail 'Usage: bash update-homebrew.sh VERSION RENDERED_FORMULA'
version_check "$1"
[[ -f $2 && ! -L $2 && -s $2 ]] || fail 'Rendered formula must be a nonempty regular file'
grep -Fxq "  version \"$1\"" "$2" || fail 'Rendered formula version does not match'
[[ -n ${HOMEBREW_TAP_SSH_KEY:-} ]] || fail 'HOMEBREW_TAP_SSH_KEY is required'

formula=Formula/agent-launcher.rb
umask 077
temp=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/agent-launcher-tap.XXXXXXXX")
trap 'rm -rf -- "$temp"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
printf '%s\n' "$HOMEBREW_TAP_SSH_KEY" > "$temp/key"
unset HOMEBREW_TAP_SSH_KEY
chmod 600 "$temp/key"
# Bootstrap trust over verified HTTPS, never from an unauthenticated ssh-keyscan.
curl --fail --silent --show-error --proto '=https' --tlsv1.2 \
    --connect-timeout 15 --max-time 60 https://api.github.com/meta > "$temp/meta.json"
jq -er '.ssh_keys | select(type == "array" and length > 0) | .[] | "github.com " + .' \
    "$temp/meta.json" > "$temp/known_hosts"
export GIT_SSH_COMMAND="ssh -F /dev/null -i '$temp/key' -o IdentitiesOnly=yes -o IdentityAgent=none -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile='$temp/known_hosts' -o GlobalKnownHostsFile=/dev/null"
export GIT_TERMINAL_PROMPT=0
git -c core.hooksPath=/dev/null clone --depth=1 --single-branch \
    git@github.com:penso/homebrew-tap.git "$temp/tap"
branch=$(git -C "$temp/tap" symbolic-ref --short HEAD)
git check-ref-format "refs/heads/$branch"
[[ ! -L $temp/tap/Formula && ! -L $temp/tap/$formula ]] || fail 'Refusing symlink formula path'
[[ ! -e $temp/tap/$formula || -f $temp/tap/$formula ]] || fail 'Formula path is not a regular file'
if [[ -f $temp/tap/$formula ]]; then
    # Parse only a single literal version declaration; never evaluate tap Ruby.
    declaration=$(grep -E '^[[:blank:]]*version([[:blank:]]|$)' "$temp/tap/$formula") || fail 'Missing existing formula version'
    [[ $declaration =~ ^[[:blank:]]*version[[:blank:]]+\"([0-9.]+)\"[[:blank:]]*$ ]] || fail 'Malformed existing formula version'
    current_version=${BASH_REMATCH[1]}
    version_check "$current_version"
    IFS=. read -r -a current_parts <<< "$current_version"
    IFS=. read -r -a next_parts <<< "$1"
    for i in 0 1; do
        current=${current_parts[$i]}
        next=${next_parts[$i]}
        # Length then lexical comparison avoids integer overflow for large components.
        if [[ ${#next} -lt ${#current} || ( ${#next} -eq ${#current} && $next < $current ) ]]; then
            fail "Refusing Homebrew downgrade from $current_version to $1"
        fi
        [[ $next == "$current" ]] || break
    done
    if [[ $1 == "$current_version" ]]; then
        cmp -s -- "$2" "$temp/tap/$formula" || fail 'Same formula version has different content; publish a new version'
        printf '%s\n' 'Homebrew formula is already up to date'
        exit 0
    fi
fi
mkdir -p "$temp/tap/Formula"
cp -- "$2" "$temp/tap/$formula"
git -C "$temp/tap" add -- "$formula"
if git -C "$temp/tap" diff --cached --quiet --exit-code; then
    printf '%s\n' 'Homebrew formula is already up to date'
    exit 0
else
    status=$?
    [[ $status == 1 ]] || exit "$status"
fi
export GIT_AUTHOR_NAME='agent-launcher release'
export GIT_AUTHOR_EMAIL='41898282+github-actions[bot]@users.noreply.github.com'
export GIT_COMMITTER_NAME="$GIT_AUTHOR_NAME" GIT_COMMITTER_EMAIL="$GIT_AUTHOR_EMAIL"
git -C "$temp/tap" -c core.hooksPath=/dev/null -c commit.gpgSign=false \
    commit -m "chore: release agent-launcher $1"
git -C "$temp/tap" -c core.hooksPath=/dev/null push origin "HEAD:refs/heads/$branch"
