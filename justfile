default:
    @just --list

nightly_toolchain := "nightly-2025-11-30"

format:
    cargo +{{nightly_toolchain}} fmt --all

format-check:
    cargo +{{nightly_toolchain}} fmt --all -- --check

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
