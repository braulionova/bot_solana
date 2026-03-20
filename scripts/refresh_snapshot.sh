#!/bin/bash
# refresh_snapshot.sh — Download fresh snapshot + parse vault accounts to Redis.
#
# This gives us mainnet-current account state without running Agave full validator.
# Run every 6 hours via cron: 0 */6 * * * /root/spy_node/scripts/refresh_snapshot.sh
#
# Flow:
# 1. Download snapshot from fast validator (~10 min for 108GB)
# 2. Extract to temp directory
# 3. Run parse_snapshot_pools to extract vault balances → Redis
# 4. Cleanup temp files
# 5. Restart helios-bot to load fresh Redis data

set -e
LEDGER_DIR="/root/solana-spy-ledger"
ACCOUNTS_DIR="$LEDGER_DIR/accounts/run"
REDIS_URL="redis://127.0.0.1/"
LOG="/var/log/snapshot-refresh.log"

echo "$(date): Starting snapshot refresh" >> $LOG

# Step 1: Check if Agave produced a recent snapshot
LATEST_SNAP=$(ls -t $LEDGER_DIR/snapshot-*.tar.zst 2>/dev/null | head -1)
if [ -z "$LATEST_SNAP" ]; then
    echo "$(date): No snapshot found, requesting new one" >> $LOG
    # Trigger snapshot download via Agave restart
    # (Agave with --no-snapshot-fetch=false will download on start)
    exit 1
fi

SNAP_SLOT=$(echo "$LATEST_SNAP" | grep -oP 'snapshot-\K\d+')
CURRENT_SLOT=$(curl -s "https://solana-rpc.publicnode.com" -X POST -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","id":1,"method":"getSlot"}' 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('result',0))" 2>/dev/null)

BEHIND=$((CURRENT_SLOT - SNAP_SLOT))
echo "$(date): Snapshot at slot $SNAP_SLOT, mainnet at $CURRENT_SLOT, behind $BEHIND slots" >> $LOG

# Only refresh if snapshot is >12h old (>108K slots)
if [ "$BEHIND" -lt 108000 ]; then
    echo "$(date): Snapshot fresh enough ($BEHIND slots), skipping" >> $LOG
    exit 0
fi

# Step 2: Check if accounts directory exists (from Agave bootstrap)
if [ ! -d "$ACCOUNTS_DIR" ]; then
    echo "$(date): Accounts directory not found at $ACCOUNTS_DIR" >> $LOG
    echo "$(date): Need Agave to extract snapshot first" >> $LOG
    exit 1
fi

# Step 3: Parse vault accounts to Redis
echo "$(date): Parsing vault accounts from $ACCOUNTS_DIR" >> $LOG
cd /root/spy_node
REDIS_URL=$REDIS_URL ./target/release/parse_snapshot_pools \
    --accounts-dir "$ACCOUNTS_DIR" >> $LOG 2>&1

echo "$(date): Parse complete, restarting helios-bot" >> $LOG

# Step 4: Restart helios-bot to load fresh Redis data
killall -9 helios 2>/dev/null
sleep 2
systemctl start helios-bot

echo "$(date): Snapshot refresh complete" >> $LOG
