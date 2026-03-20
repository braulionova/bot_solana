#!/bin/bash
# Fetch new pools from APIs and update mapped_pools.json
echo "Starting pool update at $(date)"
cd /root/spy_node
python3 fetch_pools.py 2>/dev/null || echo "fetch_pools.py failed or not found"

# Restart helios-bot (NOT helios!) to load new pools
echo "Restarting helios-bot service..."
killall -9 helios 2>/dev/null
sleep 2
systemctl start helios-bot
echo "Update complete at $(date)"
