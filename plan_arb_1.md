# Plan ARB-1: Estrategias de Arbitraje Rentables para Helios Gold V2

**Fecha**: 2026-03-19
**Objetivo**: Implementar las 6 estrategias viables identificadas en el análisis cruzado on-chain.
**Pipeline actual**: 87ms end-to-end (detect → vault refresh → build → send)
**Balance**: 0.054 SOL | **Costo por TX fallida (Jito)**: 0 SOL

---

## Datos del análisis on-chain (2026-03-19)

| Métrica | Valor |
|---------|-------|
| Whale swaps detectados | 250/min (~4/segundo) |
| Cross-DEX spreads detectados | 28,260 en 5 min |
| Graduaciones PumpSwap | ~3/min |
| Delta vault updates | 607/10s |
| Shreds decodificados | 2,235/s |
| TXs decodificadas | 3,035/s |
| Pools en grafo | 316 |
| Cross-DEX tokens | 99 (PumpSwap + Raydium) |
| Arbs exitosos históricos | 5 (todos PumpSwap, hora 14 UTC, pool GS4CU59F) |
| Rutas bloqueadas por ML | 796,871 (spread predictor defectuoso → bypassed) |

---

## Estrategia #1: BACKRUN WHALE SWAPS (Prioridad MÁXIMA)

### Por qué
- 250 whale swaps/min con >1% price impact detectados en shreds
- Cada whale swap desplaza el precio de una pool → spread temporal vs otra pool del mismo token
- Spread dura ~400ms (1 slot) antes de ser arbed
- **80% ya implementado**: detección en shreds ✅, cross-DEX scan ✅, Jito bundle ✅

### Qué falta
1. **Refresh RPC de la pool contrapartida** antes del cross-DEX scan (el whale swap actualiza pool A, pero pool B tiene reserves stale)
2. **Construir TX como Jito backrun bundle**: `[whale_tx, arb_tx]` para que el arb ejecute en el MISMO slot que el whale
3. **Calcular profit post-swap**: usar los reserves DESPUÉS del whale swap para cotizar el arb

### Implementación

```
Archivo: executor/src/whale_backrun.rs (NUEVO)

Flujo:
1. signal_processor detecta whale swap en shreds (slot N)
   → Emite SpySignal::WhaleSwap { pool, amount_in, a_to_b }

2. whale_backrun recibe el signal
   → Calcula reserves post-swap de pool A (apply_swap_delta locally)
   → Busca pool B (mismo token, distinto DEX) en pool_cache
   → RPC refresh pool B vaults (publicnode, 75ms)
   → Re-quote: comprar en pool barata, vender en pool cara
   → Si profit > min_profit → build TX

3. Build backrun TX:
   → Flash loan (MarginFi, 0% fee)
   → Swap hop 1: buy en pool barata (post-whale-swap price)
   → Swap hop 2: sell en pool cara (pre-whale-swap price = stale = favorable)
   → Repay flash loan
   → Jito tip (% of profit)

4. Submit como Jito bundle:
   → send_bundle([arb_tx]) — no necesitamos incluir whale_tx
   → Jito ordena TXs por tip, nuestro arb se inserta después del whale
   → Si whale TX no llega al mismo slot → nuestro arb falla → 0 cost

Latencia target: <150ms (detect 10ms + RPC 75ms + build 2ms + send 2ms + overhead 60ms)
```

### Código clave

```rust
// executor/src/whale_backrun.rs
pub struct WhaleBackrunner {
    pool_cache: Arc<PoolStateCache>,
    vault_pool: Arc<RpcPool>,      // publicnode + mainnet
    builder: Arc<TxBuilder>,
    sender: Arc<JitoSender>,
}

impl WhaleBackrunner {
    /// Called when whale swap detected in shreds.
    pub fn on_whale_swap(
        &self,
        pool_addr: Pubkey,
        amount_in: u64,
        a_to_b: bool,
        slot: u64,
    ) -> Option<Transaction> {
        let pool = self.pool_cache.get(&pool_addr)?;

        // 1. Simulate post-swap reserves locally
        let (post_ra, post_rb) = simulate_post_swap(&pool, amount_in, a_to_b);

        // 2. Find cross-DEX counterpart
        let token = if a_to_b { pool.token_b } else { pool.token_a };
        let counterparts = self.pool_cache.pools_for_token(&token);
        let best_counterpart = counterparts.iter()
            .filter(|p| p.pool_address != pool_addr)
            .filter(|p| p.reserve_a > 0 && p.reserve_b > 0)
            .max_by_key(|p| p.reserve_a.min(p.reserve_b))?;

        // 3. RPC refresh counterpart (75ms)
        self.refresh_pool_vaults(&best_counterpart.pool_address);
        let fresh_counterpart = self.pool_cache.get(&best_counterpart.pool_address)?;

        // 4. Quote arb: buy cheap pool → sell expensive pool
        let (buy_pool, sell_pool) = determine_direction(
            &pool, post_ra, post_rb,
            &fresh_counterpart,
        )?;

        // 5. Calculate profit
        let borrow = optimal_borrow_amount(buy_pool, sell_pool);
        let amount_out = quote_round_trip(buy_pool, sell_pool, borrow);
        let profit = amount_out as i64 - borrow as i64 - TX_FEE;

        if profit < MIN_PROFIT { return None; }

        // 6. Build + send via Jito
        let tx = self.build_arb_tx(buy_pool, sell_pool, borrow, profit)?;
        Some(tx)
    }
}
```

### Archivos a modificar/crear
- `executor/src/whale_backrun.rs` — NUEVO: lógica de backrun
- `executor/src/lib.rs` — agregar `pub mod whale_backrun`
- `helios/src/main.rs` — wiring: WhaleSwap signal → whale_backrun → Jito sender

### ROI estimado
- 250 whale swaps/min × 1% success rate × 0.01 SOL avg profit = **0.025 SOL/min = 1.5 SOL/hora**
- Con 5% success rate (optimista): **7.5 SOL/hora**

---

## Estrategia #2: RAYDIUM CLMM ↔ AMM V4 TICK CROSSING

### Por qué
- Tokens principales (SOL/USDC, SOL/USDT, SOL/RAY) tienen pools en CLMM Y AMM V4
- CLMM usa concentrated liquidity con ticks discretos
- Cuando un swap cruza un tick boundary, el precio salta discontinuamente
- AMM V4 (XY=K) no salta → spread temporal entre CLMM price y V4 price
- Ya decodificamos CLMM swaps en `swap_decoder.rs` pero lo excluimos del route engine

### Qué falta
1. **Agregar pools CLMM a mapped_pools.json** (actualmente 0 pools CLMM)
2. **Re-habilitar CLMM en route engine** (actualmente `DexType::RaydiumClmm => return false`)
3. **Detector de tick crossing**: cuando decoded swap muestra tick change, calcular nuevo CLMM price
4. **Cross-check CLMM vs V4**: comparar precio post-tick-crossing con V4 price

### Implementación

```
Archivo: market-engine/src/clmm_tick_arb.rs (NUEVO)

Flujo:
1. swap_decoder detecta Raydium CLMM swap en shreds
   → Extrae: pool, amount_in, sqrt_price_limit

2. clmm_tick_arb recibe el swap
   → Lee tick_current_index del pool cache
   → Simula swap: calcula nuevo tick_current_index
   → Si tick cambió → calcula nuevo CLMM price

3. Compara CLMM price con AMM V4 price para el mismo token pair
   → Si spread > 50bps (25bps CLMM fee + 25bps V4 fee)
   → Build arb route: buy en pool barata → sell en pool cara

4. Submit via Jito bundle

Latencia target: <100ms (detect in shred, no RPC needed for CLMM state)
```

### Archivos a modificar/crear
- `market-engine/src/clmm_tick_arb.rs` — NUEVO: detector de tick crossing + arb calculator
- `market-engine/src/lib.rs` — agregar `pub mod clmm_tick_arb`
- `market-engine/src/route_engine.rs` — re-habilitar `RaydiumClmm` en `is_pool_tradeable()`
- `market-engine/src/pool_hydrator.rs` — agregar CLMM pool discovery via `getProgramAccounts`
- Script: `fetch_clmm_pools.py` — one-time fetch de CLMM pools para mapped_pools.json

### ROI estimado
- ~50 tick crossings/hora en pares principales
- × 10% capturables (competencia media) × 0.01 SOL avg = **0.05 SOL/hora**

---

## Estrategia #3: JUPITER LIMIT ORDER FILL

### Por qué
- Jupiter DCA y Limit Orders crean órdenes pendientes on-chain (públicas)
- Cuando el precio de mercado cruza el precio límite, cualquiera puede "fill" la orden
- El filler compra al precio de mercado + vende al precio límite → spread = profit
- Alta frecuencia (~100 fills/hora), bajo profit individual (0.001-0.01 SOL)

### Qué falta
1. **Decoder de Jupiter Limit Orders** — nuevo decoder en `swap_decoder.rs`
2. **Monitor de órdenes pendientes** — scan de `JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4` accounts
3. **Fill executor** — construir TX que llena la orden al precio de mercado

### Implementación

```
Archivo: market-engine/src/jupiter_limit_fill.rs (NUEVO)

Flujo:
1. Periodic scan: getProgramAccounts de Jupiter Limit Order program
   → Obtener todas las órdenes pendientes con sus precios límite
   → Indexar por token pair

2. En cada slot, comparar precio de mercado (de pool cache) vs precios límite
   → Si precio de mercado cruzó el límite → opportunity

3. Build fill TX:
   → Buy token al precio de mercado (DEX swap)
   → Fill la orden Jupiter al precio límite (CPI a Jupiter)
   → Pocket la diferencia

4. Submit via Jito

Programa Jupiter Limit: jupoNjAxXgZ4rjzxzPMP4oxduvQsQtQVG7YU6LFHX9k (v2)
```

### Archivos a crear
- `market-engine/src/jupiter_limit_fill.rs` — NUEVO
- Script: `fetch_jupiter_orders.py` — scan de órdenes pendientes

### ROI estimado
- ~100 fills/hora × 0.003 SOL avg = **0.3 SOL/hora**

---

## Estrategia #4: LIQUIDATION ARB (MarginFi/Kamino)

### Por qué
- Positions bajo-colateralizadas en lending protocols pueden ser liquidadas
- Liquidador recibe 5-10% bonus sobre el collateral
- Flash loan → liquidate → swap collateral → repay → pocket bonus
- MUY rentable en mercados volátiles (0.1-5 SOL por liquidación)

### Qué falta
- `liquidation_scanner.rs` Phase A existe (monitor de health factors)
- Phase B: TX builder para liquidación no implementado
- Necesita: decode MarginFi account state → calcular health factor → build liquidate IX

### Implementación

```
Archivo: executor/src/liquidation_executor.rs (NUEVO)

Flujo:
1. liquidation_scanner (ya existe) monitorea health factors via getProgramAccounts
   → Accounts con health_factor < 1.0 → candidate for liquidation

2. liquidation_executor recibe candidate
   → Flash borrow (MarginFi/Kamino)
   → CPI: lending_protocol.liquidate(borrower, repay_amount)
   → Recibe collateral con bonus (5-10%)
   → Swap collateral → SOL
   → Repay flash loan
   → Pocket: collateral_bonus - swap_slippage - fees

3. Submit via Jito

Programas:
  MarginFi:  MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA
  Kamino:    KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD
  Solend:    So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo
```

### ROI estimado
- ~10 liquidaciones/día × 0.5 SOL avg = **5 SOL/día** (solo en mercados volátiles)

---

## Estrategia #5: PUMPSWAP VOLUME MINING

### Por qué
- PumpSwap incentiva volumen con PUMP token rewards
- Self-trade: buy + sell mismo token en misma pool
- Costo: 50bps round-trip (0.005 SOL per SOL traded)
- Si PUMP reward > 0.5% del volumen → profit neto

### Qué falta
- Verificar economía del PUMP token (precio, reward rate)
- Implementar self-trade loop
- Monitor de reward acumulado

### Implementación

```
Archivo: executor/src/volume_miner.rs (NUEVO)

Flujo:
1. Check PUMP token price y reward rate
2. Si profitable: ejecutar loop de buy+sell en pools con alta liquidez
3. Cada N operaciones: claim PUMP rewards
4. Sell PUMP → SOL

Cálculo break-even:
  reward_per_sol_volume × PUMP_price_in_SOL > 0.005 SOL
```

### ROI estimado
- Depende enteramente del PUMP token economics
- Si reward rate = 1% → profit neto 0.5% per round trip = **escalable**

---

## Estrategia #6: STABLECOIN MICRO-DEPEG ARB

### Por qué
- 107 SOL/USDC PumpSwap pools + 1 Raydium SOL/USDC pool
- Cualquier fluctuación en USDC price crea micro-spread entre pools
- PumpSwap pools actualizan precio más lento que Raydium → window
- Zero risk (stablecoins revert to peg)

### Qué falta
- Monitor de USDC/USDT price deviation (Pyth oracle vs DEX price)
- Trigger cuando depeg > 30bps
- Seleccionar la PumpSwap pool más líquida para el arb

### Implementación

```
Archivo: market-engine/src/stablecoin_arb.rs (NUEVO)

Flujo:
1. Monitor Pyth USDC/USD oracle price (via shreds o HTTP)
2. Cuando price deviation > 30bps from $1.00:
   → Si USDC cheap: buy USDC en pool barata → sell en pool cara
   → Si USDC expensive: buy SOL en pool USDC cara → sell SOL en pool USDC barata
3. Submit via Jito

Target pools:
  Raydium SOL/USDC: 58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2
  PumpSwap SOL/USDC: 107 pools (seleccionar top 5 por liquidez)
```

### ROI estimado
- ~5 depeg events/día × 0.02 SOL avg = **0.1 SOL/día** (baseline)
- En eventos de volatilidad alta: 10-50× más

---

## Orden de implementación (por ROI/esfuerzo)

| Prioridad | Estrategia | Esfuerzo | ROI estimado | Dependencias |
|-----------|-----------|----------|-------------|-------------|
| P0 | #1 Backrun Whale | 1-2 días | 1-10 SOL/h | RPC pool (ya tenemos) |
| P1 | #2 CLMM Tick Crossing | 2-3 días | 0.05-2.5 SOL/h | CLMM pools discovery |
| P1 | #6 Stablecoin Depeg | 1 día | 0.1 SOL/d baseline | Pyth oracle monitor |
| P2 | #5 Volume Mining | 0.5 días | ? (depende de PUMP) | PUMP token economics |
| P2 | #3 Jupiter Limit Fill | 3-5 días | 0.3 SOL/h | Jupiter order decoder |
| P3 | #4 Liquidation | 5-7 días | 5 SOL/d (volatile) | MarginFi account parser |

---

## Infraestructura compartida necesaria

### 1. RPC Pool mejorado
```
VAULT_RPC_URLS=https://solana-rpc.publicnode.com,https://api.mainnet-beta.solana.com
```
- Agregar más endpoints conforme se descubran
- Cuando Agave local sincronice: `http://127.0.0.1:9000` como primary (0ms latency)

### 2. Blockhash de shreds (YA IMPLEMENTADO ✅)
- 0 RPC para blockhash, ~400ms freshness
- Usado por todas las estrategias

### 3. Jito bundle sender (YA IMPLEMENTADO ✅)
- Multi-region (Frankfurt, Amsterdam, NY, mainnet)
- JITO_ONLY=true → 0 cost on failure

### 4. Flash loan (YA IMPLEMENTADO ✅)
- MarginFi: 0% fee, up to 10% of pool liquidity
- Save/Solend, Kamino: backup providers

### 5. Pool cache con delta tracking (YA IMPLEMENTADO ✅)
- 3,774 vaults tracked from shreds
- Instant cross-DEX check on every swap

---

## Métricas de éxito

| Métrica | Target semana 1 | Target mes 1 |
|---------|----------------|-------------|
| TXs landed | 1 | 50 |
| Profit total | 0.1 SOL | 5 SOL |
| Win rate | >0% | >5% |
| Latencia avg | <150ms | <100ms |
| Balance | >0.054 SOL | >1 SOL |
| Estrategias activas | 2 (#1 + #6) | 4 |
