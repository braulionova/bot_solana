# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

---

## REGLA DE ORO #0: Solo Arbitraje con Flash Loans — PROHIBIDO Sniper

**Helios Gold V2 es EXCLUSIVAMENTE un bot de arbitraje atómico con flash loans.**

- **PROHIBIDO**: graduation sniping, token sniping, compra especulativa de tokens, hold de posiciones.
- **PERMITIDO**: arbitraje cross-DEX, arbitraje cíclico (2-3 hops), LST arb, DLMM bin imbalance — siempre con flash loan atómico.
- **Principio**: toda operación debe ser atómica en una sola TX. Si la TX falla = 0 costo. Si tiene profit = profit. Nunca holdear tokens.
- **SNIPE_SOL_AMOUNT=0**: el sniper debe estar permanentemente deshabilitado.
- **No implementar**: estrategias de compra/venta, trailing stops, auto-sell, hold timers, ni nada que implique posiciones abiertas.

---

## REGLA DE ORO #1: Agave NO Necesita Estar Sincronizado para Funcionar

**Agave NO necesita estar en el tip de mainnet para ser útil. Solo necesita estar en replay activo.**

- Richat gRPC funciona con replay activo — emite account updates en cada slot replayeado, no solo en el tip.
- `getMultipleAccounts` RPC funciona aunque esté miles de slots detrás — los accounts del snapshot son válidos para metadata estática (market accounts, vaults, tick_spacing).
- **NUNCA esperar a que Agave catchee al tip** para usarlo. Usar inmediatamente cuando RPC responda.
- El flujo: snapshot → bank state → replay → Geyser plugin → Richat gRPC → bot conecta a localhost.
- Pool metadata (Raydium market accounts, Orca vaults) es ESTÁTICA — no cambia entre slots. Un snapshot de hace horas sigue teniendo metadata válida.
- Solo los vault BALANCES (reserves) cambian cada slot — y esos vienen de shreds via delta tracker, no de Agave.
- **Config óptima**: `--no-voting --no-snapshot-fetch --full-rpc-api --geyser-plugin-config richat.json`

---

## REGLA DE ORO #2: PumpSwap Graduation Arb = Prioridad #1

**PumpSwap graduation es el ÚNICO arbitraje confirmado rentable. 5 TXs on-chain, 0.386 SOL profit.**

**HALLAZGO CRÍTICO (2026-03-19):** Los 5 arbs exitosos fueron **PumpSwap↔PumpSwap** (NO cross-DEX). Eran arbs entre 2 pools PumpSwap del mismo token con precios diferentes. Las pools eran efímeras (cerradas después).

- Arbs exitosos: **PumpSwap↔PumpSwap same-token** (no PumpSwap→Raydium como se asumía)
- Spreads duran **1-5 segundos** (pool nuevo sin historial de arbitraje)
- Flash loan atómico: comprar barato en pool PumpSwap A → vender caro en pool PumpSwap B
- Fuentes: PumpSwap `create_pool`, PumpFun `migrate`, LaunchLab `migrate_to_amm`/`migrate_to_cpswap`, Moonshot `migrateFunds`
- Processing order: **graduation FIRST**, antes de BF/cross-dex/cyclic
- Signal age: **2000ms** para graduation (vs 200ms para otros)
- Tip: **30%** del profit (agresivo, alta convicción)
- Min spread: **20bps** (flash loan = 0 risk)
- Hora óptima: **14 UTC** (US market open + EU afternoon)
- Same-DEX arb habilitado (PumpSwap↔PumpSwap con reserves REALES, no estimadas)
- Cross-DEX spreads estáticos = 0% (arbed por bots más rápidos en <1s)
- Solo arb de **EVENTOS** funciona (graduación, nueva pool, swap grande)
- **Todas las demás estrategias son secundarias** a graduation arb

---

## REGLA DE ORO #3: SIEMPRE Matar Zombies Antes de Reiniciar

**`killall -9 helios` ANTES de `systemctl start helios-bot`. SIEMPRE.**

**Causa raíz encontrada (2026-03-19):** `helios.service` (servicio DUPLICADO) tenía `Restart=always` y creaba el zombie cada vez. Ahora está `systemctl disable --now helios.service`. **NUNCA re-habilitarlo.**

El binario helios crea procesos zombie que roban 50% de shreds UDP → pipeline muerto → 0 TXs decodificadas → 0 graduaciones detectadas → 0 arbs.

```bash
# Protocolo OBLIGATORIO de restart:
killall -9 helios 2>/dev/null && sleep 2
systemctl start helios-bot && sleep 5
MAIN=$(systemctl show helios-bot -p MainPID --value)
ps -C helios --no-headers -o pid | while read p; do [ "$p" != "$MAIN" ] && kill -9 $p; done
systemctl start shred-mirror
```

- NUNCA usar solo `systemctl restart helios-bot` (crea zombie)
- NUNCA usar `sed` para editar archivos Rust (corrompe macros)
- SIEMPRE verificar `ps -C helios | wc -l` = 1 después de restart

---

## REGLA DE ORO #4: Optimizar Todo para Baja Latencia

**Cada decisión de diseño e implementación debe minimizar latencia end-to-end.**

Principios obligatorios:
- **Zero-copy donde sea posible**: evitar clones/allocaciones en hot paths; usar referencias, slices, `Arc` en lugar de copias.
- **Allocator mimalloc**: usar `mimalloc = { version = "0.1", default-features = false }` como allocator global en todos los binarios. Reduce jitter vs glibc malloc.
- **CPU pinning**: todo thread crítico (UDP recv, shred parser, signal processor) debe usar `core_affinity` para pinnar a un core dedicado.
- **recvmmsg en Linux**: el listener UDP DEBE usar `recvmmsg(2)` en batch (ya implementado en solana-bot). Nunca usar `recv_from` en el loop caliente.
- **Canales bounded crossbeam**: usar `crossbeam_channel::bounded` (no `unbounded`) en todo el pipeline. Si un canal está lleno → drop del paquete (backpressure > OOM).
- **Perfil release estricto**: siempre compilar con `lto="fat"`, `codegen-units=1`, `panic="abort"`, `opt-level=3`, `overflow-checks=false`.
- **No syscalls en hot path**: evitar `std::time::SystemTime::now()` en tight loops; usar RDTSC o timestamps al inicio del batch.
- **Sin locks en hot path**: preferir `dashmap` sobre `Mutex<HashMap>`, `crossbeam` sobre `std::sync::mpsc`.
- **Evitar async en hot path de shreds**: el pipeline UDP → parser → signal_bus corre en threads OS dedicados, no en tokio. Tokio solo para I/O de red auxiliar (RPC calls, Jito API, Telegram).
- **Simulate antes de enviar**: `simulateTransaction` obligatorio antes de cada bundle. Una TX fallida cuesta 0 SOL pero un bundle enviado sin simular puede perder el tip.

---

## Recursos Disponibles en Este VPS (reutilizar antes de escribir)

### Código Base: `/root/solana-bot/`

**Este proyecto tiene IMPLEMENTADO el 80% del spy node.** Antes de escribir cualquier módulo, revisar si ya existe aquí.

| Módulo spy_node | Código reutilizable en solana-bot |
|---|---|
| `udp_ingest` | `src/shred/udp_listener.rs` (232 líneas) — recvmmsg, SO_REUSEPORT, CPU pin |
| `shred_parser` | `src/shred/shred_parser.rs` (208 líneas) — Legacy + Merkle, offsets exactos |
| `fec_assembler` | `src/shred/fec_accumulator.rs` (207 líneas) — FEC sets, RS recovery |
| `reed_solomon` | `src/shred/reed_solomon.rs` (109 líneas) |
| `tx_decoder` | `src/shred/tx_decoder.rs` (261 líneas) + `entry_decoder.rs` (92) |
| `signal_bus` / pipeline | `src/shred/arb_pipeline.rs` (107 líneas) — ArbSignal, watch_set filter |
| `route_engine` | `src/dex/graph.rs` (220 líneas) — PriceGraph, petgraph DiGraph |
| DEX integrations | `src/dex/` — raydium.rs, orca.rs, meteora.rs, pumpswap.rs (2542 líneas total) |
| Flash loans | `src/flash_loan/marginfi.rs` |
| Jito sender | `src/jito/bundle.rs` + `src/shred/jito_client.rs` |
| Scanner signals | `src/scanner/` — pump_fun_monitor, pumpswap_monitor, cross_dex_scanner |
| On-chain program | `programs/helium-gold-v2/` — `arb_with_flash` instruction completa |

**Dependencias ya resueltas** (Cargo.toml en solana-bot, copiar directamente):
```toml
solana-sdk = "1.18.26"          # versión estable, sin git deps
anchor-lang = "0.30"
petgraph = "0.8.3"
reed-solomon-erasure = "6"
crossbeam-channel = "0.5"
dashmap = "5"
core_affinity = "0.8"
mimalloc = { version = "0.1", default-features = false }
yellowstone-grpc-client = "1.14.0"
zeroize = "=1.3.0"              # PIN obligatorio — conflicto solana-perf vs rustls
```

**Perfil release** (ya validado en solana-bot, copiar):
```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
strip = "symbols"
overflow-checks = false
debug = false
```

### Herramientas Instaladas
- **Rust** 1.93.1 | **Cargo** 1.93.1
- **Solana CLI** 3.1.9 (Agave) | **Anchor CLI** 0.32.1
- **Node.js** v22.22.0 (nvm)
- **libclang** 18 (requerido para bindgen/solana crates)
- **psql** (PostgreSQL client) | **jq**
- **git** con anchors en caché: `~/.cargo/git/db/anchor-*`, `crossbeam-*`

### Ledger de Referencia: `/root/solana-spy-ledger/`
Contiene snapshots de mainnet-beta. Útil para testing offline del parser de shreds y entry decoder sin conectar a la red.

### Bot anterior activo: `/root/solana-bot/`
- Tiene `solana_bot.db` (SQLite) con pool states y whale wallets
- `mapped_pools.json` — pools mapeados con cuentas vault
- `top_routes.json` — rutas precalculadas
- `auth.json` — keypair del bot (NO copiar a spy_node, referenciar desde config)

---

## Arquitectura del Spy Node

Pipeline de 4 capas: `Spy Node → Market Engine → helios-arb (onchain) → Jito Sender`

```
UDP:8002 (Turbine) + UDP:20000 (Jito ShredStream)
  → udp_ingest [thread dedicado, recvmmsg, CPU pin]
  → shred_parser [slot, index, fec_set_index, variant]
  → sig_verifier [Ed25519, leader pubkey from gossip]
  → deduper [LRU 10K, key=hash(slot+index+type)]
  → fec_assembler [Reed-Solomon 32+32, 50% loss tolerance]
  → tx_decoder [bincode entries, ALT cache]
  → signal_bus [crossbeam bounded → SpySignal]
       ↓
  signal_processor [DEX filter, whale detect, graduation detect]
  → route_engine [petgraph + Bellman-Ford desde WSOL/USDC/USDT]
  → scoring [score = (profit/cost)*confidence*urgency*(1-risk)]
  → flash_selector [MarginFi 0% > Kamino 0.01% > Port 0.09% > Solend 0.3%]
       ↓
  executor [flash wrapper TX-level: provider borrow → helios-arb route CPI → provider repay]
       ↓
  helios-arb program [solo swap_hops + profit check sobre ATA destino]
       ↓
  Jito Block Engine [simulate → bundle → telemetry]
  + TPU directo fanout=4 líderes (fallback si Jito no autorizado)
```

### Program IDs Clave
```
Raydium AMM V4:    675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8
Raydium CLMM:      CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK
Orca Whirlpool:    whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc
Meteora DLMM:      LBUZKhRxPF3XUpBCjp4YzTKgLLjgGmGuiVkdseHQcpS
PumpSwap AMM:      pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA  ← AMM (swaps)
PumpFun bonding:   6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P  ← bonding curve (NO es el AMM)
MarginFi:          MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA
Port Finance:      Port7uDYB3wk6GJAw4KT1WpTeMtSu9bTcChBHkX2LfR
Solend:            So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo
Kamino:            KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD
Jito Tip:          96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5
```

### Wallet operativa actual
```
payer por defecto: /root/solana-bot/wallet.json
pubkey:            Hn87MEK6NLiGquWay8n151xXDN4o3xyvhwuRmge4tS1Y
```

### Estado real de flash loan providers (2026-03-09)
| Provider | Estado en executor | Notas |
|---|---|---|
| MarginFi | **Activo** | Wrapper actualizado al flujo oficial `start_flashloan -> borrow -> helios_ix -> end_flashloan`. Usa cuenta `Gj3hRZsQCEboWngk64eFGd3ja4JBoAUMYEiQKSFoAEWY`. |
| Save / Solend | **Activo** | Usa cuentas mainnet reales y `host_fee_receiver` opcional. |
| Kamino | **Activo** | ABI oficial actual de flash borrow/repay con `lending_market`, `market_authority` y `fee_vault`. |
| Port Finance | **Parcialmente activo** | Receiver desplegado en mainnet (`Hwrv5nizU3MWRbmDswt1Z2uwbGjhoJbz88chJnsyHDVi`), `helios-arb` actualizado en mainnet (`8Hi69VoPFTufCZd1Ht2XHHt5mDxrGCMedea1WfCLwE9c`), callback validado on-chain. El zero-hop ya cruza Port -> receiver -> `helios-arb`, pero en el executor sigue deshabilitado en runtime mientras falte `PORT_SOL_RESERVE` en el entorno. |

### Estado Port mainnet verificado
- `PORT_MAIN_MARKET=6T4XxKerq744sSuj3jaoV6QiZ8acirf4TrPwQzHAoSy5`
- `PORT_USDC_RESERVE=DcENuKuYd6BWGhKfGr7eARxodqG12Bz1sN5WA8NwvLRx`
- `PORT_USDC_LIQUIDITY_SUPPLY=2xPnqU4bWhUSjZ74CibY63NrtkHHw5eKntsxf8dzwiid`
- `PORT_USDC_FEE_RECEIVER=2pb9va1fRPzvgxj5w2P2Sqq2a5SQpWMbBD5fMYTNSifp`
- El `flash_accounts.env` ya expone `PORT_USDC_FEE_RECEIVER` para que el executor pueda resolver el receptor de fees, cerrando así el gap operativo mencionado anteriormente.
- El callback de Port se introspecta desde la siguiente instrucción (`CarryPlan`) usando `sysvar::instructions`.
- `execute_route_only` en `helios-arb` ya quedó expuesto y validado on-chain desde el callback de Port.
- El siguiente pendiente ya no es de wiring, sino de ruta real: encontrar/simular una ruta que deje al menos el extra que Port exige para repagar.

### Estado Orca Whirlpool mainnet verificado
- El pipeline `Port → receiver → helios-arb → Orca SwapV2` ya supera los bloques de ABI, ALT dinámica y cuentas intermedias.
- Pool operativo principal: `7qbRF6YsyGuLUVs6Y1q64bdVrfe4ZcUUz1JRdoVNUJnm` (SOL/USDC, tick_spacing=8, ~1138 SOL / ~101k USDC)
  - `vault_a=9RfZwn2Prux6QesG1Noo4HzMEBv3rPndJ2bN2Wwd6a7p`
  - `vault_b=BVNo8ftg2LkkssnWT4ZWdtoFaevnfD6ExYeramwM27pe`
  - `oracle=6vK8gSiRHSnZzAa5JsvBF2ej1LrxpRX21Y185CzP4PeA`
- **Fixes aplicados en `tx_builder.rs` y `port_callback_probe.rs`**:
  1. `SwapV2Args` incluye `remaining_accounts_info` serializado con Borsh, no el placeholder viejo `Option<u8>`.
  2. Layout SwapV2: ambos mints van JUNTOS antes de los ATAs/vaults (posiciones 6,7 en la lista del helios hop). Orden real verificado contra txs on-chain: `[token_program_a, token_program_b, memo_program, token_authority, whirlpool, token_mint_a, token_mint_b, owner_a, vault_a, owner_b, vault_b, tick_array_0, tick_array_1, tick_array_2, oracle]` — fijo `InvalidAccountData on token_mint_b`
  3. Tick arrays: seeds usan **string ASCII** del índice (no bytes LE), y **div_euclid** (floor division) para el start tick — fijo `InvalidTickArraySequence (6023)`
  4. `find_routes` ya refresca en caliente la metadata de los pools elegidos antes de construir la TX.
  5. La TX principal ya crea ATAs idempotentes para tokens intermedios dentro de la misma simulación.
  6. ALT dinámica ya quedó operativa en mainnet: las rutas grandes bajan de >`1500` bytes a ~`500-650` bytes y sí llegan a simulación real.
- **Bloqueador actual real:** el tercer hop Orca del pool `7rDhNbGByYY3rbnJQDFMV9txQYZeBUNQwsMnowDRvw22` falla con `TickArraySequenceInvalidIndex (6038)`.
- El probe dirigido `executor/src/bin/probe_orca_tick_arrays.rs` ya confirmó que barrer offsets lineales y combinaciones cercanas de hasta 3 supplemental tick arrays no encuentra un set válido para ese pool:
  - `shift=-2/-1` tiende a `6023`
  - `shift=0/1/2` tiende a `6038`
  - subir demasiado los supplemental arrays dispara `6055 TooManySupplementalTickArrays`
- Conclusión actual: el siguiente paso serio ya no es "agregar más arrays", sino derivar la secuencia exacta desde la lógica oficial de traversal/quote de Orca para el `amount_in` real del hop.

### Scoring
```
score = (expected_profit / cost) * confidence * urgency * (1 - risk_factor)
> 5.0 → ejecutar inmediato con flash loan máximo
2.0-5.0 → ejecutar si capital idle
1.0-2.0 → baja prioridad
< 1.0 → descartar
```

---

## Estado de Implementación

### ✅ Fase 1 — MVP (COMPLETADO en solana-bot, portar a spy_node)
- [x] UDP ingest con recvmmsg (`src/shred/udp_listener.rs`)
- [x] Shred parser Legacy + Merkle (`src/shred/shred_parser.rs`)
- [x] FEC accumulator + Reed-Solomon (`src/shred/fec_accumulator.rs` + `reed_solomon.rs`)
- [x] Entry decoder + TX decoder (`src/shred/entry_decoder.rs` + `tx_decoder.rs`)
- [x] Arb pipeline / signal bus (`src/shred/arb_pipeline.rs`)
- [x] Workspace Cargo.toml creado en spy_node
- [x] Estructura de directorios (spy-node/, market-engine/, executor/, programs/)

### ✅ Fase 2 — Decoder Spy Node (COMPLETADO)
- [x] Portar/adaptar módulos shred de solana-bot a `spy-node/src/`
- [x] Gossip client en modo spy (shred_version=0, descubrir peers y leader schedule)
- [x] SigVerifier integrado con leader schedule del gossip
- [x] Deduper LRU 10K
- [x] SpySignal types (NewTransaction, WhaleSwap, Graduation, LiquidityEvent)
- [x] Config struct desde env/args
- [x] Métricas Prometheus (shreds/s, FEC recovery rate, latency p50)

### ✅ Fase 3 — Route Engine (FUNCIONAL en mainnet)
- [x] Portar `src/dex/graph.rs` → `market-engine/src/route_engine.rs`
- [x] Bellman-Ford para ciclos negativos (log-precios), solo desde WSOL/USDC/USDT
- [x] Signal processor: filtro DEX, whale detect (>1% pool impact), graduation detect
- [x] Flash selector automático
- [x] Pool state cache con dashmap
- [x] Filtro dust pools: `MIN_RESERVE_WITHOUT_ORACLE=1_000_000` raw units
- [x] Señales viejas descartadas: `if detected_at.elapsed() > 2s { continue; }`

### ✅ Fase 4 — Flash Loans + On-Chain (COMPILADO)
- [x] Portar/extender `programs/helium-gold-v2/` → `programs/helios-arb/`
- [x] Agregar soporte multi-hop (E2: 3+ hops)
- [x] Flash aggregator: MarginFi + Save/Solend + Kamino
- [x] Port Finance receiver program + callback flow
- [x] DEX CPIs: Raydium + Orca + Meteora + PumpSwap

### ✅ Fase 5 — Execution (COMPILADO)
- [x] TX Builder en executor/
- [x] Jito bundle sender (portar `src/jito/bundle.rs`)
- [x] Estrategias E1 (graduation snipe) + E3 (whale backrun)
- [x] Límite de tamaño de tx resuelto con ALT real + separación de ATA setup
- [x] Testing en mainnet con ruta rentable real — PRIMER TRADE LANDED (2026-03-12 20:28 UTC)

### ✅ Fase 6 — Producción (COMPILADO)
- [x] Estrategias E4 + E5 (wired en helios/src/main.rs)
- [x] Oracle cache Pyth Hermes (market-engine/src/oracle.rs, refresh 15s)
- [x] Telegram alerts (executor/src/telegram.rs)
- [x] Pool hydrator (market-engine/src/pool_hydrator.rs, refresh 30s)
- [x] Swap decoder con discriminators reales (market-engine/src/swap_decoder.rs)
- [x] Deploy systemd en Contabo 24/7 (deploy/ files ready)
- [x] Todos los binarios compilados: helios (6.3M), executor (5.6M), spy-node (5.5M)
- [x] Deploy mainnet del receiver de Port + wiring final
- [x] Testing mainnet — primer trade landed (graduation snipe PumpSwap)

### ✅ Fase 7 — Turbine Real + Sniper (2026-03-10)
- [x] Agave 2.x shred header offsets corregidos (slot=65, index=73, version=77, fec_set=79)
- [x] ShredVariant::from_byte() corregido para Agave 2.x (bit7-based, no nibble-based)
- [x] gossip_pong_responder.rs — responde PINGs de validadores para mantener ContactInfo vivo en CRDS
- [x] Gossip port movido a 8003 (8001 ocupado por solana-bot PID 1112)
- [x] shred_latency_probe: p50=669ms antes que RPC, pipeline max=89µs
- [x] Git local inicializado en /root/spy_node/ (commit e786fcb "v10-03-26")
- [x] `parse_shred` migrado a `solana_ledger::shred::Shred` para leer `slot/index/fec_set_index/version` desde el layout oficial en vez de offsets manuales
- [x] `tx_decoder` alineado al bincode real de `solana_entry::entry::Entry`
- [x] `fec_assembler` reescrito con semántica tipo blockstore (`completed_data_indexes`, boundaries por slot, reuse de `Shred` parseado)
- [x] Recovery Merkle local implementado y cubierto con test: `ingest_can_recover_missing_merkle_data_shred`
- [x] Telemetría extra de FEC: attempts Merkle vs legacy + dump automático de sets vacíos a `/tmp/spy-node-fec-dumps/`
- [ ] Registrar en Jito ShredStream + Searcher para bundle submission
- [ ] Activar DRY_RUN=false tras validar primera ruta simulada

### ✅ Fase 8 — FEC Recovery + Route Engine (FUNCIONAL 2026-03-11)
- [x] **FEC Recovery fix (bug crítico)**: `build_recovered_merkle_data_payload` y `build_recovered_merkle_code_payload` usaban `merkle_code_capacity` que EXCLUYE el chained root de 32B para shreds encadenados (todos en Agave 2.x mainnet). Fix: `expected = LEGACY_PAYLOAD_SIZE - LEGACY_CODING_HEADERS_SIZE - proof_bytes - resigned_bytes`. Resultado: `fec_recovery_errors=0`, `fec_recovered_data_shreds>0`, `txs_per_s=13–58`.
- [x] **Route Engine fix (rendimiento)**: Bellman-Ford corría desde los 3710 nodos del grafo → O(V²·E) = **223 segundos** por run. Fix: solo corre desde WSOL/USDC/USDT (`ANCHOR_TOKENS` en `route_engine.rs`). Resultado: milisegundos por run.
- [x] **Dust pool fix**: pools sin oracle incluidas todas antes. Fix: `reserve_a >= 1_000_000 && reserve_b >= 1_000_000` como filtro mínimo. Elimina 2982 rutas falsas.
- [x] **Signal staleness fix**: señales con >2s de antigüedad descartadas antes de entrar al route engine.
- [x] Pipeline confirmado vivo: `shreds_per_s=1300`, `fec_recovered_data_shreds>0`, `txs_per_s=13–58`, route engine desde WSOL/USDC/USDT activo.

### ✅ Fase 9 — Producción LIVE (2026-03-11)
- [x] DRY_RUN=false — bot ejecutando bundles reales
- [x] Token-2022 ATA detection en TxBuilder (fix 100% bloqueante)
- [x] pool_hydrator race condition fix (raydium_meta atómica)
- [x] Dead-route blacklist 5min en executor
- [x] Cycle normalization (mSOL phantom routes eliminadas)
- [x] WS DEX feed multiplexado (1 conexión, 4 suscripciones)
- [x] TpuSender via sim-server (QUIC directo a validadores)
- [x] Telegram wallet balance tras bundle landed
- [x] sim-server Telegram startup notifications
- [x] agave no-vote validator configurado (OOM fix, esperando bootstrap)
- [ ] Jito Searcher auth (pendiente registro)
- [ ] Jito ShredStream whitelist (pendiente registro)
- [ ] BanksClient local (esperando agave validator en puerto 9000)

### ✅ Fase 10 — Pool Hydration Agave Local + Multi-Borrow (2026-03-12)
- [x] `POOL_RPC_URL=http://127.0.0.1:9000` — Agave local para pool hydration (sin rate limits)
- [x] Pool hydrator: 5685/5712 pools con reserves (antes ~32 con Helius rate limit)
- [x] Route engine: 4014 pools en grafo, Bellman-Ford encuentra ciclos arb
- [x] Multi-borrow amounts: [0.1, 0.5, 1.0] SOL — 1 Bellman-Ford run, 3 amounts por ciclo
- [x] Route dedup por `swap_gen` — solo emite cuando reserves cambian
- [x] Batch sleep 5ms (configurable `POOL_BATCH_SLEEP_MS`)
- [x] `get_multiple_accounts_retry()` con 3 intentos + exponential backoff
- [x] `deploy/wait-agave-catchup.sh` — watcher auto-switch sim-server a Agave local cuando healthy
- [ ] Agave catchup completo → sim-server también a Agave local (watcher activo)

### Estado del pipeline shred live (2026-03-12) — PRODUCCIÓN LIVE
- FEC recovery: `fec_recovery_errors=0`, `fec_recovered_data_shreds` creciendo, `txs_per_s=14–270` ✅
- FEC mid-flight fix: `find_complete_data_block` skip boundaries sin shreds tempranos ✅
- Route engine: Bellman-Ford <50ms desde WSOL/USDC/USDT, 4014 pools en grafo ✅
- Pool hydration: Agave local RPC (5685/5712 pools con reserves, sin rate limits) ✅
- Phantom route filter: pools Raydium con vault_a/vault_b == default rechazados ✅
- Señales DEX: shreds + WS DEX multiplexado (Raydium/Orca/Meteora/PumpSwap) ✅
- Graduación sniper: detecta → TPU fanout 4 líderes + Jito ✅
- DRY_RUN=false: bundles reales enviados cuando profit > 10,000 lamports ✅
- Simulación: sim-server HTTP/2 → PublicNode (80-150ms), auto-switch a Agave local via `wait-agave-catchup.sh`

### Estado de producción (2026-03-11) — LIVE
- **DRY_RUN=false** ✅ — bot ejecutando bundles reales
- **Token-2022 ATA fix** ✅ — TxBuilder detecta SPL Token vs Token-2022 por mint owner
- **Dead-route blacklist** ✅ — 5 min cooldown cuando TX build falla todos los providers
- **WS DEX feed multiplexado** ✅ — 1 conexión, 4 logsSubscribe (Raydium/Orca/Meteora/PumpSwap)
- **Telegram wallet balance** ✅ — alerta con SOL+USDC+USDT tras bundle landed
- **TPU fanout** ✅ — 4 líderes vía QUIC (sim-server TpuClient)
- **MAX_HOPS=3** ✅ — 3-hop arb habilitado (A→B→C→A)
- **Route cache TTL=5s** ✅ — reducción de carga en route engine
- Pipeline: shreds_per_s=1500-1700, txs_per_s=7-46, pools_in_graph=251

### PRIMER TRADE EXITOSO (2026-03-12 20:28 UTC)
- **TX**: `232TZoR8czPyaExpLHj4Wg5mmsub6ybQ42WaSMA22LSMhuZmBty9zoJtNLvE8u5LAzWgfxgPyjbTDD3fgzwp4VEz`
- **Slot**: 405,979,397 | **Token**: `4zcv2Yohi9FBs8wALvxxFn7oTeN2cmm3ci1SNyT87qTx`
- **Pool**: `5zk1eTBshwBCTVZswzwW4zwiBBBBUwtrTgrV1i4HMdTr` (PumpSwap graduation)
- **SOL spent**: ~0.054 SOL → **664,312 tokens received**
- **Method**: PumpSwap Sell (WSOL=base) via Agave local RPC
- **Key fix**: WSOL wrapping (create ATA → system transfer → syncNative → sell → closeAccount)

### PumpSwap Sniper — Verified Account Layouts (2026-03-12)
- **Sell** (21 accounts, WSOL=base): pool, user, global_config, base_mint, quote_mint, user_base, user_quote, pool_base_vault, pool_quote_vault, fee_recipient, fee_recipient_quote_ata, base_tp, quote_tp, system, assoc, event_authority, program, creator_vault_ata, creator_vault_authority, fee_config, fee_program
- **Buy** (23 accounts, WSOL=quote): same + global_volume_acc + user_volume_acc before fee_config/fee_program
- coin_creator = `Pubkey::default()` for PumpFun graduated pools
- Pool vaults read from on-chain data (offsets 139, 171) when available, derived as ATAs otherwise
- WSOL wrapping sequence: createATA(idempotent) → system_transfer → syncNative → swap → closeAccount
- Sell data: `[sell_disc(8)][base_amount_in(8)][min_quote_amount_out(8)]`
- Buy data: `[buy_disc(8)][base_amount_out(8)][max_quote_amount_in(8)]`

### ✅ Fase 11 — FAST_ARB + Latency Optimization (2026-03-12)
- [x] **FAST_ARB blind send** — skip simulation, single provider (MarginFi), 22ms build (env `FAST_ARB=true`)
- [x] **2-phase send**: TPU QUIC sync (~2ms on-wire) → Agave/Jito/Helius async background
- [x] **Jito tip dinámico in TX** — `build_with_tip()` appends p75 tip floor as last IX
- [x] **Priority fee** — `ComputeBudgetInstruction::set_compute_unit_limit/price` en arb + sniper + auto-sell
- [x] **min_profit on-chain = 10K lamports** — antes exigía profit exacto, causando InsufficientProfit reverts
- [x] **Cross-DEX scanner** (`market-engine/src/cross_dex.rs`) — E6 strategy, same-pair 2-hop arb
- [x] **Sniper auto-sell** — trailing stop (3%/3%), emergency stop 30%, max hold 30s
- [x] **Sniper pool liquidity check** — skip pools < 200 SOL WSOL reserve (data-driven from 1114 pool analysis)
- [x] **Anti-scam sniper** — freeze auth + mint auth + holder concentration (>30% = skip)
- [x] **PumpFun graduation analysis** — 1114 pools: 99.4% lose, 96.2% rugged, 0.6% winners
- [x] **sell_tokens binary** — liquidate accumulated token holdings back to SOL
- [x] **Jito client timeout** — 5s → 2s (avoid blocking on 429 rejections)
- [x] **RPC TX polling** — `getSignatureStatuses` for Agave/TPU landing detection (not just Jito bundle status)
- [ ] Jito Searcher + ShredStream whitelist (registro enviado, esperando respuesta)

### ✅ Fase 12 — 4 Estrategias + Sniper Hardening (2026-03-13)
- [x] **Whale Backrun** (cross_dex.rs) — focused pair scan on WhaleSwap signal, 15bps margin
- [x] **LST Arb** (lst_scanner.rs) — mSOL/jitoSOL/bSOL/stSOL/INF pairs, 10bps threshold
- [x] **Multi-Launchpad Sniper** — PumpSwap + LaunchLab (Raydium AMM V4 SwapBaseIn) + Moonshot (pending)
- [x] **Liquidation Scanner Phase A** (liquidation_scanner.rs) — MarginFi getProgramAccounts, health factor monitoring
- [x] **PumpFun analysis tool** (analyze_graduations.rs) — 1114 pools analyzed
- [x] **Sniper data-driven hardening** — MIN_POOL_SOL=200, trailing stop 3%/3%, emergency 30%, holder check
- [ ] Liquidation Phase B — flash-borrow → liquidate → swap → repay TX builder
- [ ] Moonshot pool discovery post-migration

### PumpFun Graduation Analysis Results (2026-03-12)
- **1114 pools analyzed** from helios-bot logs
- Winners: 7 (0.6%) — all had 614-797 SOL initial liquidity, no freeze/mint auth
- Losers: 1107 (99.4%) — 96.2% completely rugged (<1 SOL remaining)
- Freeze/mint auth correlation: NONE — 99% lose regardless
- Safe tokens win rate: only 1% (5/958)
- **Strategy**: only snipe pools with >200 SOL initial liquidity, cut losses at 30%

### Latencia medida (2026-03-16 — optimizada)
```
Route engine (incremental graph):               11-16ms (antes 326ms, 30x mejora)
Blockhash (sim-server cache):                   0ms
TX Build (MarginFi + tip + priority fee):       0ms (cached)
JetSender (gRPC leader → TPU+QUIC):             ~5ms
────────────────────────────────────────────────────
Total detección → on-wire:                      ~27-87ms (FAST_ARB + JetSender)
Signal freshness max:                           200ms (antes 2000ms)
Graph rebuild:                                  31ms (primera), luego incremental ~1ms
Route cache TTL:                                500ms (antes 5s)
Jito RTT:                                       Frankfurt=5ms, mainnet=8ms
```

### FAST_ARB Flow (executor/src/lib.rs)
```
Signal → route_engine (incremental BF/cross-dex, 11ms) → executor_loop
  → blockhash (sim-server cache, ~0ms)
  → build_with_tip (MarginFi, 20% profit tip, priority fee)
  → Phase 1: JetSender (gRPC slot → exact leader, TPU+QUIC dual port)
  → Phase 2 (async, 5 channels):
    ├── Jito bundle multi-region (6 endpoints: FRA+AMS+NY+mainnet+Nozomi+NextBlock)
    ├── Jito sendTx multi-region
    ├── Helius staked RPC
    ├── Helius rebate (50% MEV rebate)
    └── Helius Atlas fast sender
  → Phase 3: poll getSignatureStatuses / Jito bundle status
```

### Agave Fork — 19 known + 19 gossip + 15 repair validators (2026-03-16)
- Lista completa en `/root/spy_node/validadores.md`
- Top staked mainnet validators (Helius 14.5M, Galaxy 13.3M, Coinbase 11.7M, etc.)
- `--known-validator` para repair trust + snapshot download
- `--gossip-validator` para priorizar gossip push/pull → mejor posición Turbine
- `--repair-validator` para repair dedicado a top validators (faster shred recovery)
- Telegram monitor notifica cada fase + auto-switch pool hydration cuando synce

### TurboSender — Local BDN Relay (2026-03-16)
- Reemplaza bloXroute ($300+/mo) con servicio local gratuito
- 10+ endpoints paralelos: Jito 4 regions + Nozomi + NextBlock + Helius staked/rebate/atlas
- Landing rate tracking por endpoint (EMA, alpha=0.15)
- Safe validator list (anti-sandwich)
- `executor/src/turbo_sender.rs`

### TurboLearner — ML Model (274K samples trained)
- Data: 141K execution_events + 50K missed_opportunities + 82K pool_discoveries + 494 hot_routes
- Key insights: 4 confirmed TXs at 560ms (PumpSwap, hours 13-14 UTC)
- 82,936 pools (54K Raydium, 18K Orca, 9K Meteora)
- Model saved: `ml_model_state.turbo_sender` in PostgreSQL
- Binary: `train_turbo`
- `executor/src/turbo_learner.rs`

### TX Execution Fixes (2026-03-16)
- **min_profit=0** on-chain: eliminó InsufficientProfit (Custom 6001) — flash loan atómico es el safety net
- **slippage=5%**: tolera más movimiento de precio (era 0.5-3%)
- **Direct swap first**: sin helios-arb CPI → menos CUs, sin profit check on-chain
- **CPI fallback**: para Orca Whirlpool (direct swap no soportado aún)
- **JITO_ONLY=true**: 0 SOL cost en TXs fallidas

### Pool Discovery Auto (2026-03-16)
- `ml-scorer/src/pool_discoverer.rs`: scan getProgramAccounts para 4 DEXes
- `discover_pools` binary: encuentra nuevas pools, guarda en PG + mapped_pools.json
- `ShredDeltaTracker.detect_dex_activity()`: detecta swaps DEX en shreds → hot route discovery
- `unknown_pools`: pools no en cache descubiertas desde shreds

### Blockers pendientes (2026-03-16)
1. **Agave sync**: bootstrapping, accounts hash en progreso → ~20 min para RPC
2. **Pool hydration local**: Geyser vault subscribe <1ms (espera Agave sync)
3. **Primera TX arb rentable**: TXs aterrizan on-chain pero profit=0 (reserves stale)
4. ~~**InsufficientProfit**~~: **RESUELTO** — min_profit=0
5. ~~**Slippage**~~: **RESUELTO** — 5% tolerance

### Jito ShredStream Proxy (compilado, pendiente auth)
- Binario: `/usr/local/bin/jito-shredstream-proxy` v0.2.13 (compilado de source)
- Service: `/etc/systemd/system/jito-shredstream.service` (DISABLED — necesita whitelist)
- Endpoint oficial Frankfurt requiere whitelist → "pubkey not authorized"
- Alternativas sin whitelist: ERPC (`shreds-fra.erpc.global`, 7d trial), RPCFast, Solana Vibe Station
- Ningún tercero funciona con `--block-engine-url` del proxy — usan gRPC propio con `x-token`

### Dual Turbine Feed — shred-mirror (LISTO, esperando Agave)
- Binario: `target/release/shred-mirror` (`spy-node/src/bin/shred_mirror.rs`)
- Mecanismo: AF_PACKET raw socket + BPF filter → copia UDP del Agave TVU :8802 → helios :8002
- BPF offsets: ethernet-aware (EtherType@12, protocol@23, dst_port@36)
- Service: `shred-mirror.service` — enabled, `BindsTo=solana-spy.service helios-bot.service`
- Watcher: `deploy/wait-agave-tvu.sh` (background) arranca el service al detectar tráfico TVU
- Config env: `MIRROR_SRC_PORT=8802`, `MIRROR_DST=127.0.0.1:8002`, `MIRROR_LOG_INTERVAL_SECS=30`
- **Beneficio**: 2 feeds Turbine desde posiciones diferentes del fanout tree = más cobertura de shreds + mejor FEC recovery
- **Impacto en Agave**: cero — AF_PACKET es copy-on-receive a nivel kernel

### Agave 2.x Shred Header (CORREGIDO 2026-03-10)
```
Offset  Size  Campo
0       64    signature (Ed25519)
64      1     shred_variant
65      8     slot (u64 LE)          ← era 75 en parser viejo
73      4     index (u32 LE)         ← era 71
77      2     shred_version (u16 LE) ← era 65
79      4     fec_set_index (u32 LE) ← era 67
83      2     parent_offset (data shreds only)
85      1     flags
86      2     data_size
88      ...   payload data
```
ShredVariant byte: `0x5a`=LegacyCode, `0xa5`=LegacyData. Merkle: bit7=1→Data, bit7=0→Code; proof_size=bits[3:0].
Chained Merkle (mainnet Agave 2.x): upper nibble `0b1001`=chained data, `0b0110`=chained code.
Version mainnet-beta: **50093** (verificado empíricamente offset 77, 2026-03-10).

### FEC Recovery Merkle — Shard Size Formula (CRÍTICO)
Para shreds Merkle encadenados (todos en mainnet Agave 2.x), el shard RS tiene tamaño:
```
shard_size = LEGACY_PAYLOAD_SIZE - LEGACY_CODING_HEADERS_SIZE - proof_size*20 - (if resigned { 64 } else { 0 })
           = 1228 - 89 - proof_size*20 - resigned_bytes
           = 1139 - proof_size*20 - resigned_bytes
```
Esto es el mismo para data shreds Y code shreds (simetría RS).
El chained root (32B) está INCLUIDO en el shard (es parte de los bytes protegidos por RS).
**NO usar `merkle_code_capacity(variant)` para el size check** — esa función descuenta el chained root y falla para shreds chained.

### Gossip PING/PONG (crítico para recibir shreds Turbine)
Sin responder PINGs, los validadores eliminan nuestro ContactInfo de CRDS → dejan de enviar shreds.
- PING wire: `[u32 LE=4][Pubkey 32B][token 32B][Sig 64B]` = 132 bytes
- PONG wire: `[u32 LE=5][Pubkey 32B][pong_hash 32B][Sig 64B]` = 132 bytes
- `pong_hash = sha256(b"SOLANA_PING_PONG" || token)`
- Implementado en `spy-node/src/gossip_pong_responder.rs`
- Helios lo arranca en puerto `HELIOS_GOSSIP_PORT` (default 8003)

### Route Engine — Reglas Críticas
- Bellman-Ford **solo desde WSOL/USDC/USDT** (`ANCHOR_TOKENS` en `market-engine/src/route_engine.rs`). Correr desde todos los nodos = O(V²·E) = 223 segundos.
- **MAX_HOPS=3**: rutas hasta 3-hop (A→B→C→A). Antes era 2 (solo same-pair).
- **Route cache TTL=5s**: reduce carga en route engine. Antes era 2s.
- Pool filter: `MIN_RESERVE_WITHOUT_ORACLE = 1_000_000` raw units. Sin esto → miles de dust pools con arb falso.
- **Vault filter**: pools Raydium V4 con `meta.vault_a == default || meta.vault_b == default` rechazados. Sin esto → phantom routes con profit falso (ej: 831M lamports).
- Señales descartadas si `detected_at.elapsed() > 2s` (urgency ≈ 0).
- `DEFAULT_BORROW_AMOUNT = 1_000_000_000` lamports (1 SOL).
- `MIN_NET_PROFIT = 10_000` lamports (antes 50_000).

### Sniper Flow Óptimo (implementado 2026-03-10)
```
# Build sniper + sim-server
cargo build --release -p executor -p helios -p sim-server

# Arrancar sim-server (local blockhash cache + BanksClient simulation)
UPSTREAM_RPC_URL="https://mainnet.helius-rpc.com/?api-key=..." \
UPSTREAM_WS_URL="wss://mainnet.helius-rpc.com/?api-key=..." \
BANKS_SERVER_ADDR="127.0.0.1:8900" \
./target/release/sim-server

# Configurar helios para usar sim-server
SIM_RPC_URL=http://127.0.0.1:8081 TPU_FANOUT=4 systemctl restart helios-bot
```

### Simulación Local (latencia reducida)
Pipeline de simulación (por orden de latencia):
1. **Preflight in-process** (`executor/src/preflight.rs`, ~0-5µs): size ≤1232B, accounts ≤64, profit>min, amounts>0, risk<1.0. Sin red.
2. **sim-server BanksClient** (`sim-server/src/banks_backend.rs`, ~30-60ms): activar cuando agave-validator (9000) esté listo con `BANKS_SERVER_ADDR=127.0.0.1:8900`
3. **sim-server HTTP/2 proxy** (activo ahora, ~80-150ms): pool HTTP/2 a Helius

Pipeline de envío (4 canales paralelos):
```
// Executor (arb routes):
tokio::join!(
    sender.send_bundle_multi_region(&[tx]),     // Jito bundle → 4 regiones (Frankfurt+Amsterdam+NY+mainnet)
    sender.send_transaction_multi_region(&tx),   // Jito sendTx → 4 regiones (faster propagation)
    agave_send(&tx),                             // Agave local/sim-server sendTransaction (gossip)
);
// + TPU QUIC fanout → 4 líderes (blocking thread)

// Sniper (graduation):
tokio::join!(
    sender.send_bundle_multi_region(&bundle),    // Jito bundle → 4 regiones
    sender.send_transaction_multi_region(&tx),   // Jito sendTx → 4 regiones
    agave_send_tx(&tx),                          // Agave local/sim-server sendTransaction
);
// + TPU QUIC fanout → 4 líderes (blocking thread)
// + Helius rebate sendTransaction (50% MEV rebate)
// + Helius Atlas fast sender
```
- Jito multi-region: Frankfurt + Amsterdam + NY + mainnet (NO requiere Searcher auth — público)
- Jito rate limit: 1 req/s/IP/región (429 = rate limit, no auth)
- Agave local: `AGAVE_RPC_URL` env (default `http://127.0.0.1:9000`, actualmente sim-server `http://127.0.0.1:8081`)
- TPU QUIC: 4 líderes vía sim-server TpuClient
- `helios-arb` on-chain ID mainnet: `8Hi69VoPFTufCZd1Ht2XHHt5mDxrGCMedea1WfCLwE9c`
- Cobertura ~1.6s sin esperar confirmación
- **TpuSender en helios/src/main.rs usa `sim_rpc_url`** (sim-server maneja getEpochInfo sin rate limit)

### Latencia de Shreds — Frankfurt VPS
- **Turbine actual (fuente única)**: p50=669ms antes que RPC, ~1500-1700 shreds/s
- **Jito ShredStream Frankfurt** (pendiente whitelist): ~50–100ms → 13× mejor latencia
- VPS IP: `194.163.150.169` (Contabo Frankfurt) — cerca del block engine Frankfurt ✓
- Proxy compilado: `/usr/local/bin/jito-shredstream-proxy` v0.2.13
- Para activar ShredStream oficial: registrar `H1xo5JpUd36GJDhkNapDHsZkg6Xt57FPsbCRS1QQZWjZ` en jito.network/shredstream/ → `systemctl enable --now jito-shredstream`
- Alternativa sin whitelist: ERPC Frankfurt (`shreds-fra.erpc.global`, 7d trial gratis via Validators DAO Discord)

### Servicios systemd activos
- `helios-bot.service` — bot arb principal (LIVE, DRY_RUN=false)
- `sim-server.service` — proxy RPC local + TpuClient QUIC
- `solana-spy.service` — agave stock no-vote (config optimizada 2026-03-16)
- `helios-agave.service` — agave fork con `--helios-mode` (NEW, pendiente test)
- `richat-server.service` — Richat gRPC server en :10000 (NEW, config validada)
- `shred-mirror.service` — dual Turbine feed (ENABLED, waiting for Agave TVU)
- `jito-shredstream.service` — DISABLED (necesita Jito whitelist)
- `helios.service` — **DISABLED + NEVER RE-ENABLE** (causa raíz de zombie, tenía Restart=always)
- `solana-bot.service` — **DISABLED** (bot viejo, parado 2026-03-12)

### Notas operativas actuales
- **Token-2022 Support:** `TxBuilder::new(payer, rpc_url)` — ahora toma rpc_url para detectar si cada mint es SPL Token o Token-2022 vía `getMultipleAccounts` batch. ATAs derivadas con el programa correcto. Sin esto → "incorrect program id" bloqueaba 100% de oportunidades.
- **ensure_route_atas resiliente:** fallo en setup de ATAs ya no aborta la oportunidad — continúa a simulación. Simulación captura el error real con mejor contexto.
- **Dead-route blacklist:** 5 min de cooldown cuando todos los providers fallan TX build (era 2s). Evita spam de rutas que siempre fallan.
- **Cycle normalization route engine:** ciclos Bellman-Ford rotados para empezar en WSOL/USDC/USDT. Sin esto → rutas con borrow mint=mSOL que flash selectors no soportan.
- **pool_hydrator race condition fix:** `pending_raydium_metas` buffer — raydium_meta solo se escribe al cache cuando step 1b ha resuelto los market accounts (bids/asks/event_queue). Elimina la condición de carrera donde el executor veía meta parcial con default market accounts.
- **WS DEX feed:** refactorizado a 1 conexión WebSocket con 4 logsSubscribe multiplexados. Helius free plan limita conexiones concurrentes — antes daba 403/429 con 4 conexiones separadas.
- **TxBuilder ATA:** `build_ata_setup_instructions` y `associated_token_address` usan el token program correcto por mint. `build_ata_setup_transaction` incluye detección automática.
- **Dynamic ALT Creation:** `alt_loader.rs` crea ALTs dinámicos para rutas >1232 bytes.
- **Pessimistic Slippage:** `safe_out` 99.5% en Orca para evitar `InsufficientFunds` por tick crossing.
- **Ecosistema Expandido:** `mapped_pools.json` expandido a 5700+ pools via `fetch_pools.py`.

### Helios Agave Fork — `--helios-mode` (2026-03-16)
- **Repo**: `/root/helios-agave/` — branch `helios/no-vote-optimized`, base Agave v3.1.9
- **Binario**: `/root/helios-agave/target/release/agave-validator`
- **Service**: `helios-agave.service` (reemplaza `solana-spy.service`)
- **Patches aplicados**:
  1. `core/src/validator.rs` — `helios_mode: bool` en ValidatorConfig
  2. `core/src/tvu.rs` — RetransmitStage, VotingService, WarmQuicCache hechos Optional, skip en helios mode
  3. `validator/src/commands/run/args.rs` — flag `--helios-mode`
  4. `validator/src/commands/run/execute.rs` — wire helios_mode, implica `--no-voting`
- **Ahorro RAM estimado**: ~20-30GB (de 91GB peak a ~65-75GB)
  - RetransmitStage: ~1GB (16K batch channel) + CPU
  - VotingService: threads + connection cache
  - WarmQuicCacheService: QUIC connection warming
  - accounts-db-cache: 8192→2048 MB
  - Richat buffer: 8→2 GiB
  - scheduler-handler-threads: 12→6
  - jemalloc decay: 500ms/1s (era 1s/2s)
  - vm.swappiness: 60 (era 10), vfs_cache_pressure: 200 (era 50)
- **Build**: `cd /root/helios-agave && cargo build --release -p agave-validator`

### Richat gRPC Integration (2026-03-16)
- **Plugin** (Agave → Richat Server): `/etc/richat/plugin.json` — gRPC en :10100
- **Server** (Richat → clientes): `/etc/richat/server.yml` — gRPC en :10000
- **Service**: `richat-server.service` — process separado, max 4GB RAM
- **Binarios compilados**: `/root/richat/target/release/richat` + `librichat_plugin_agave.so`
- **Cliente helios**: pendiente — subscriber gRPC para pool hydration + tx pre-detection

### Agave no-vote Validator (solana-spy.service) — ACTUALIZADO 2026-03-17
- Ledger: `/root/solana-spy-ledger/` — accounts=~452GB, index=~121GB (disco), rocksdb=~59GB
- Puerto RPC: 9000 (localhost) — usado para pool hydration
- Gossip port 8801, TVU port 8802
- Swap total: 153GB
- **Optimizaciones (2026-03-16):**
  - `--accounts-db-cache-limit-mb 2048` (antes 8192)
  - `--limit-ledger-size 50000000` (antes 200000000)
  - `--unified-scheduler-handler-threads 6` (antes 12)
  - `--rpc-threads 2`, `--rpc-pubsub-worker-threads 1`
  - `--no-incremental-snapshots`, `--maximum-incremental-snapshots-to-retain 1` (0 inválido en 3.1.9)
  - `--accounts-shrink-ratio 0.8`
  - jemalloc decay 500ms/1s, `MemoryMax=88G`, `MemorySwapMax=40G`, `MemoryHigh=78G`
- **CRITICAL: NO usar `--repair-validator`** — causa `repair_peers=0` porque nuestro nodo unstaked no está en whitelist de los top validators → no responden repair requests → catchup imposible. Dejar default (all validators).
- **15 `--known-validator`** (top staked: Helius, Galaxy, Coinbase, Binance, Jito, etc.) — solo afecta snapshot download
- **`--minimal-snapshot-download-speed 10000000`** — 10MB/s mínimo, cambia de fuente si más lento
- **Kernel tuning** (`/etc/sysctl.d/99-helios.conf`): swappiness=60, vfs_cache_pressure=200, dirty_ratio=10
- **Snapshot bootstrap**: ~66 min descarga (108GB), ~40 min post-processing (storages + index + hash)
- **Cuando Turbine activo**: `deploy/wait-agave-tvu.sh` arranca `shred-mirror.service` automáticamente
- **Cuando catchup completo**: `deploy/wait-agave-catchup.sh` switcha sim-server automáticamente

### Modo 3-HOP ONLY (2026-03-17)
- **Executor**: `hops < 3 → continue` — descarta toda ruta de 2 hops antes de alertar/ejecutar
- **Route engine**: BF filtrado a ≥3 hops, USDT removido de ANCHOR_TOKENS (no borrowable)
- **Scanners desactivados**: cross_dex 2-hop, hot_routes 2-hop, whale backrun 2-hop
- **Scanners activos**: cyclic_arb (10 combos), BF ≥3 hops, pump_graduation_arb ≥3, dlmm_whirlpool ≥3, LST ≥3
- **ML predictor**: `route_3hop_predictor` filtra rutas con P(land) < 0.3, online learning cada 5 min
- **Telegram**: solo notifica 3-hop, wallet_pattern deshabilitado

### Arquitectura 100% Shred-Driven (2026-03-20)
```
Startup:  ONE-SHOT RPC bootstrap (1396 vaults, ~18s) → verifica + carga reserves reales
Runtime:  100% shreds (0 RPC) → delta tracker 1300+ vault updates/s
Blockhash: derivado de shreds (~2.5/s, 0 RPC)
TX send:  Jito bundles direct (Frankfurt ~5ms)
Latencia: ~17ms detect → on-wire
```
- Pool hydrator RPC: **DISABLED**
- xdex-vault-refresh: **DISABLED**
- Vault refresh in executor: **DISABLED**
- Todo viene de shreds después del bootstrap de 18 segundos

### Causa Raíz Definitiva: 0 Arbs Exitosos (2026-03-20)
1. **8+ pools CERRADAS on-chain** en cache → 100% de TXs iban a cuentas inexistentes
2. **Reserves de snapshot 50h old** → spreads phantom que no existen on-chain
3. **Spreads reales <0.01%** → competidores con NVMe+Jito colocación los cierran en <50ms
4. **Disco SATA 25MB/s** → Agave nunca sincroniza → sin Geyser push → sin reserves real-time
5. FIX: ONE-SHOT RPC bootstrap + pool validator + 100% shreds
6. Para profit consistente: **NVMe VPS necesario** ($100-200/mes)

### lite-rpc (Blockworks) — Compilado, Pendiente Activar
- Binario: `/root/lite-rpc/target/release/lite-rpc`
- Service: `lite-rpc.service` (DISABLED — necesita Agave upstream sincronizado)
- Función: sendTransaction optimizado via QUIC directo a líderes
- RAM: ~4GB — ligero vs Agave completo
- Activar cuando: Agave local sincronice → lite-rpc como proxy de envío

### Bugs Críticos Encontrados (2026-03-17 + 2026-03-19 + 2026-03-20)
- **Pools cerradas on-chain**: 8+ pools (58oQChx4, AVs9TA4n, 6UmmUiYo, HBS7a3br, etc.) CERRADAS pero en cache. Generaban 100% de las rutas → 100% failure. **FIX**: ONE-SHOT RPC bootstrap verifica existencia + pool validator cada 5min purga dead pools.
- **Proceso zombie helios**: **CAUSA RAÍZ ENCONTRADA**: `helios.service` (duplicado) con `Restart=always`. Ahora MASKED (/dev/null symlink). `helios-pool-updater.timer` también MASKED. **NUNCA re-habilitar.**
- **Spread predictor ML**: bloqueó 796,871 rutas rentables (max 0.523 SOL). Modelo entrenado con 0% success. **FIX**: bypass cuando `JITO_ONLY=true`.
- **Pool cache vacío al startup**: `mapped_pools.json` fallaba deserialización (PumpSwap pools sin `symbol`/`decimals`). **FIX**: `#[serde(default)]` en MappedPool.
- **Redis vault balances no cargados**: `load_all_metadata()` usaba `KEYS` scan (1.87M keys) y no cargaba vaults. **FIX**: `load_metadata_fast()` + `load_vault_balances()` con MGET.
- **PumpSwap graduation pools no insertadas**: detección OK pero pool no entraba al cache. **FIX**: insert con reserves estimadas (~79 SOL) + RPC hydration.
- **`--repair-validator` en nodo unstaked**: `repair_peers=0` → catchup imposible. **NUNCA usar.**
- **`--maximum-incremental-snapshots-to-retain 0`**: inválido en Agave 3.1.9, usar 1.
- **RaydiumCpmm match order**: `"raydium_cpmm"` contiene "raydium" → match como RaydiumAmmV4. El match de CPMM debe ir ANTES del match de Raydium genérico.
- **pg-logger type mismatches**: `i16` vs PG `integer` en n_hops, `f64` vs PG `integer` en price_impact_bps. **FIX**: ALTER TABLE columnas a tipos correctos.
- **sqlite_route_feed spam**: tabla inexistente. **FIX**: `SQLITE_ROUTE_FEED=false`.
- **SolanaCDN spam**: conectaba a localhost:9999 inexistente. **FIX**: `ERPC_SHREDS_ENABLED=false`.

---

## Comandos de Desarrollo

```bash
# Build workspace completo
cd /root/spy_node && cargo build --release

# Build solo spy-node
cargo build --release -p spy-node

# Build on-chain program (Anchor)
cd /root/spy_node/programs/helios-arb && anchor build

# Build on-chain programs SBF reales
cd /root/spy_node && cargo build-sbf --manifest-path programs/helios-arb/Cargo.toml --optimize-size
cd /root/spy_node && cargo build-sbf --manifest-path programs/port-flash-receiver/Cargo.toml --optimize-size

# Build probe Port mainnet
cd /root/spy_node && cargo build --release --bin port_callback_probe

# Simular callback Port con descubrimiento de rutas reales
cd /root/spy_node && ./target/release/port_callback_probe

# Ajustar borrow sizes / número de candidatos del probe
PORT_PROBE_BORROW_AMOUNTS=1000000000,500000000,100000000 PORT_PROBE_MAX_ROUTES=10 ./target/release/port_callback_probe

# Ver dependencias del proyecto referencia
cat /root/solana-bot/Cargo.toml

# Ledger local para testing offline
ls /root/solana-spy-ledger/

# Logs del bot anterior (referencia para wallet/keys)
cat /root/solana-bot/config.toml

# Monitorear pipeline en vivo
journalctl -fu helios-bot | grep -E "(throughput|route engine|opportunity|simulation|FEC|error)"

# Probar latencia de shreds
./target/release/shred_latency_probe

# Build Agave fork (helios-mode)
cd /root/helios-agave && cargo build --release -p agave-validator

# Test Agave fork
/root/helios-agave/target/release/agave-validator --helios-mode --help

# Arrancar stack completo (Richat + Agave fork)
systemctl start richat-server && sleep 2 && systemctl start helios-agave

# Monitorear Richat gRPC
curl -s http://127.0.0.1:10124/metrics 2>/dev/null | head -20
```

---

## Reglas de Riesgo (Inviolables)
1. Max daily loss: 5% working capital. Si >0.5 SOL en tips fallidos → pausar 24h.
2. Never hold: ninguna posición >1 bloque (~400ms). Todo atómico via Jito.
3. FAST_ARB: simulación opcional (flash loans son atómicos — TX falla = 0 costo VIA JITO BUNDLES).
4. Token blacklist automática: mint authority activa, freeze authority, LP no locked, top holder >30%.
5. Max flash amount: 10% de liquidez del proveedor.
6. Solo ejecutar si profit neto > 2× costos totales.
7. PROHIBIDO sniping — solo arbitraje atómico con flash loans.

## REGLA CRÍTICA: JITO_ONLY — NUNCA enviar TXs con tip vía sendTransaction

**BUG ENCONTRADO 2026-03-18: Pérdida de 0.113 SOL en 84 TXs fallidas.**

El TurboSender enviaba TXs a 9 endpoints (Jito + Helius + public RPC). Las TXs enviadas via `sendTransaction` (no bundle) **aterrizan on-chain y cobran fee + tip INCLUSO si el swap falla**. Cada TX fallida costaba ~0.002 SOL (6250 lamports fee + ~1.5M lamports tip).

**Regla inviolable:**
- **NUNCA** enviar TXs con instrucción de tip Jito via `sendTransaction` — solo via `send_bundle`
- Cuando `JITO_ONLY=true`, el TurboSender DEBE filtrar endpoints no-Jito (`if jito_only && !ep.is_jito { continue; }`)
- Jito bundles son atómicos: TX fallida = 0 costo. `sendTransaction` NO es atómico: TX fallida = fee + tip cobrados.
- **SIEMPRE verificar** que TXs con tip solo fluyen por el path de bundles Jito
- **Monitorear balance** después de cada sesión — si baja sin TXs exitosas, hay un leak de tips
- El fix está en `executor/src/turbo_sender.rs` línea 197

### Anatomía del bug:
```
TX con tip_lamports=2000000:
  ├── Via Jito bundle: si falla → 0 costo (bundle descartado)
  └── Via sendTransaction: si falla → cobra 6250 fee + 2000000 tip = 0.002 SOL PERDIDO
```

## REGLA CRÍTICA: JITO_ONLY Bypass — NO filtrar rutas cuando failed TX = 0 cost

**BUG ENCONTRADO 2026-03-19: Spread predictor bloqueó 796,871 rutas rentables.**

El modelo ML `spread_survival_predictor` fue entrenado con data donde 100% de rutas fallaban (reserves stale). Aprendió a rechazar TODO → `p_spread=0.000` para cada ruta. Resultado: 0 TXs enviadas al executor durante días, incluyendo rutas con hasta 0.523 SOL de profit.

**Regla inviolable:**
- Cuando `JITO_ONLY=true`, bypasear **spread predictor** Y **DecisionEngine** (failed TX = 0 SOL)
- Solo los filtros básicos deben quedar: `scorer.is_viable` (bypassed para graduation), `competition blacklist`
- **NUNCA** re-activar filtros ML sin reentrenar con data de TXs exitosas reales
- El bypass: `if p_spread < 0.4 && !is_high_conviction && !jito_only { continue; }`
- Graduation/new_pool routes tienen `is_high_conviction=true` (bypasean TODO)

### Datos de oportunidades bloqueadas:
```
spread_survival=0.000: 796,871 rutas bloqueadas
  cross_dex_3hop: 518,160 (avg profit 8.9M lamports)
  cross-dex:      255,198 (avg profit 15M lamports)
  bellman-ford:    20,025 (avg profit 37.9M lamports, max 523M!)
  lst-arb:            885 (avg profit 654K lamports)
```
