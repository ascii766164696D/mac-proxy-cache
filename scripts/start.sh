#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

BINARY="$ROOT/target/release/mac-proxy-cache"

if [ ! -f "$BINARY" ]; then
    echo "Binary not found. Building first..."
    cargo build --release
fi

echo "=== Starting proxy with system proxy ==="
echo "  Proxy:     http://127.0.0.1:9090"
echo "  Dashboard: http://127.0.0.1:9091"
echo "  System proxy will be configured for Safari/Chrome"
echo ""
echo "  Press Ctrl+C to stop (system proxy will be restored)"
echo ""

RUST_LOG="${RUST_LOG:-proxy_core=info,info}" \
  no_proxy="*" NO_PROXY="*" \
  "$BINARY" start --foreground "$@"
