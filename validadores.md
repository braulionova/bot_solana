# Validators List — Helios Agave Fork
## Top Staked Mainnet Validators (2026-03-16)

Used for: `--known-validator` (repair/snapshot trust) + `--gossip-validator` (gossip priority)

| # | Pubkey | Stake (SOL) | Name/Label |
|---|--------|-------------|------------|
| 1 | `HEL1USMZKAL2odpNBj2oCjffnFGaYwmbGmyewGv1e2TU` | 14,537,418 | Helius |
| 2 | `Fd7btgySsrjuo25CJCj7oE7VPMyezDhnx7pZkj2v69Nk` | 13,299,309 | Galaxy |
| 3 | `DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy` | 11,666,021 | Coinbase |
| 4 | `5pPRHniefFjkiaArbGX3Y8NUysJmQ9tMZg3FrFGwHzSm` | 7,770,199 | Figment |
| 5 | `EvnRmnMrd69kFdbLMxWkTn1icZ7DCceRhvmb2SJXqDo4` | 7,387,951 | Everstake |
| 6 | `Awes4Tr6TX8JDzEhCZY2QVNimT6iD1zWHzf1vNyGvpLM` | 6,829,634 | Marinade |
| 7 | `CAo1dCGYrB6NhHh5xb1cGjUiu86iyCfMTENxgHumSve4` | 6,144,507 | Chorus One |
| 8 | `EkvdKhULbMFqjKBKotAzGi3kwMvMpYNDKJXXQQmi6C1f` | 3,952,429 | Triton |
| 9 | `FBKFWadXZJahGtFitAsBvbqh5968gLY7dMBBJUoUjeNi` | 3,866,364 | - |
| 10 | `BtsmiEEvnSuUnKxqXj2PZRYpPJAc7C34mGz8gtJ1DAaH` | 3,850,052 | - |
| 11 | `5ZqveVffQPiUbkjBg4KD9kib1MKHLqiFno4ke9jSq9qk` | 3,798,464 | - |

### Anza/Foundation Validators (always reliable)
| # | Pubkey | Role |
|---|--------|------|
| 12 | `7Np41oeYqPefeNQEHSv1UDhYrehxin3NStELsSKCT4K2` | Anza foundation |
| 13 | `GdnSyH3YtwcxFvQrVVJMm1JhTS4QVX7MFsX56uJLUfiZ` | Anza foundation |
| 14 | `DE1bawNcRJB9rVm3buyMVEdYoED72j2v1TTkdVi8oKEX` | Anza foundation |
| 15 | `CakcnaRDHka2gXyfbEd2d3xsvkJkqsLw2akB3zsN1D2S` | Anza foundation |
| 16 | `dv1ZAGvdsz5hHLwWXsVnM94hWf1pjbKVau1QVkaMJ92` | Anza/Solana Labs |
| 17 | `dv2eQHeP4RFrJZ6UeiZWoc3XTtmtZCUKxxCApCDcRNV` | Anza/Solana Labs |
| 18 | `Certusm1sa411sMpV9FPqU5dXAYhmmhygvxJ23S6hJ24` | Certus One |
| 19 | `SerGoB2ZUyi9A1uKqXCLYfWgscEg8dXpSHNjVPHkqGo` | Sergo |

## Usage in helios-agave.service

```bash
# Repair trust + snapshot download
--known-validator <PUBKEY>

# Gossip priority — push/pull only with these validators
# Results in faster ContactInfo updates = faster Turbine tree position
--gossip-validator <PUBKEY>
```

## How this helps

1. **Repair**: When catching up, Agave sends repair requests to known-validators first. More known-validators = more sources = faster repair.

2. **Gossip priority**: With `--gossip-validator`, the node only exchanges gossip with these validators. This means:
   - Faster ContactInfo updates (know leader TPU addresses sooner)
   - Better position in Turbine fanout tree (closer to high-stake nodes)
   - Less gossip noise from low-stake validators

3. **Snapshot trust**: Only downloads snapshots from known-validators, ensuring data integrity.

## Source
Query: `getVoteAccounts` on mainnet, sorted by `activatedStake` descending.
Date: 2026-03-16
