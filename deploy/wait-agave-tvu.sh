#!/usr/bin/env bash
# wait-agave-tvu.sh — Polls until Agave TVU port (8802) receives external shreds,
# then starts the shred-mirror service.
#
# Usage: nohup bash /root/spy_node/deploy/wait-agave-tvu.sh &

set -euo pipefail

TVU_PORT="${1:-8802}"
CHECK_INTERVAL=30

echo "[wait-agave-tvu] Waiting for Agave to join Turbine (port $TVU_PORT)..."

while true; do
    # Check if any external UDP packets arrive at TVU port in 3 seconds
    count=$(timeout 3 tcpdump -i eth0 -c 1 "udp dst port $TVU_PORT" 2>/dev/null | grep -c "UDP" || true)
    if [[ "$count" -gt 0 ]]; then
        echo "[wait-agave-tvu] Agave TVU receiving shreds! Starting shred-mirror..."
        systemctl start shred-mirror.service
        echo "[wait-agave-tvu] shred-mirror started. Dual Turbine feed active."

        # Also check if RPC is ready for sim-server upgrade
        if curl -s http://127.0.0.1:9000 -X POST -H "Content-Type: application/json" \
            -d '{"jsonrpc":"2.0","id":1,"method":"getSlot"}' 2>/dev/null | grep -q "result"; then
            echo "[wait-agave-tvu] Agave RPC is READY on port 9000!"
            echo "[wait-agave-tvu] TODO: Update sim-server.service with UPSTREAM_RPC_URL=http://127.0.0.1:9000"
        fi
        exit 0
    fi
    echo "[wait-agave-tvu] No TVU traffic yet on port $TVU_PORT. Checking again in ${CHECK_INTERVAL}s..."
    sleep "$CHECK_INTERVAL"
done
