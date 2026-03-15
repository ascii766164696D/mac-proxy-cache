#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

RUST_BINARY="$ROOT/target/release/mac-proxy-cache"
SWIFT_BINARY="$ROOT/app/.build/release/MacProxyCache"

# Build if needed
if [ ! -f "$RUST_BINARY" ]; then
    echo "Building Rust proxy..."
    cargo build --release
fi

if [ ! -f "$SWIFT_BINARY" ]; then
    echo "Building Swift menu bar app..."
    cd app && swift build -c release && cd "$ROOT"
fi

echo "=== Starting menu bar app ==="
echo "  The proxy will be managed from the menu bar icon."
echo ""

"$SWIFT_BINARY"
