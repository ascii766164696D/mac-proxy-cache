#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "=== Building Rust proxy (release) ==="
cargo build --release

echo ""
echo "=== Building Swift menu bar app ==="
cd app
swift build -c release
cd "$ROOT"

echo ""
echo "=== Build complete ==="
echo "  Proxy binary:    $ROOT/target/release/mac-proxy-cache"
echo "  Menu bar app:    $ROOT/app/.build/release/MacProxyCache"
