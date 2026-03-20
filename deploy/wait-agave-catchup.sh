#!/usr/bin/env bash
# wait-agave-catchup.sh
# Polls Agave RPC health every 30s. When caught up, switches sim-server
# to use Agave local (faster simulation) and restarts it.
#
# Usage: nohup bash deploy/wait-agave-catchup.sh &

set -euo pipefail

AGAVE_RPC="http://127.0.0.1:9000"
SERVICE_FILE="/etc/systemd/system/sim-server.service"
POLL_INTERVAL=30
MAX_SLOTS_BEHIND=150  # tolerate small lag

echo "[$(date -Iseconds)] wait-agave-catchup: starting (max_behind=$MAX_SLOTS_BEHIND, poll=${POLL_INTERVAL}s)"

while true; do
    RESP=$(curl -sf "$AGAVE_RPC" -X POST \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' 2>/dev/null || echo '{"error":"unreachable"}')

    # Check if healthy
    if echo "$RESP" | grep -q '"result":"ok"'; then
        echo "[$(date -Iseconds)] Agave RPC is healthy!"
        break
    fi

    # Extract slots behind for progress reporting
    BEHIND=$(echo "$RESP" | grep -oP '"numSlotsBehind":\K[0-9]+' 2>/dev/null || echo "?")

    # Accept if close enough
    if [[ "$BEHIND" =~ ^[0-9]+$ ]] && [ "$BEHIND" -le "$MAX_SLOTS_BEHIND" ]; then
        echo "[$(date -Iseconds)] Agave RPC close enough (${BEHIND} slots behind, threshold=$MAX_SLOTS_BEHIND)"
        break
    fi

    echo "[$(date -Iseconds)] Agave still catching up: ${BEHIND} slots behind"
    sleep "$POLL_INTERVAL"
done

echo "[$(date -Iseconds)] Switching sim-server to Agave local RPC..."

# Update sim-server service: switch UPSTREAM_RPC_URL to local Agave
# and enable BanksClient
sed -i \
    -e 's|^Environment=UPSTREAM_RPC_URL=.*|Environment=UPSTREAM_RPC_URL=http://127.0.0.1:9000|' \
    -e 's|^# Environment=BANKS_SERVER_ADDR=.*|Environment=BANKS_SERVER_ADDR=127.0.0.1:8900|' \
    "$SERVICE_FILE"

systemctl daemon-reload
systemctl restart sim-server.service

echo "[$(date -Iseconds)] sim-server restarted with Agave local RPC + BanksClient"
echo "[$(date -Iseconds)] Verifying sim-server is up..."

sleep 3

if systemctl is-active --quiet sim-server.service; then
    echo "[$(date -Iseconds)] sim-server OK ✓"
    # Verify the switch
    SIM_HEALTH=$(curl -sf http://127.0.0.1:8081 -X POST \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' 2>/dev/null || echo "no response")
    echo "[$(date -Iseconds)] sim-server health: $SIM_HEALTH"
else
    echo "[$(date -Iseconds)] ERROR: sim-server failed to start! Rolling back..."
    # Rollback
    sed -i \
        -e 's|^Environment=UPSTREAM_RPC_URL=.*|Environment=UPSTREAM_RPC_URL=https://solana-rpc.publicnode.com/813e9f33c3d71de4ff935943cbf85ebe3dc3b6321d700ee9a6c98234610563b2|' \
        -e 's|^Environment=BANKS_SERVER_ADDR=.*|# Environment=BANKS_SERVER_ADDR=127.0.0.1:8900|' \
        "$SERVICE_FILE"
    systemctl daemon-reload
    systemctl restart sim-server.service
    echo "[$(date -Iseconds)] Rolled back to PublicNode RPC"
fi

echo "[$(date -Iseconds)] wait-agave-catchup: done"
