# Plan de Modelos de IA para Arbitraje Exitoso
## Helios Gold V2 — ML Pipeline

---

## 1. Datos Disponibles

| Dataset | Rows | Ubicación | Contenido |
|---|---|---|---|
| `execution_events` | 141,988 | SQLite `/root/solana-bot/solana_bot.db` | Cada TX intentada: stage/status/latency/route/reason_code |
| `missed_opportunities` | 252,501 | SQLite `/root/solana-bot/solana_bot.db` | Opps detectadas no ejecutadas: profit/spread/dex_path/liquidity |
| `opportunities` | 255 | SQLite | Opps ejecutadas con resultados |
| `tokens` | 11,019 | SQLite | Token metadata (mint, symbol) |
| `pool_discoveries` | 119,788 | PG `helios` | Pools on-chain (78K Raydium, 26K Orca, 14K Meteora) |
| `hot_routes` | 494 | PG `helios` | Rutas ML-scored con competition/uniqueness |
| `historical_executions` | 141,988 | PG `helios` | Importado de SQLite para queries rápidos |
| `historical_missed` | 50,000 | PG `helios` | Subset importado de missed_opportunities |
| `mapped_pools.json` | 6,578 | JSON | Pools con vaults, fees, mints, decimals |
| `ml_model_state` | 2 models | PG `helios` | turbo_sender (274K samples), route_scorer (62) |
| **Shred deltas** | real-time | In-memory | Strategy A: optimistic reserves from shred simulation |

## 2. Insights Clave (del análisis de datos)

### TXs Confirmadas (4 exitosas)
- Todas `_free_→PumpAmm` (PumpSwap graduation snipe)
- Horas: 13-14 UTC
- Latencia promedio: 560ms
- Ruta: compra de token recién graduado

### Missed Opportunities (252K)
- **70K Orca→Orca**: profit promedio 3.78 SOL — enormes pero fantasma (reserves stale)
- **8K Orca→Ray cross-DEX**: profit promedio 1.21 SOL
- **78K con profit >10M lamports**: mayoría fantasma
- **54K blocked_single_pool**: no hay segundo pool para cross-dex
- **14K blocked_by_liquidity**: pool sin liquidez suficiente
- **11K blocked_by_profit**: profit negativo al re-quotear

### Execution Pipeline
- **48K passed_jupiter_validation**: oportunidades reales que pasaron filtros
- **15 attempted submit**: solo 15 llegaron a envío real
- **4 confirmed**: 4/15 = 26.7% landing rate cuando se envía
- Horas peak: 12-15 UTC (80% de los intentos)

### Pool Universe
- 78K Raydium V4, 26K Orca Whirlpool, 14K Meteora DLMM
- 6,578 pools activas con metadata completa
- Hot routes: 494 scored, solo 4 con competition < 0.5

---

## 3. Los 6 Modelos

### Modelo #1: Landing Predictor
**Tipo**: Logistic Regression / Lightweight Neural Net
**Propósito**: Predecir P(TX aterriza on-chain sin error)
**Impacto**: Filtrar rutas que nunca aterrizarán → ahorrar bandwidth

**Features (18)**:
```
route_type          : categorical (cross-dex, bellman-ford, hot-route, whale-backrun)
dex_combo           : categorical hash (Orca+Ray, Meteora+Meteora, etc.)
n_hops              : int (2, 3)
estimated_profit    : float (lamports)
tip_pct             : float (% of profit)
detection_ms        : float (signal age)
hour_utc            : int (0-23)
competition_score   : float (0-1, from hot_routes)
min_pool_liquidity  : float (USD)
reserve_freshness_ms: float (age of last reserve update)
spread_bps          : int
signal_type         : categorical (tx, liq, whale, graduation)
pool_age_slots      : int (slots since pool creation)
historical_rate     : float (pool's historical landing rate)
jito_tip_percentile : float (our tip vs p75)
n_pools_for_pair    : int (competition proxy)
max_price_impact    : float (%)
is_optimistic_reserve: bool (using shred delta vs Helius)
```

**Training Data**: 141K execution_events (4 positive, 141K negative → use SMOTE resampling)
**Inference**: ~50ns (dot product + sigmoid)
**Threshold**: P(land) > 0.15 → attempt

---

### Modelo #2: Route Profitability Estimator (PRIORIDAD ALTA)
**Tipo**: Gradient Boosted Trees (LightGBM-style, implemented in Rust)
**Propósito**: Estimar el profit REAL de una ruta (vs el profit estimado que es inflado por reserves stale)
**Impacto**: Descartar rutas "fantasma" → solo enviar las reales

**Features (12)**:
```
estimated_profit    : float (from BF/cross-dex quote)
reserve_a_age_ms    : float (time since last update)
reserve_b_age_ms    : float
is_optimistic       : bool (shred delta vs RPC poll)
dex_type_hop1       : categorical
dex_type_hop2       : categorical
spread_bps          : int
pool_liquidity_usd  : float
reserve_a_velocity  : float (|delta| per second from shred tracker)
reserve_b_velocity  : float
historical_requote_fail_rate: float (% of times this route fails requote)
n_swaps_last_10s    : int (activity from shred tracker)
```

**Target**: `real_profit / estimated_profit` (ratio, 0.0 = phantom, 1.0 = accurate)

**Training Data**:
- 252K missed_opportunities: estimated profit known
- Match with execution_events: actual result known
- 48K passed_jupiter_validation: were the estimates accurate?
- Cross-reference with shred delta freshness

**Decision Rule**:
```
predicted_ratio = model.predict(features)
adjusted_profit = estimated_profit * predicted_ratio
if adjusted_profit > MIN_NET_PROFIT:
    send_bundle()
else:
    skip("phantom route")
```

**Implementation**: Rust, in-process, <1ms inference

---

### Modelo #3: Optimal Tip Selector
**Tipo**: Quantile Regression
**Propósito**: Calcular el tip exacto que maximiza EV = P(land) × (profit - tip)
**Impacto**: Ganar auctions de Jito sin overpay

**Features (8)**:
```
estimated_profit    : float
competition_score   : float
hour_utc            : int
n_hops              : int
jito_p75_tip        : float (current)
route_type          : categorical
historical_tip_success: float (EMA of tips that landed)
slot_in_leader      : int (position within 4-slot leader window)
```

**Target**: `tip_lamports` que minimiza cost y maximiza landing

**Training Data**:
- 15 submit attempts (4 landed) — sparse pero informativo
- Jito tip floor history (p75 from API, logged every 10s)
- On-chain arb analysis (competitor tips from onchain_arbs)

**Strategy**:
```
low_profit  (<500K):  tip = max(p75, 15% of profit)
med_profit  (<5M):    tip = max(p75, 20% of profit)
high_profit (>5M):    tip = max(p75, 30% of profit)
ultra_comp  (>0.8):   tip += 50% bonus
peak_hours  (13-14):  tip += 20% bonus
```

---

### Modelo #4: Pool Volatility Classifier
**Tipo**: Online Streaming (EMA + thresholds)
**Propósito**: Clasificar pools por volatilidad → pools volátiles = más oportunidades reales
**Impacto**: Priorizar monitoreo de pools high-volatility

**Features (6)**:
```
reserve_a_velocity  : float (|delta_reserve_a| per second)
reserve_b_velocity  : float
swap_frequency      : float (swaps per minute from shred tracker)
price_cv            : float (coefficient of variation of price ratio)
mean_reversion_ms   : float (half-life of reserve reversion)
activity_ratio      : float (shred_updates / expected_updates)
```

**Classes**: HIGH (>10 swaps/min), MEDIUM (2-10), LOW (<2), DEAD (0)

**Training Data**: pool_snapshots time-series + shred delta tracker stats

**Usage**:
```rust
if pool.volatility == HIGH {
    // Check this pool every signal
    // Use optimistic reserves (Strategy A)
    // Lower min_profit threshold
} else if pool.volatility == LOW {
    // Check only every 5th signal
    // Use Helius reserves (more accurate for stable pools)
}
```

---

### Modelo #5: Competition Detector (PRIORIDAD ALTA)
**Tipo**: Anomaly Detection (Isolation Forest-style, implemented as simple heuristics)
**Propósito**: Detectar cuándo otros bots son más rápidos en una ruta → evitarla
**Impacto**: No perder tiempo en rutas donde siempre perdemos

**Features (7)**:
```
route_key           : string (pool_a + pool_b hash)
n_attempts_24h      : int
n_landed_24h        : int
n_requote_fails_24h : int
avg_detection_ms    : float
time_between_fails  : float (ms, lower = more competition)
jito_rejection_rate : float
```

**Rules** (heuristic, no training needed):
```
COMPETITION_BLACKLIST if:
  - n_attempts > 20 AND n_landed == 0          → never lands, skip
  - n_requote_fails > 50 AND n_landed == 0     → always stale, skip
  - avg_detection_ms > 200 AND n_landed == 0   → too slow for this route
  - jito_rejection_rate > 0.9                   → always outbid

COMPETITION_REDUCE if:
  - n_attempts > 10 AND landing_rate < 0.05     → reduce priority 50%
  - time_between_fails < 1000ms                 → fast competition

COMPETITION_OK if:
  - n_attempts < 5                              → not enough data, try
  - landing_rate > 0.1                          → viable route
```

**Training Data**: 141K execution_events + real-time accumulation

**Implementation**: DashMap in-memory, no ML library needed, ~100ns lookup

---

### Modelo #6: Reserve Freshness Estimator (PRIORIDAD CRÍTICA)
**Tipo**: Kalman Filter
**Propósito**: Fusionar shred deltas (Strategy A) con Helius polling para obtener la mejor estimación de reserves reales
**Impacto**: Resuelve `bf_rejected_requote=3` — EL BLOQUEADOR PRINCIPAL

**State**:
```
x = [reserve_a, reserve_b]  // estimated true reserves
P = [[var_a, 0], [0, var_b]] // uncertainty
```

**Prediction Step** (on shred delta):
```
// Shred tells us delta_a, delta_b from swap instruction
x_predicted = x + [delta_a, delta_b]
P_predicted = P + Q  // Q = process noise (shred simulation error)
// Q is higher for Orca/Meteora (we don't simulate their math)
```

**Update Step** (on Helius poll):
```
// Helius tells us actual reserve values
z = [helius_reserve_a, helius_reserve_b]
K = P_predicted / (P_predicted + R)  // R = measurement noise (Helius delay)
x_updated = x_predicted + K * (z - x_predicted)
P_updated = (I - K) * P_predicted
```

**Reconciliation**:
```
// If shred estimate diverges >10% from Helius → trust Helius (shred had drop)
if abs(x_optimistic - x_helius) / x_helius > 0.10:
    x = x_helius  // reset
    log_discrepancy()  // track for model improvement
```

**Implementation**: Per-pool Kalman state in DexPool struct

**Training Data**: Implicit — Q and R matrices learned from discrepancy rates

---

## 4. Arquitectura de ML Pipeline

```
┌──────────────────────────────────────────────────────────────────┐
│                    Helios ML Pipeline                             │
│                                                                   │
│  ┌─────────────────┐    ┌──────────────────┐                     │
│  │ Data Sources     │    │ Feature Store     │                    │
│  │                  │    │ (DashMap in-mem)  │                    │
│  │ • Shred deltas   │───▶│ • pool_features   │                   │
│  │ • Helius polls   │    │ • route_features  │                   │
│  │ • Jito results   │    │ • time_features   │                   │
│  │ • PG historical  │    │ • competition_map │                   │
│  └─────────────────┘    └────────┬─────────┘                    │
│                                  │                                │
│         ┌────────────────────────┼────────────────────────┐      │
│         ▼                        ▼                        ▼      │
│  ┌──────────────┐    ┌──────────────────┐    ┌──────────────┐   │
│  │ #6 Kalman    │    │ #2 Profitability │    │ #5 Competition│   │
│  │ Reserve Est. │    │ Estimator        │    │ Detector      │   │
│  │ (per-pool)   │    │ (per-route)      │    │ (per-route)   │   │
│  └──────┬───────┘    └────────┬─────────┘    └──────┬───────┘   │
│         │                     │                      │           │
│         ▼                     ▼                      ▼           │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │                    Decision Engine                           │ │
│  │                                                              │ │
│  │  kalman_reserves = model6.estimate(pool)                    │ │
│  │  real_profit = model2.predict(route, kalman_reserves)       │ │
│  │  competition = model5.score(route)                          │ │
│  │  tip = model3.recommend(profit, competition, hour)          │ │
│  │  p_land = model1.predict(all_features)                      │ │
│  │                                                              │ │
│  │  EV = p_land * (real_profit - tip)                          │ │
│  │  if EV > threshold AND competition != BLACKLIST:            │ │
│  │      build_tx(tip) → turbo_sender.send()                   │ │
│  └─────────────────────────────────────────────────────────────┘ │
│                                                                   │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │ Feedback Loop (post-execution)                               │ │
│  │                                                              │ │
│  │  on_tx_result(sig, landed, actual_profit):                  │ │
│  │    model1.observe(features, landed)                         │ │
│  │    model2.observe(estimated_profit, actual_profit)          │ │
│  │    model3.observe(tip, landed)                              │ │
│  │    model5.observe(route, landed, detection_ms)              │ │
│  │    turbo_learner.observe(endpoint, landed, latency)         │ │
│  │    save_to_pg(every 5 min)                                  │ │
│  └─────────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────┘
```

---

## 5. Orden de Implementación

| Fase | Modelo | Impacto | Esfuerzo | Datos |
|---|---|---|---|---|
| **1** | #6 Kalman Reserve | **CRÍTICO** — resuelve requote | 3h | Shred + Helius |
| **2** | #5 Competition Detector | **ALTO** — evita rutas perdidas | 1h | 141K events |
| **3** | #2 Profitability Estimator | **ALTO** — filtra fantasmas | 4h | 252K missed |
| **4** | #3 Optimal Tip | **MEDIO** — mejora landing | 2h | Jito data |
| **5** | #4 Pool Volatility | **MEDIO** — prioriza pools | 2h | Shred deltas |
| **6** | #1 Landing Predictor | **MEDIO** — filtra envíos | 3h | 141K events |

---

## 6. Métricas de Éxito

| Métrica | Actual | Target |
|---|---|---|
| `bf_rejected_requote` rate | ~100% | <30% |
| Landing rate (sent → confirmed) | 0% (arb), 26.7% (snipe) | >10% |
| Detection to on-wire | 18-43ms | <20ms |
| Phantom route filter rate | 0% | >80% |
| Monthly profit | 0 SOL | >1 SOL |
| Daily attempts | ~50 | >200 |
| Competition blacklist accuracy | N/A | >90% |

---

## 7. Archivos Existentes

| Archivo | Módulo | Estado |
|---|---|---|
| `ml-scorer/src/lib.rs` | MlScorer (pool EMA) | ✅ Activo |
| `ml-scorer/src/landing_predictor.rs` | Modelo #1 base | ✅ Parcial |
| `ml-scorer/src/onchain_scanner.rs` | Competitor analysis | ✅ Activo |
| `ml-scorer/src/snapshot_scanner.rs` | Hot route scanner | ✅ Activo |
| `ml-scorer/src/pool_discoverer.rs` | Auto-discovery | ✅ Compilado |
| `executor/src/turbo_learner.rs` | Endpoint learning | ✅ Compilado |
| `executor/src/turbo_sender.rs` | Multi-path relay | ✅ Activo |
| `market-engine/src/shred_delta_tracker.rs` | Strategy A deltas | ✅ Activo |
| `market-engine/src/scoring.rs` | Route scoring | ✅ Activo |
| `ml-scorer/src/bin/train_turbo.rs` | Training binary | ✅ Trained 274K |

---

## 8. Dependencias Técnicas

- **No se necesitan ML frameworks externos** — todo implementable con:
  - EMA (exponential moving average) para online learning
  - DashMap para feature stores lock-free
  - Kalman filter: 2x2 matrix math (manual, ~10 lines)
  - Heurísticas para competition detection
  - Gradient boosted trees: lookup tables trained offline

- **Latencia de inferencia target**: <1ms por modelo, <5ms total pipeline
- **Memoria**: ~50MB para todos los modelos en memoria
- **Persistencia**: PostgreSQL `ml_model_state` table (JSON weights)
