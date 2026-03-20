# Plan de Ingresos – Helios Arb Bot
**Fecha inicio:** 2026-03-10 | **Actualizado:** 2026-03-10

---

## Diagnóstico: Por qué no hay ingresos hoy

### Bug #1 — CRÍTICO: Raydium pool hydration completamente rota
- **Causa:** `decode_raydium_amm()` valida vault_a/vault_b decodificados contra los del JSON (que son todos `11111...` = Pubkey::default()). Nunca coinciden → returns None → ningún pool Raydium tiene `raydium_meta` → sus vaults nunca se añaden a `all_vaults` → **reserves_a/b se quedan en 0** para los 3729 pools Raydium.
- **Impacto:** 3729/5681 pools = 65% del universo de pools sin datos. El grafo Bellman-Ford no tiene aristas → 0 rutas encontradas.
- **Fix:** Eliminar la validación en `decode_raydium_amm()`. Añadir vault_a/b de raydium_meta al `all_vaults` dict.

### Bug #2 — CRÍTICO: Pool state no se actualiza desde shreds
- **Causa:** El `signal_processor.rs` detecta swaps pero NO actualiza el caché de reservas. Sólo hace `estimate_swap_impact()` con datos obsoletos.
- **Impacto:** Por mucho que lleguen shreds, el estado de los pools sólo se actualiza cada 30s via RPC. El arb detectado está siempre desactualizado.
- **Fix:** Llamar `swap_decoder::decode_swaps(tx)` en signal_processor y aplicar XY=K para actualizar reservas en el acto (latencia 0ms).

### Bug #3 — ALTO: Orca y Meteora no tienen estado inicial
- **Causa:** Orca: vaults se leen correctamente del account data en `refresh_all`, pero `tick_arrays` pueden quedar vacíos si el Orca whirlpool tiene sqrt_price/liquidity = 0.
- **Meteora:** Los 607 pools con vaults reales deben funcionar — verificar si `decode_meteora_dlmm` trabaja correctamente.
- **Fix:** Prioridad baja tras fix #1.

### Bug #4 — ALTO: Oracle filter elimina pools válidos
- **Causa:** `pool_is_eligible()` usa `MIN_KNOWN_TVL_USD = 10_000` cuando el oracle tiene precios. Pools de tokens nuevos/exóticos no tienen precio oracle → filtrados.
- **Fix:** Solo aplicar filtro oracle si el pool tiene AMBOS tokens con precio oracle. Si solo uno, usar ese lado * 2 como lower bound.

### Bug #5 — MEDIO: swap_generation no cambia = route cache siempre stale
- **Causa:** Si no hay swaps decodificados desde shreds, `swap_generation` nunca incrementa. El route cache retorna `Vec::new()` del warmup.
- **Fix:** Forzar invalidación del cache cuando llega cualquier señal DEX.

---

## Plan de Acción

### Fase 1: Arreglar hidratación (HECHO en progreso)
- [x] Diagn óstico completo
- [ ] Fix decode_raydium_amm — sin validación de vaults placeholder
- [ ] Fix step-1d — añadir raydium vault_a/b a all_vaults
- [ ] Fix step-1c — skip Pubkey::default() vaults
- [ ] Validar que pools Raydium tienen reserves > 0 tras refresh

### Fase 2: Hot-path pool update desde shreds (latencia máxima)
- [ ] `signal_processor.rs` llama `swap_decoder::decode_swaps(tx)`
- [ ] `pool_state.rs` → `update_reserves_from_swap(pool, amount_in, a_to_b)` con XY=K
- [ ] Al detectar swap: incrementar swap_generation para invalidar route cache
- [ ] Trigger targeted RPC refresh de solo ese pool (< 50ms vs 30s)

### Fase 3: Route engine → encontrar rutas reales
- [ ] Verificar que grafo tiene > 100 aristas con reserves reales
- [ ] Bajar MIN_NET_PROFIT_LAMPORTS de 10_000 a 1_000 temporalmente para testing
- [ ] Log detallado: cuántos pools válidos, cuántas aristas, cuántos ciclos detectados
- [ ] Confirmar "arbitrage opportunity found" en logs

### Fase 4: Execution pipeline completo
- [ ] sim-server corriendo en localhost:8081
- [ ] Preflight in-process funcionando (ya implementado)
- [ ] Simulación exitosa de al menos 1 ruta
- [ ] DRY_RUN=true → confirmar "DRY RUN – would send bundle" en logs
- [ ] Verificar TX size ≤ 1232 bytes
- [ ] Cambiar DRY_RUN=false cuando simulación sea consistentemente exitosa

### Fase 5: Optimización de latencia (ya parcialmente implementado)
- [x] Preflight in-process (~0-5µs)
- [x] TPU direct fanout (4 líderes paralelo a Jito)
- [x] sim-server blockhash cache (~0ms)
- [ ] sim-server BanksClient simulation (~30ms vs 80-150ms)
- [ ] Reducir refresh interval de pool hydrator de 30s a 10s
- [ ] Targeted single-pool refresh en < 50ms tras swap detection

---

## Métricas de Éxito

| Métrica | Actual | Objetivo Fase 1 | Objetivo Final |
|---|---|---|---|
| Pools con reserves > 0 | ~607 (solo Meteora) | > 4000 | > 5000 |
| Rutas encontradas/señal | 0 | > 0 | > 5 |
| Tiempo detección→envío | N/A | < 500ms | < 100ms |
| Simulaciones exitosas/día | 0 | > 10 | > 100 |
| Bundles enviados/día | 0 | > 0 (dry run) | > 50 |
| Profit neto/día | 0 SOL | N/A | > 0.1 SOL |

---

## Reglas de Riesgo (INVIOLABLES)
1. MAX daily loss: 5% working capital. Si > 0.5 SOL en tips fallidos → pausar 24h
2. Simulación SIEMPRE antes de enviar
3. Flash loan max: 10% de liquidez del proveedor
4. Solo ejecutar si profit neto > 2× costos totales
5. Never hold: posición atómica via Jito, máximo 1 bloque

---

## Estado Actual (2026-03-10)
- Binarios compilados: helios (6.3M), executor (5.6M), spy-node (5.5M)
- Shreds recibidos: ~634/s via Turbine (verificado con latency probe)
- Pipeline: UDP ingest ✅ → shred parser ✅ → sig_verifier ✅ → deduper ✅ → FEC ✅ → TX decoder ✅ → signal_processor ✅ → route_engine ❌ (0 rutas) → executor ❌ (no llega)
- DRY_RUN=true (producción segura)
- TPU fanout: implementado, pendiente activar con rutas reales
- Preflight in-process: implementado
