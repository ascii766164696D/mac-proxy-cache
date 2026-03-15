#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Dev profile with opt-level=2 (release profile has a TLS bug with google.com)
RUST_BINARY="$ROOT/target/debug/mac-proxy-cache"
SWIFT_BINARY="$ROOT/app/.build/release/MacProxyCache"
DATA_DIR="$HOME/mac-proxy-cache"
PID_FILE="$DATA_DIR/proxy.pid"
LOG_FILE="$DATA_DIR/proxy.log"

stop_all() {
    echo "Stopping existing processes..."

    # Stop menu bar app first
    pkill -x MacProxyCache 2>/dev/null && echo "  Stopped menu bar app" || true

    # Stop proxy via CLI (handles system proxy restore)
    if [ -f "$RUST_BINARY" ] && [ -f "$PID_FILE" ]; then
        "$RUST_BINARY" stop 2>/dev/null && echo "  Stopped proxy" || true
    fi

    # Clean up anything still holding our ports
    sleep 1
    for PORT in 9090 9091; do
        PIDS=$(lsof -ti :"$PORT" 2>/dev/null || true)
        if [ -n "$PIDS" ]; then
            echo "$PIDS" | xargs kill 2>/dev/null || true
        fi
    done

    rm -f "$PID_FILE"
    sleep 1
}

# --stop flag: just stop
if [ "${1:-}" = "--stop" ]; then
    stop_all
    echo "Stopped."
    exit 0
fi

stop_all

# --- Build ---

echo ""
echo "Building proxy..."
cargo build 2>&1 | grep -E "Compiling proxy-|Finished|error" || true

if [ ! -f "$SWIFT_BINARY" ]; then
    echo "Building Swift menu bar app (release)..."
    (cd app && swift build -c release 2>&1 | tail -3)
fi

# --- Start ---

mkdir -p "$DATA_DIR"

echo ""
echo "Starting proxy..."
ulimit -n 65536 2>/dev/null || true
# Start proxy first WITHOUT system proxy, then enable it via API after proxy is listening.
# The proxy process inherits the "no proxy" network state so its own outbound
# connections go direct, while browsers pick up the system proxy setting.
networksetup -setwebproxystate Wi-Fi off 2>/dev/null || true
networksetup -setsecurewebproxystate Wi-Fi off 2>/dev/null || true
sleep 0.5
RUST_LOG="${RUST_LOG:-proxy_core=info,info}" \
  nohup "$RUST_BINARY" start --foreground --no-system-proxy > "$LOG_FILE" 2>&1 &

# Wait for proxy health
for i in $(seq 1 25); do
    if curl -s -o /dev/null http://127.0.0.1:9091/api/health 2>/dev/null; then
        break
    fi
    sleep 0.2
done

if curl -s -o /dev/null http://127.0.0.1:9091/api/health 2>/dev/null; then
    PROXY_PID=$(cat "$PID_FILE" 2>/dev/null || echo "?")
    echo "  Proxy ready (PID $PROXY_PID)"

    # Now enable system proxy via the API (proxy is already listening, won't loop)
    curl -s -X POST http://127.0.0.1:9091/api/system-proxy \
      -H "Content-Type: application/json" \
      -d '{"enabled":true}' > /dev/null 2>&1
    echo "  System proxy enabled"
else
    echo "  WARNING: Proxy may not have started. Check: tail -f $LOG_FILE"
fi

echo ""
echo "Starting menu bar app..."
nohup "$SWIFT_BINARY" > /dev/null 2>&1 &
echo "  Menu bar app started"

echo ""
echo "=== Running ==="
echo "  Proxy:     http://127.0.0.1:9090"
echo "  Dashboard: http://127.0.0.1:9091"
echo "  Cache:     ~/mac-proxy-cache/cache"
echo "  Logs:      tail -f $LOG_FILE"
echo ""
echo "  Stop:      ./scripts/restart.sh --stop"
echo "  Restart:   ./scripts/restart.sh"
