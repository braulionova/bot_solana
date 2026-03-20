# Helios Gold V2 — Solana MEV Arbitrage Bot

High-performance atomic arbitrage bot for Solana, powered by Turbine shred decoding for sub-100ms signal detection.

## Architecture

```
Turbine Shreds (UDP:8002)
    │
    ▼
┌─────────────────────────────┐
│  Spy Node                   │
│  FEC Assembly + Deshredder  │
│  3,000+ TXs/s decoded      │
│  100-200ms ahead of RPC     │
└─────────────┬───────────────┘
              ▼
┌─────────────────────────────┐
│  Market Engine              │
│  14 strategies in parallel  │
│  Pool cache (DashMap, <1μs) │
│  Opportunity scanner        │
└─────────────┬───────────────┘
              ▼
┌─────────────────────────────┐
│  Executor                   │
│  Flash loans (MarginFi 0%)  │
│  Jito bundles (0 cost fail) │
│  Blockhash from shreds      │
└─────────────────────────────┘
```

## Strategies

| # | Strategy | Source | Speed |
|---|----------|--------|-------|
| 1 | PumpSwap Graduation Arb | Shred-detected `create_pool` | ~100ms |
| 2 | Whale Swap Backrun | Shred-detected swaps >1% impact | ~75ms |
| 3 | CLMM Tick Crossing | Raydium CLMM vs AMM V4 tick divergence | ~50ms |
| 4 | Cross-DEX Cyclic Arb | Bellman-Ford negative cycles (3-hop) | ~10ms |
| 5 | Stablecoin Micro-Depeg | USDC/USDT price deviation across pools | ~100ms |
| 6 | Shred-Based Backrun | Every decoded swap → instant cross-DEX check | ~10ms |
| 7 | Opportunity Scanner | Continuous vault update → cross-DEX scan | ~1μs/check |
| 8 | Jupiter Limit Order Fill | Public limit orders fillable at market price | periodic |
| 9 | LP Remove Detection | Large liquidity removal → amplified impact | event |
| 10 | New Pool Monitor | Graduation → watch for 2nd pool creation | event |
| 11 | Oracle Front-Run | Pyth update → DEX price lag | planned |
| 12 | Liquidation Arb | Under-collateralized positions (MarginFi/Kamino) | planned |
| 13 | Volume Mining | PumpSwap fee rebate via self-trade | planned |
| 14 | Pool Validator | Background dead pool removal | every 5min |

## Key Features

- **Shred-first pipeline**: decode Turbine shreds 100-200ms before RPC/Geyser bots
- **Zero-RPC hot path**: blockhash derived from shreds, no RPC calls in execution
- **Jito-only execution**: failed TX = 0 SOL cost (atomic bundles)
- **Flash loans**: MarginFi 0% fee, Kamino, Solend, Port Finance
- **14 parallel strategies**: graduation arb, whale backrun, CLMM tick, stablecoin depeg, etc.
- **Pool validator**: automatically removes closed/dead pools from cache
- **Delta tracker**: 1,300+ vault updates/s from shred-decoded SPL transfers
- **Cross-DEX scanner**: 324 indexed pools across 108 token pairs
- **Per-pool fees**: Orca 1-200bps, Raydium CLMM variable, Meteora dynamic
- **Backrun calculator**: profit ceiling from user's slippage tolerance

## Project Structure

```
spy_node/
├── spy-node/src/           # Shred ingestion, FEC, TX decoder, gossip
├── market-engine/src/      # 14 strategy modules, pool cache, route engine
├── executor/src/           # TX builder, Jito sender, flash loans
├── helios/src/             # Main binary, pipeline wiring
├── ml-scorer/src/          # ML models (graduation predictor, landing predictor)
├── strategy-logger/src/    # PostgreSQL logging for ML training
├── programs/               # On-chain programs (helios-arb, port-flash-receiver)
├── sim-server/src/         # Local RPC proxy + simulation
└── agave-lite-core/src/    # Lightweight account cache
```

## Build

```bash
# Requirements: Rust 1.93+, libclang-18
cargo build --release

# Run
cp .env.example /etc/helios/helios.env
# Edit helios.env with your keys
systemctl start helios-bot
```

## Configuration

Copy `.env.example` to your environment file and configure:
- `PAYER_KEYPAIR`: path to your wallet keypair
- `JITO_ENDPOINT`: Jito block engine URL
- `DATABASE_URL`: PostgreSQL for ML training data
- `REDIS_URL`: Redis for pool metadata cache
- `DRY_RUN=true`: start in dry-run mode, set `false` for live execution

## Performance

| Metric | Value |
|--------|-------|
| Shred decode | 2,200+ shreds/s |
| TX decode | 3,000+ TXs/s |
| Vault updates (shreds) | 1,300/s |
| Blockhash freshness | ~400ms (from shreds) |
| Detection → on-wire | ~75ms |
| Jito bundle RTT | ~5ms (Frankfurt) |

## On-Chain Programs

- **helios-arb**: Multi-hop swap executor with flash loan integration
- **port-flash-receiver**: Port Finance flash loan callback receiver

## Historical Results

5 successful on-chain arbitrages (all PumpSwap, 14 UTC):
- Max profit: 0.306 SOL (single TX)
- Total: 0.386 SOL across 5 TXs
- Method: PumpSwap pool-to-pool atomic swap

---

## Support This Project

Building and running a competitive MEV bot on Solana requires significant infrastructure costs. Currently running on a budget VPS with SATA disks, which limits performance (Agave can't sync, no NVMe for accounts index).

**A dedicated NVMe server (~$200/month) would unlock:**
- Agave mainnet sync → Geyser push with <1ms vault updates
- 10x faster disk I/O → real-time account state
- Competitive with top MEV bots

**If you find this project useful or want to help it reach its full potential:**

**SOL donations**: `Hn87MEK6NLiGquWay8n151xXDN4o3xyvhwuRmge4tS1Y`

Every SOL helps keep the bot running and improves the infrastructure. Thank you!

---

## License

MIT

## Disclaimer

This software is provided for educational and research purposes. Use at your own risk. MEV extraction carries financial risk. Always start with `DRY_RUN=true`.
