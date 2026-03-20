#!/usr/bin/env bash
# agave-watchdog.sh
# Monitors Agave health continuously. When caught up, switches sim-server.
# If Agave falls behind or dies, handles recovery.
# Run as: systemctl start agave-watchdog
set -uo pipefail

AGAVE_RPC="http://127.0.0.1:9000"
SIM_SERVICE_FILE="/etc/systemd/system/sim-server.service"
POLL_INTERVAL=30
MAX_SLOTS_BEHIND=300
CATCHUP_SWITCH_THRESHOLD=150
SIMSERVER_SWITCHED=false
CONSECUTIVE_FAILURES=0
MAX_CONSECUTIVE_FAILURES=20  # 20 * 30s = 10 min without RPC = log warning

log() { echo "[$(date -Iseconds)] agave-watchdog: $*"; }

check_agave_health() {
    local resp
    resp=$(curl -sf --max-time 5 "$AGAVE_RPC" -X POST \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' 2>/dev/null) || { echo "unreachable"; return 1; }

    if echo "$resp" | grep -q '"result":"ok"'; then
        echo "healthy"
        return 0
    fi

    local behind
    behind=$(echo "$resp" | grep -oP '"numSlotsBehind":\K[0-9]+' 2>/dev/null || echo "unknown")
    echo "$behind"

    if [[ "$behind" =~ ^[0-9]+$ ]] && [ "$behind" -le "$MAX_SLOTS_BEHIND" ]; then
        return 0
    fi
    return 1
}

switch_simserver_to_agave() {
    if [ "$SIMSERVER_SWITCHED" = true ]; then return; fi
    log "Switching sim-server to Agave local RPC..."

    if [ -f "$SIM_SERVICE_FILE" ]; then
        sed -i \
            -e 's|^Environment=UPSTREAM_RPC_URL=.*|Environment=UPSTREAM_RPC_URL=http://127.0.0.1:9000|' \
            -e 's|^# Environment=BANKS_SERVER_ADDR=.*|Environment=BANKS_SERVER_ADDR=127.0.0.1:8900|' \
            "$SIM_SERVICE_FILE"
        systemctl daemon-reload
        systemctl restart sim-server.service
        sleep 3
        if systemctl is-active --quiet sim-server.service; then
            log "sim-server switched to Agave local OK"
            SIMSERVER_SWITCHED=true
        else
            log "sim-server failed to start with Agave, rolling back"
            rollback_simserver
        fi
    fi
}

rollback_simserver() {
    if [ "$SIMSERVER_SWITCHED" = false ]; then return; fi
    log "Rolling back sim-server to PublicNode..."
    if [ -f "$SIM_SERVICE_FILE" ]; then
        sed -i \
            -e 's|^Environment=UPSTREAM_RPC_URL=.*|Environment=UPSTREAM_RPC_URL=https://solana-rpc.publicnode.com/813e9f33c3d71de4ff935943cbf85ebe3dc3b6321d700ee9a6c98234610563b2|' \
            -e 's|^Environment=BANKS_SERVER_ADDR=.*|# Environment=BANKS_SERVER_ADDR=127.0.0.1:8900|' \
            "$SIM_SERVICE_FILE"
        systemctl daemon-reload
        systemctl restart sim-server.service
        SIMSERVER_SWITCHED=false
        log "sim-server rolled back to PublicNode"
    fi
}

log "started (poll=${POLL_INTERVAL}s, max_behind=${MAX_SLOTS_BEHIND}, switch_at=${CATCHUP_SWITCH_THRESHOLD})"

while true; do
    result=$(check_agave_health)
    rc=$?

    if [ "$result" = "unreachable" ]; then
        CONSECUTIVE_FAILURES=$((CONSECUTIVE_FAILURES + 1))
        if [ $CONSECUTIVE_FAILURES -eq 1 ]; then
            log "Agave RPC unreachable (startup/rebuild in progress)"
        elif [ $((CONSECUTIVE_FAILURES % MAX_CONSECUTIVE_FAILURES)) -eq 0 ]; then
            log "Agave RPC unreachable for $((CONSECUTIVE_FAILURES * POLL_INTERVAL))s"
            # Check if Agave process is alive
            if ! systemctl is-active --quiet solana-spy.service; then
                log "Agave service is down! systemd should auto-restart it."
            fi
            # If sim-server was on Agave local, rollback
            rollback_simserver
        fi
    elif [ "$result" = "healthy" ] || { [[ "$result" =~ ^[0-9]+$ ]] && [ "$result" -le "$CATCHUP_SWITCH_THRESHOLD" ]; }; then
        if [ $CONSECUTIVE_FAILURES -gt 0 ]; then
            log "Agave RPC recovered (was unreachable for $((CONSECUTIVE_FAILURES * POLL_INTERVAL))s)"
        fi
        CONSECUTIVE_FAILURES=0

        if [ "$result" = "healthy" ]; then
            log "Agave healthy (caught up)"
        else
            log "Agave close enough (${result} slots behind)"
        fi
        switch_simserver_to_agave
    else
        CONSECUTIVE_FAILURES=0
        # Catching up — report progress periodically (every 5 polls = 2.5 min)
        if [[ "$result" =~ ^[0-9]+$ ]]; then
            log "Agave catching up: ${result} slots behind"
            # If we were switched to local and now it's falling behind, rollback
            if [ "$SIMSERVER_SWITCHED" = true ] && [ "$result" -gt "$MAX_SLOTS_BEHIND" ]; then
                log "Agave fell behind threshold, rolling back sim-server"
                rollback_simserver
            fi
        else
            log "Agave status: ${result}"
        fi
    fi

    sleep "$POLL_INTERVAL"
done
