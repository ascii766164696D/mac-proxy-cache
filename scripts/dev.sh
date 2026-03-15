#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "=== Building (debug) ==="
cargo build

echo ""
echo "=== Starting proxy in foreground ==="
echo "  Proxy:     http://127.0.0.1:9090"
echo "  Dashboard: http://127.0.0.1:9091"
echo "  Cache dir: ~/mac-proxy-cache/cache"
echo ""
echo "  Configure your browser to use proxy 127.0.0.1:9090"
echo "  Press Ctrl+C to stop"
echo ""

RUST_LOG="${RUST_LOG:-proxy_core=info,info}" \
  cargo run -- start --foreground --no-system-proxy "$@"
