# Plan: 4 Estrategias Adicionales para Helios Bot

## Orden de implementación por ROI/esfuerzo

---

## Estrategia 1: Whale Backrun (E3) — PRIORIDAD ALTA
**Esfuerzo**: Bajo | **Frecuencia**: 10-50/día | **Profit**: 0.1-0.5% del swap

### Concepto
Whale hace swap de 50 SOL en Raydium → precio se mueve 2% → compramos en Orca (precio viejo) → vendemos en Raydium (precio nuevo).

### Ya tenemos
- `SignalProcessor` detecta `WhaleSwap` (impact >= 1%)
- Cross-DEX scanner con `scan_cross_dex_opportunities()`
- FAST_ARB executor (27ms)

### Implementar
1. **`market-engine/src/cross_dex.rs`** — Nueva función `scan_pair_opportunities(pool_cache, token_a, token_b, borrow_amounts)`:
   - Filtra solo pools del par afectado por el whale
   - Misma lógica que `scan_cross_dex_opportunities` pero focalizada
   - Strategy label: `"whale-backrun"`

2. **`helios/src/main.rs`** — En el route engine loop, cuando `sig == WhaleSwap`:
   - Extraer pool → lookup tokens del pool en cache
   - Llamar `scan_pair_opportunities()` con esos tokens
   - Merge resultados con rutas existentes

3. **`market-engine/src/pool_state.rs`** — Agregar `get_pool_tokens(pool) -> Option<(Pubkey, Pubkey)>`

### Scoring
- Confidence boost 1.1x (la dislocación de precio está confirmada por el whale TX)

---

## Estrategia 2: Arbitraje de Staking Derivatives (LST) — PRIORIDAD ALTA
**Esfuerzo**: Medio | **Frecuencia**: 30-100/día | **Profit**: 0.01-0.05 SOL por trade

### Concepto
mSOL/SOL, jitoSOL/SOL, bSOL/SOL divergen 0.1-0.3% entre DEXes constantemente. Son low-risk porque el precio está "anclado" al staking rate.

### LST Mints (mainnet)
```
mSOL:    mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So
jitoSOL: J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn
bSOL:    bSo13r4TkiE4KumL71LsHTPpL2euBYLFx6h9HP3piy1
stSOL:   7dHbWXmci3dT8UFYWYZweBLXgycu7Y3iL6trKn1Y7ARj
INF:     5oVNBeEEQvYi1cX3ir8Dx5n1P7pdxydbGF2X4TxVusJm
```

### Implementar
1. **`market-engine/src/lst_scanner.rs`** (NUEVO) — Scanner dedicado:
   - Array `LST_MINTS` con las 5+ direcciones
   - `scan_lst_opportunities(pool_cache, borrow_amounts) -> Vec<RouteParams>`
   - Threshold más bajo: `MIN_MARGIN_ABOVE_FEES_BPS = 10` (0.1% vs 0.3% del cross-dex)
   - Reutiliza `build_cross_dex_route` / `build_cross_dex_route_reverse` de cross_dex.rs
   - Strategy: `"lst-arb"`

2. **`market-engine/src/cross_dex.rs`** — Hacer públicas:
   - `pub fn build_cross_dex_route()`
   - `pub fn build_cross_dex_route_reverse()`
   - `pub fn dex_fee_bps()`
   - `pub fn dex_program_for()`

3. **`market-engine/src/lib.rs`** — `pub mod lst_scanner;`

4. **`helios/src/main.rs`** — Llamar `lst_scanner::scan_lst_opportunities()` cada ciclo del route engine (junto al cross-dex scan)

### Scoring
- Confidence boost 1.15x (baja volatilidad, precio anclado)
- Risk factor bajo (0.1)

---

## Estrategia 3: Sniping Multi-Launchpad — PRIORIDAD MEDIA
**Esfuerzo**: Medio | **Frecuencia**: 5-20/día | **Profit**: variable (auto-sell protege capital)

### Concepto
Expandir graduation sniper a LaunchLab (BONK.fun) y Moonshot. Ya detectamos las migraciones — solo falta construir TXs de compra para cada DEX destino.

### Ya tenemos
- `swap_decoder.rs` con discriminators: `LAUNCHLAB_MIGRATE_AMM_DISC`, `LAUNCHLAB_MIGRATE_CPSWAP_DISC`, `MOONSHOT_MIGRATE_CAMEL_DISC`, `MOONSHOT_MIGRATE_SNAKE_DISC`
- `detect_graduation()` ya maneja PumpSwap + LaunchLab + Moonshot
- `signal_processor.rs` emite `SpySignal::Graduation { source }` con la fuente correcta

### Implementar
1. **`executor/src/sniper.rs`** — Refactorizar `snipe()`:
   - Recibir `source` parameter
   - Match en source:
     - `"PumpSwap"` → existente `build_pumpswap_buy_ix()`
     - `"LaunchLab"` → nuevo `build_raydium_buy_ix()` (Raydium AMM V4 swap, discriminator byte 9)
     - `"Moonshot"` → nuevo `build_generic_buy_ix()` (query pool type post-migration)
   - Auto-sell aplica igual para todas las fuentes

2. **`executor/src/sniper.rs`** — Nuevas funciones:
   - `build_raydium_v4_buy_ix(pool, token_mint, payer, amount)` — Raydium AMM V4 swap con account layout conocido
   - Fetch pool state via `getMultipleAccounts` (vaults, market accounts)

3. **`helios/src/main.rs`** — Pasar `source` a `sniper.snipe()` (ya disponible en el destructure del signal)

### Notas
- LaunchLab migra a Raydium AMM V4 o CPSwap — el pool address ya viene en `GraduationEvent.pump_pool`
- Moonshot: pool destino necesita discovery post-migración (+100ms latencia)
- BONK.fun (LaunchLab) tiene buen volumen actualmente

---

## Estrategia 4: Liquidaciones DeFi — PRIORIDAD BAJA
**Esfuerzo**: Alto | **Frecuencia**: 1-10/día | **Profit**: 5-10% del colateral liquidado

### Concepto
Monitorear posiciones undercollateralized en MarginFi/Kamino. Cuando health < 1.0: flash-borrow debt token → repay deuda → recibir colateral con descuento → swap colateral → repay flash loan.

### Fase A: Monitor (detection only)
1. **`market-engine/src/liquidation_scanner.rs`** (NUEVO):
   - Poll periódico de obligation accounts (MarginFi primero)
   - Parse account data → calcular health factor
   - `health = weighted_collateral / weighted_borrow`
   - Cuando `health < 1.0`, emitir `LiquidationOpportunity`
   - Poll cada 5-10s via Agave local RPC (sin rate limits)

2. **`spy-node/src/signal_bus.rs`** — Nuevo variant:
   ```rust
   Liquidation {
       slot: u64,
       obligation: Pubkey,
       protocol: &'static str,
       collateral_mint: Pubkey,
       debt_mint: Pubkey,
       max_amount: u64,
   }
   ```

3. **`helios/src/main.rs`** — Thread `liquidation-monitor` con polling loop

### Fase B: Executor
4. **`executor/src/liquidator.rs`** (NUEVO):
   - Build TX: flash-borrow → liquidate → swap colateral → repay
   - Account layout MarginFi `liquidate` instruction (Anchor IDL)
   - Reusa `TxBuilder` para ATAs y `JitoSender`/`TpuSender` para envío

### MarginFi Account Layout (liquidate)
```
Program: MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA
Accounts: [liquidator, obligation, lending_market, reserve_collateral, reserve_debt, ...]
Discriminator: sha256("global:liquidate")[..8]
```

### Notas
- Implementar Fase A primero → validar frecuencia de oportunidades antes de construir executor
- MarginFi es más simple que Kamino (account layout más directo)
- Requiere desserializar obligation accounts de Anchor (borsh)

---

## Resumen de Prioridades

| # | Estrategia | Esfuerzo | Frecuencia | Profit/trade | Dependencia |
|---|---|---|---|---|---|
| 1 | Whale Backrun | Bajo | 10-50/día | 0.01-0.5 SOL | Ninguna |
| 2 | LST Arb | Medio | 30-100/día | 0.001-0.05 SOL | Ninguna |
| 3 | Multi-launchpad | Medio | 5-20/día | Variable | Ninguna |
| 4 | Liquidaciones | Alto | 1-10/día | 0.1-5 SOL | Fase A→B |

## Archivos a crear
- `market-engine/src/lst_scanner.rs`
- `market-engine/src/liquidation_scanner.rs` (Fase A)
- `executor/src/liquidator.rs` (Fase B)

## Archivos a modificar
- `market-engine/src/cross_dex.rs` — hacer funciones pub + scan_pair_opportunities
- `market-engine/src/lib.rs` — pub mod lst_scanner, liquidation_scanner
- `executor/src/sniper.rs` — multi-launchpad dispatch
- `helios/src/main.rs` — whale backrun trigger, LST scanner, source→sniper, liquidation thread
- `market-engine/src/scoring.rs` — strategy-specific confidence
- `spy-node/src/signal_bus.rs` — Liquidation variant
