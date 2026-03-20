import json
import urllib.request
import os

MAPPED_POOLS_FILE = '/root/solana-bot/mapped_pools.json'

def load_existing():
    if os.path.exists(MAPPED_POOLS_FILE):
        try:
            with open(MAPPED_POOLS_FILE, 'r') as f:
                return json.load(f)
        except:
            return []
    return []

def save_pools(pools):
    with open(MAPPED_POOLS_FILE, 'w') as f:
        json.dump(pools, f, indent=2)

def fetch_raydium():
    print("Fetching Raydium pools...")
    try:
        req = urllib.request.Request(
            "https://api.raydium.io/v2/main/pairs",
            headers={'User-Agent': 'Mozilla/5.0'}
        )
        with urllib.request.urlopen(req, timeout=15) as response:
            data = json.loads(response.read().decode('utf-8'))
        
        print(f"Got {len(data)} Raydium pools")
        
        new_pools = []
        for p in data:
            if p.get('liquidity', 0) < 10000:
                continue
                
            new_pools.append({
                "pool_id": p["ammId"],
                "dex": "Raydium V4",
                "symbol": p["name"],
                "vault_a": p.get("baseVault", "11111111111111111111111111111111"),
                "vault_b": p.get("quoteVault", "11111111111111111111111111111111"),
                "mint_a": p["baseMint"],
                "mint_b": p["quoteMint"],
                "decimals_a": p.get("baseDecimals", 9),
                "decimals_b": p.get("quoteDecimals", 6),
                "fee_bps": 25
            })
        return new_pools
    except Exception as e:
        print(f"Error fetching Raydium: {e}")
        return []

def fetch_orca():
    print("Fetching Orca pools...")
    try:
        req = urllib.request.Request(
            "https://api.mainnet-orca.so/v1/whirlpool/list",
            headers={'User-Agent': 'Mozilla/5.0'}
        )
        with urllib.request.urlopen(req, timeout=15) as response:
            data = json.loads(response.read().decode('utf-8'))
        
        whirlpools = data.get('whirlpools', [])
        print(f"Got {len(whirlpools)} Orca pools")
        
        new_pools = []
        for p in whirlpools:
            if p.get('tvl', 0) < 1000:
                continue
            
            new_pools.append({
                "pool_id": p["address"],
                "dex": "Orca",
                "symbol": f"{p['tokenA']['symbol']}-{p['tokenB']['symbol']}",
                "vault_a": "11111111111111111111111111111111",
                "vault_b": "11111111111111111111111111111111",
                "mint_a": p["tokenA"]["mint"],
                "mint_b": p["tokenB"]["mint"],
                "decimals_a": p["tokenA"]["decimals"],
                "decimals_b": p["tokenB"]["decimals"],
                "fee_bps": int(p["lpFeeRate"] * 10000)
            })
        return new_pools
    except Exception as e:
        print(f"Error fetching Orca: {e}")
        return []

def fetch_meteora():
    print("Fetching Meteora DLMM pools...")
    try:
        req = urllib.request.Request(
            "https://dlmm-api.meteora.ag/pair/all",
            headers={'User-Agent': 'Mozilla/5.0'}
        )
        with urllib.request.urlopen(req, timeout=15) as response:
            data = json.loads(response.read().decode('utf-8'))
        
        print(f"Got {len(data)} Meteora pools")
        
        new_pools = []
        for p in data:
            try:
                liquidity = float(p.get('liquidity', 0))
            except:
                liquidity = 0
                
            if liquidity < 10000:
                continue
                
            new_pools.append({
                "pool_id": p["address"],
                "dex": "Meteora DLMM",
                "symbol": p["name"],
                "vault_a": p["reserve_x"],
                "vault_b": p["reserve_y"],
                "mint_a": p["mint_x"],
                "mint_b": p["mint_y"],
                "decimals_a": 9,
                "decimals_b": 6,
                "fee_bps": int(float(p.get("base_fee_percentage", 0.3)) * 100)
            })
        return new_pools
    except Exception as e:
        print(f"Error fetching Meteora: {e}")
        return []

def main():
    existing = load_existing()
    existing_ids = set(p['pool_id'] for p in existing)
    print(f"Existing pools: {len(existing)}")
    
    raydium = fetch_raydium()
    orca = fetch_orca()
    meteora = fetch_meteora()
    
    added = 0
    for p in raydium + orca + meteora:
        if p['pool_id'] not in existing_ids:
            existing.append(p)
            existing_ids.add(p['pool_id'])
            added += 1
            
    print(f"Added {added} new pools.")
    print(f"Total pools: {len(existing)}")
    save_pools(existing)

if __name__ == "__main__":
    main()
