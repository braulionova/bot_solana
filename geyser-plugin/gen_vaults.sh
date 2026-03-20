#!/bin/bash
# Generate vaults.json from the current pool cache (via PG hot routes + mapped_pools)
# Run this after pool hydration to update the vault list for the Geyser plugin.
set -e

PGPASSWORD=helios_ml_2026 psql -U helios -d helios_ml -h localhost -t -A -c "
WITH top_pools AS (
    SELECT oh.pool, oh.dex_type,
           count(*) as freq, avg(o.net_profit)::bigint as avg_profit
    FROM opportunity_hops oh
    JOIN opportunities o ON o.id = oh.opp_db_id
    WHERE o.net_profit > 50000
    GROUP BY oh.pool, oh.dex_type
    ORDER BY count(*) * avg(o.net_profit) DESC
    LIMIT 200
)
SELECT json_agg(json_build_object('pool', pool, 'dex_type', dex_type))
FROM top_pools;
" > /tmp/top_pools.json

# Use a Python script to resolve vault addresses from mapped_pools.json
python3 - << 'PYEOF'
import json, sys

# Load top pools from PG
with open('/tmp/top_pools.json') as f:
    content = f.read().strip()
    if not content or content == '':
        print("No pools found in PG", file=sys.stderr)
        sys.exit(1)
    top_pools = json.loads(content)

# Load mapped pools for vault addresses
try:
    with open('/root/solana-bot/mapped_pools.json') as f:
        mapped = json.load(f)
except:
    mapped = []

# Build pool→vaults mapping
pool_vaults = {}
for p in mapped:
    pid = p.get('pool_id') or p.get('id') or p.get('address', '')
    va = p.get('vault_a') or p.get('tokenVaultA', '')
    vb = p.get('vault_b') or p.get('tokenVaultB', '')
    if pid and (va or vb):
        pool_vaults[pid] = (va, vb)

# Generate vaults.json
vaults = []
seen = set()
for tp in (top_pools or []):
    pool = tp['pool']
    if pool in pool_vaults:
        va, vb = pool_vaults[pool]
        if va and va not in seen:
            vaults.append({"pubkey": va, "pool": pool, "side": "A"})
            seen.add(va)
        if vb and vb not in seen:
            vaults.append({"pubkey": vb, "pool": pool, "side": "B"})
            seen.add(vb)

print(f"Generated {len(vaults)} vault entries from {len(top_pools or [])} pools", file=sys.stderr)
with open('/root/spy_node/geyser-plugin/vaults.json', 'w') as f:
    json.dump(vaults, f, indent=2)
print(f"Written to /root/spy_node/geyser-plugin/vaults.json", file=sys.stderr)
PYEOF

echo "Done: $(wc -l < /root/spy_node/geyser-plugin/vaults.json) lines in vaults.json"
