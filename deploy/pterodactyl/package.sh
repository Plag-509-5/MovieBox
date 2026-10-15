#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "Build this archive on Linux to match the Pterodactyl runtime." >&2
    exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo is required to build the Rust provider backend." >&2
    exit 1
fi

cargo build --release --locked --bin moviebox-api

mkdir -p "$ROOT/dist"
STAGING_DIR="$(mktemp -d "$ROOT/dist/.pterodactyl-build.XXXXXX")"
trap 'rm -rf "$STAGING_DIR"' EXIT
mkdir -p "$STAGING_DIR/bin"
install -m 0755 "$ROOT/target/release/moviebox-api" "$STAGING_DIR/bin/moviebox-api"
install -m 0644 "$ROOT/deploy/pterodactyl/server.js" "$STAGING_DIR/server.js"
install -m 0644 "$ROOT/deploy/pterodactyl/package.json" "$STAGING_DIR/package.json"
install -m 0644 "$ROOT/deploy/pterodactyl/package-lock.json" "$STAGING_DIR/package-lock.json"

tar -czf "$ROOT/dist/moviebox-api-pterodactyl.tar.gz" -C "$STAGING_DIR" .
echo "Created $ROOT/dist/moviebox-api-pterodactyl.tar.gz"
echo "Upload and extract it into the root directory of the Pterodactyl server."
