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
    cp target/release/agent-launcher "$HOME/.local/bin/agent-launcher"
