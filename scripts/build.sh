#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release --locked
mkdir -p dist/spotify-library-sync
cp target/release/spotify-sync README.md LICENSE .env.example dist/spotify-library-sync/
(cd dist && tar -czf "spotify-library-sync-$(uname -s)-$(uname -m).tar.gz" spotify-library-sync)
