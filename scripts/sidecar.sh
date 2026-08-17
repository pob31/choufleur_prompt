#!/usr/bin/env bash
# Build the show server and put it where the bundler expects to find it.
#
# The app ships `choufleur-replay` as a sidecar rather than linking it: the window
# and the engine have nothing to say to each other beyond a port number, and keeping
# them apart means editing the shell does not rebuild whisper.cpp. Tauri wants the
# host triple in the filename, and refuses to bundle a binary named anything else.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
server="$here/../server"
triple="$(rustc -vV | sed -n 's/^host: //p')"
dest="$server/crates/choufleur-app/binaries"

cd "$server"
cargo build --release -p choufleur-replay

mkdir -p "$dest"
cp "$server/target/release/choufleur-replay" "$dest/choufleur-replay-$triple"
echo "sidecar: $dest/choufleur-replay-$triple"
