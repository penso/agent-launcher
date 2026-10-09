default:
    @just --list

nightly_toolchain := "nightly-2025-11-30"

# Rust with rustfmt, TOML with taplo (aligned `=`, sorted keys; see taplo.toml).
format:
    cargo +{{nightly_toolchain}} fmt --all
    taplo fmt

format-check:
    cargo +{{nightly_toolchain}} fmt --all -- --check
    taplo fmt --check

lockfile-check:
    cargo metadata --locked --format-version=1 > /dev/null

lint: lockfile-check
    cargo +{{nightly_toolchain}} clippy --workspace --all-features --all-targets -- -D warnings

test:
    cargo +{{nightly_toolchain}} test --workspace

run:
    cargo +{{nightly_toolchain}} run -p agent-launcher-cli

# Build and install the release binary to ~/.local/bin.
install:
    cargo +{{nightly_toolchain}} build --release -p agent-launcher-cli
    mkdir -p "$HOME/.local/bin"
    # Copy to a new file and rename over the old one: overwriting a signed
    # binary in place leaves macOS's cached signature stale and the next
    # launch is killed (SIGKILL) before it runs.
    cp target/release/agent-launcher "$HOME/.local/bin/agent-launcher.new"
    mv -f "$HOME/.local/bin/agent-launcher.new" "$HOME/.local/bin/agent-launcher"

# Seed a Beads demo repository, run the launcher on it in a private tmux
# server, and write colour screenshots to target/demo/screenshots.html.
demo-shots:
    scripts/demo/screenshots.sh

# Dispatch the Release workflow for main's HEAD and watch it. The workflow picks
# the calendar version (YYYYMMDD.N), builds, publishes, and updates Homebrew.
release:
    bash scripts/release/dispatch.sh

# Check the release scripts and workflows locally, as CI does.
release-check:
    for script in scripts/release/*.sh scripts/demo/*.sh; do bash -n "$script" || exit; done
    PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/release/tests -v
    actionlint .github/workflows/*.yml
    zizmor --offline .github/
    git diff --check
