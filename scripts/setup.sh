#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "=== Mac Proxy Cache — First-time Setup ==="
echo ""

# 1. Build
echo "Step 1: Building..."
cargo build --release
echo "  Done."
echo ""

# 2. Generate CA cert (start and immediately stop)
echo "Step 2: Generating CA certificate..."
timeout 2 ./target/release/mac-proxy-cache start --foreground --no-system-proxy 2>/dev/null || true
echo "  CA certificate: ~/mac-proxy-cache/ca/ca.crt"
echo ""

# 3. Show cert info
echo "Step 3: CA certificate info:"
./target/release/mac-proxy-cache cert show
echo ""

# 4. Install CA
echo "Step 4: Installing CA into system trust store..."
echo "  This will prompt for your password."
read -p "  Proceed? [y/N] " -n 1 -r
echo ""
if [[ $REPLY =~ ^[Yy]$ ]]; then
    ./target/release/mac-proxy-cache cert install
else
    echo "  Skipped. You can run this later:"
    echo "    ./target/release/mac-proxy-cache cert install"
fi
echo ""

echo "=== Setup complete ==="
echo ""
echo "To start the proxy:"
echo "  ./scripts/dev.sh                    # foreground, manual browser config"
echo "  ./scripts/start.sh                  # with system proxy (Safari/Chrome auto-configured)"
echo ""
echo "To manage:"
echo "  ./target/release/mac-proxy-cache status"
echo "  ./target/release/mac-proxy-cache cache stats"
echo "  ./target/release/mac-proxy-cache cache search <query>"
echo "  ./target/release/mac-proxy-cache stop"
