# Estrategias Implementadas — Helios MEV Bot

## Estado General
Pipeline completo en produccion LIVE (`DRY_RUN=false`). Todas las estrategias compilan y se ejecutan. Falta el primer trade landed exitoso.

---

## E1: Graduation Sniper (Fast Path)

**Flujo**: `SpySignal::Graduation` → build PumpSwap buy TX → TPU fanout + Jito bundle

**Estado**: ACTIVO — 8+ bundles enviados, ninguno confirmado landed.

**Implementacion**:
- Detecta migracion PumpFun bonding curve → PumpSwap AMM via discriminator `create_pool`
- Canal dedicado `grad_tx` bypasses route engine (latencia minima)
- TX directa: compra 0.05 SOL del token recien graduado
- Envio paralelo: Jito Frankfurt + TPU QUIC fanout a 4 lideres

**Problemas encontrados**:
1. **Jito Searcher no autorizado** — bundles reciben 429. TPU fanout es el unico canal activo.
2. **Latencia Turbine 669ms** — detectamos la graduacion ~669ms despues del slot. Otros bots con ShredStream (50ms) nos ganan.
3. **Sin confirmacion de landing** — puede que los bundles lleguen pero sin Jito auth no podemos verificar status.

---

## E2: Bellman-Ford Cyclic Arb (Multi-hop)

**Flujo**: `SpySignal::NewTransaction` → pool cache update → Bellman-Ford negative cycle detection → flash loan TX → simulate → send

**Estado**: PARCIALMENTE ACTIVO — pipeline funcional, cycles_found=0 consistentemente.

**Implementacion**:
- Grafo dirigido con ~4100 pools (Raydium V4, Orca Whirlpool, Meteora DLMM, PumpSwap)
- Edge weights = `-log(rate)` donde rate = amount_out/amount_in
- BF corre solo desde WSOL/USDC/USDT (3 anchor tokens, no desde los 4100+ nodos)
- MAX_HOPS=3 (A→B→C→A)
- Multi-borrow: prueba 0.1, 0.5, 1.0 SOL por ciclo
- Flash loan wrapping: MarginFi → Kamino → Solend → Port (fallback chain)
- Simulacion obligatoria antes de envio

**Problemas encontrados**:
1. **ref_amount era 0.001 SOL** — CORREGIDO (2026-03-12). A este nivel, fees e integer division dominaban los edge weights, produciendo 0 cycles. Ahora usa 0.1 SOL (100M lamports) o 0.1 USDC (100K units).
2. **RaydiumClmm phantom cycles** — CORREGIDO (2026-03-12). Nuestra formula XY=K es incorrecta para pools de liquidez concentrada (CLMM). Generaba cycles fantasma con returns imposibles. Fix: excluir RaydiumClmm del grafo.
3. **PumpSwap no soportado en TX builder** — CORREGIDO (2026-03-12). Rutas que pasaban por PumpSwap pools fallaban con `bail!()`. Ahora implementado con layout de 18 accounts verificado en mainnet.
4. **ALT creation blocking** — CORREGIDO (2026-03-12). Primera oportunidad real perdida porque ALT creation tomo 24 segundos. Fix: `try_get_or_spawn_create()` en background thread.
5. **Bellman-Ford spam** — CORREGIDO (2026-03-12). Cada signal triggeaba BF completo incluso cuando el ultimo run encontro 0 cycles 100ms atras. Fix: cooldown 500ms + swap_gen delta < 10.
6. **Simulacion secuencial** — CORREGIDO (2026-03-12). Providers se simulaban uno por uno (80-150ms cada uno). Fix: top-2 providers en paralelo con `thread::scope`.
7. **Token program cache miss** — CORREGIDO (2026-03-12). `token_programs_for_route()` llamaba RPC en cada build (20-100ms). Fix: DashMap cache con TTL 300s.
8. **Latencia de deteccion** — 669ms via Turbine directo. Con ShredStream seria 50ms. Bloqueado por whitelist de Jito.
9. **Pool quotes posiblemente stale** — quotes del grafo se basan en reserves del hydrator (refresh cada 60s). Las reserves reales pueden cambiar mucho en 60s en pools activos.

---

## E3: Whale Backrun

**Flujo**: `SpySignal::WhaleSwap` (>1% pool impact) → route engine → arb opportunity

**Estado**: ACTIVO via route engine — whale swaps detectados consistentemente (14-270/s), pero se procesan por el mismo pipeline BF de E2.

**Implementacion**:
- `SignalProcessor` detecta swaps con >1% de impacto en un pool
- Emite `SpySignal::WhaleSwap` que alimenta al route engine
- El route engine busca ciclos de arb aprovechando el desequilibrio post-whale

**Problemas encontrados**:
1. Mismos problemas que E2 (cycles_found=0 por ref_amount/CLMM, ahora corregidos)
2. La latencia total (deteccion + BF + build + simulate + send) puede ser >1s, lo cual elimina la ventaja del backrun

---

## E4: Liquidity Event Arb

**Flujo**: `SpySignal::LiquidityEvent` (add/remove LP) → route engine → arb opportunity

**Estado**: Wired en pipeline, sin eventos detectados activamente.

**Implementacion**:
- Detecta adds/removes de liquidez que desbalancean pools
- Misma pipeline que E2/E3

---

## E5: Cross-DEX Same-Pair Arb

**Flujo**: Scan directo de pares identicos en diferentes DEX → comparar precios → arb

**Estado**: ACTIVO — `cross_dex::scan_cross_dex_opportunities()` corre despues de cada BF.

**Implementacion**:
- Sin BF — comparacion directa de quotes para el mismo par de tokens en pools de diferentes DEX
- Mas rapido que BF pero solo encuentra arb simple (2-hop: comprar en DEX-A, vender en DEX-B)

**Problemas encontrados**:
1. Depende de la precision de las quotes en cache (misma limitacion que E2)

---

## E6: SQLite Route Feed

**Flujo**: Lee rutas pre-calculadas de `solana_bot.db` → ejecuta directamente

**Estado**: DISPONIBLE pero dependiente de solana-bot (actualmente DISABLED).

---

## Problemas Transversales

### Latencia
| Componente | Actual | Optimo | Bloqueador |
|---|---|---|---|
| Shred detection | 669ms | 50ms | Jito ShredStream whitelist |
| Route engine BF | <50ms | <50ms | OK |
| TX build | 5-50ms | 5ms | mint program cache (CORREGIDO) |
| Simulation | 80-150ms | 30-60ms | Agave local catchup |
| Send (Jito+TPU) | 18-1238ms | 18ms | Jito Searcher auth |
| **Total** | **~900-2100ms** | **~150-250ms** | ShredStream + Jito auth |

### Blockers criticos
1. **Jito Searcher auth**: registrar `Hn87MEK6NLiGquWay8n151xXDN4o3xyvhwuRmge4tS1Y` en jito.network
2. **Jito ShredStream whitelist**: registrar `H1xo5JpUd36GJDhkNapDHsZkg6Xt57FPsbCRS1QQZWjZ` → 669ms→50ms
3. **Agave catchup**: ~22K slots behind, `wait-agave-catchup.sh` monitorea

### Fixes aplicados hoy (2026-03-12, sesion actual)
1. `ref_amount` 1M → 100M lamports (0.001→0.1 SOL) para edge weights precisos
2. RaydiumClmm excluido del grafo (XY=K quoting incorrecto para CLMM)
3. PumpSwap soporte completo en TX builder (18 accounts, buy/sell discriminators)
4. Mint→token_program DashMap cache (TTL 300s, ahorra 100-500ms/oportunidad)
5. BF cooldown (500ms + swap_gen delta check, reduce CPU waste)
6. Simulacion paralela top-2 providers (ahorra 80-150ms cuando primer provider falla)
7. Diagnosticos de conectividad del grafo (anchor_connected count)
