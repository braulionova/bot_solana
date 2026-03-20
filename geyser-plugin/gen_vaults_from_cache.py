#!/usr/bin/env python3
"""Generate vaults.json by querying pool vault addresses from RPC.
Uses getMultipleAccounts to read pool account data and extract vault pubkeys.
"""
import json, struct, sys, base64, base58
import urllib.request

RPC_URL = "https://mainnet.helius-rpc.com/?api-key=YOUR_HELIUS_KEY"
OUTPUT = "/root/spy_node/geyser-plugin/vaults.json"

# Load mapped pools
with open("/root/solana-bot/mapped_pools.json") as f:
    mapped = json.load(f)

vaults = []
seen = set()

for p in mapped:
    pid = p.get("pool_id") or p.get("id") or p.get("address", "")
    va = p.get("vault_a") or p.get("tokenVaultA", "")
    vb = p.get("vault_b") or p.get("tokenVaultB", "")
    if va and va != "11111111111111111111111111111111" and va not in seen:
        vaults.append({"pubkey": va, "pool": pid, "side": "A"})
        seen.add(va)
    if vb and vb != "11111111111111111111111111111111" and vb not in seen:
        vaults.append({"pubkey": vb, "pool": pid, "side": "B"})
        seen.add(vb)

print(f"Found {len(vaults)} vaults from {len(mapped)} pools in mapped_pools.json", file=sys.stderr)

with open(OUTPUT, "w") as f:
    json.dump(vaults, f)

print(f"Written to {OUTPUT}", file=sys.stderr)
