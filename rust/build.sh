#!/bin/sh
# Builds release binaries (GUI + CLI). Windows exes cross-compile from Linux with:
#   rustup target add x86_64-pc-windows-gnu && apt install mingw-w64
set -e
cd "$(dirname "$0")"
cargo build --release
cargo build --release --target x86_64-pc-windows-gnu
mkdir -p ../dist
cp target/release/claude-sessions ../dist/claude-sessions-linux-x86_64
cp target/release/claude-sessions-cli ../dist/claude-sessions-cli-linux-x86_64
cp target/x86_64-pc-windows-gnu/release/claude-sessions.exe target/x86_64-pc-windows-gnu/release/claude-sessions-cli.exe ../dist/
ls -la ../dist
