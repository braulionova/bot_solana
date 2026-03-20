# Plan: Mini Agave en Rust — Nodo Ligero para Helios Arb

## Problema

El Agave full validator no funciona en este VPS:
- **94GB RAM** disponibles, Agave consume **88GB RAM + 103GB swap** (100% usado)
- **43K slots behind** mainnet — nunca alcanza
- Accounts DB: ~400GB en disco, index ~53GB en RAM/disco
- Ledger: ~620GB, creciendo
- Solo necesitamos: **blockhash fresco, estado de pools, y shreds en tiempo real**

Un full validator replica TODO el estado de Solana (~400M accounts) cuando helios solo necesita ~6000 pools + blockhash.

---

## Arquitectura: `mini-agave`

```
┌─────────────────────────────────────────────────────┐
│                    mini-agave                        │
│                   (~2-4 GB RAM)                      │
│                                                      │
│  ┌──────────────┐  ┌──────────────┐  ┌────────────┐ │
│  │ ShredStream  │  │  gRPC Feed   │  │  Blockhash  │ │
│  │  (Jito/ERPC) │  │ (Yellowstone)│  │   Cache     │ │
│  │  UDP shreds  │  │  accounts +  │  │  from gRPC  │ │
│  │  → spy-node  │  │  txs + slots │  │  blocks_meta│ │
│  └──────┬───────┘  └──────┬───────┘  └──────┬──────┘ │
│         │                 │                  │        │
│  ┌──────▼─────────────────▼──────────────────▼─────┐ │
│  │              Estado Local (DashMap)               │ │
│  │  - pool vaults (reserves, fees, tick arrays)     │ │
│  │  - token mints (program owner, freeze/mint auth) │ │
│  │  - blockhash (últimos 300)                       │ │
│  │  - leader schedule                               │ │
│  │  - epoch info                                    │ │
│  └──────────────────────┬──────────────────────────┘ │
│                         │                            │
│  ┌──────────────────────▼──────────────────────────┐ │
│  │           RPC Server Local (Axum)                │ │
│  │  - getMultipleAccounts (desde cache/fallback)    │ │
│  │  - getLatestBlockhash (desde cache)              │ │
│  │  - simulateTransaction (proxy a Helius/PN)       │ │
│  │  - sendTransaction (proxy + TPU QUIC)            │ │
│  │  - getHealth / getEpochInfo                      │ │
│  └─────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────┘
         │                    │
         ▼                    ▼
   helios-bot            sim-server
   (arb engine)          (TPU QUIC)
```

### Qué NO hace mini-agave (vs full Agave):
- NO replica el ledger completo (ahorra ~620GB disco)
- NO mantiene accounts DB (ahorra ~400GB disco + ~53GB RAM index)
- NO ejecuta transacciones localmente (sin runtime SVM)
- NO participa en consenso (no vota)
- NO almacena snapshots

### Qué SÍ hace:
1. **Recibe shreds en tiempo real** via ShredStream (50-100ms vs 669ms actual)
2. **Cachea estado de pools** via gRPC account subscriptions (~6000 accounts)
3. **Mantiene blockhash fresco** via gRPC blocks_meta (~50ms vs 400ms polling)
4. **Sirve RPC local** para helios-bot y sim-server (0ms latencia de red)
5. **Detecta transacciones DEX** via gRPC transaction subscriptions

---

## Componentes en Detalle

### 1. ShredStream Client (ya existe parcialmente)

**Archivo existente**: `spy-node/src/jito_shredstream_client.rs`
**Dependencia**: `yellowstone-grpc-client = "1.14.0"` (ya en workspace)

```
Jito ShredStream Frankfurt (shreds-fra.jito.network)
  → gRPC stream de shreds
  → forward UDP → 127.0.0.1:8002 (spy-node ingest)
```

**Alternativas sin whitelist Jito**:
- ERPC Frankfurt: `shreds-fra.erpc.global` (trial 7 días, luego pago)
- RPCFast: sin auth, gRPC directo
- Solana Vibe Station: sin auth

**Latencia esperada**: 50-100ms (vs 669ms Turbine actual)

**Implementación**: El cliente gRPC ya existe. Solo falta:
- Configurar endpoint (env `SHREDSTREAM_ENDPOINT`)
- Manejar reconexión automática con backoff exponencial
- Forward shreds por UDP a spy-node (ya implementado)

### 2. gRPC Account Feed (estado de pools)

**Archivo existente**: `helios/src/geyser_feed.rs` + `sim-server/src/geyser_cache.rs`

Suscripción selectiva a ~6000 accounts (pool vaults):
```rust
// Suscribir solo a las accounts que necesitamos
let account_filter = SubscribeRequestFilterAccounts {
    account: pool_vault_pubkeys,  // ~6000 vaults de Raydium/Orca/Meteora/PumpSwap
    owner: vec![],                // no filtrar por owner (ya tenemos las pubkeys exactas)
    ..Default::default()
};
```

**RAM estimada**: 6000 accounts × ~200 bytes avg = ~1.2MB (despreciable)

**Refresh**: push en tiempo real (gRPC notifica cada cambio de account)
- Elimina pool_hydrator polling (actualmente cada 30s, 5685 accounts)
- Latencia: ~0ms (push) vs ~30s (polling)

**Fuentes gRPC gratuitas/baratas**:
| Proveedor | Endpoint | Costo | Notas |
|---|---|---|---|
| Helius | gRPC incluido en plan paid | Ya pagamos | Limits: check plan |
| Triton (RPC Pool) | `grpc.rpcpool.com` | Free tier limitado | Account subs OK |
| GenesysGo | `grpc.genesysgo.net` | Paid | Baja latencia |
| Agave local (futuro) | `127.0.0.1:50051` | Free | Solo si Agave corre |

### 3. gRPC Blockhash Cache

**Archivo existente**: `sim-server/src/geyser_cache.rs` (línea ~50)

```rust
// Ya implementado en geyser_cache.rs:
// Suscribe a blocks_meta → extrae blockhash del último bloque confirmado
let blocks_meta_filter = SubscribeRequestFilterBlocksMeta {};
// Cada ~400ms llega un nuevo bloque → blockhash fresco
```

**Latencia**: ~50ms después de confirmación (vs 400ms polling actual)
**RAM**: ~10KB (300 blockhashes × 32 bytes)

### 4. gRPC Transaction Feed (señales DEX)

**Archivo existente**: `helios/src/geyser_feed.rs`

Suscripción a transacciones que involucran programas DEX:
```rust
let tx_filter = SubscribeRequestFilterTransactions {
    account_include: vec![
        "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", // Raydium V4
        "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK", // Raydium CLMM
        "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",  // Orca
        "LBUZKhRxPF3XUpBCjp4YzTKgLLjgGmGuiVkdseHQcpS",  // Meteora
        "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA",   // PumpSwap
        "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",  // PumpFun bonding
    ],
    ..Default::default()
};
```

**Ventaja sobre WS logsSubscribe**:
- gRPC da la TX completa (accounts + data), no solo logs
- Menor latencia (push directo del validador vs WebSocket relay)
- Una conexión, múltiples filtros

### 5. RPC Server Local (reemplaza sim-server parcialmente)

Mini-agave expone un servidor RPC local (Axum) que responde desde cache:

| Método RPC | Fuente | Latencia |
|---|---|---|
| `getLatestBlockhash` | gRPC blockhash cache | ~0ms |
| `getMultipleAccounts` | gRPC account cache | ~0ms (hit) / fallback RPC ~80ms (miss) |
| `getEpochInfo` | gRPC slots | ~0ms |
| `getHealth` | siempre "ok" si gRPC conectado | ~0ms |
| `simulateTransaction` | proxy → Helius/PublicNode | ~80-150ms |
| `sendTransaction` | proxy + TPU QUIC | ~2ms |
| `getTokenAccountsByOwner` | proxy → Helius (raro, solo sniper) | ~200ms |

**Fallback**: cualquier método no cacheado → proxy a upstream RPC (Helius/PublicNode)

---

## Estimación de Recursos

### RAM
| Componente | RAM estimada |
|---|---|
| gRPC client (ShredStream) | ~50MB |
| gRPC client (accounts + txs + blocks) | ~100MB |
| Account cache (6000 pools × 500B) | ~3MB |
| Blockhash cache (300 × 32B) | ~10KB |
| Axum RPC server | ~20MB |
| Pool metadata (DashMap) | ~50MB |
| Shred pipeline (spy-node) | ~200MB |
| Tokio runtime | ~50MB |
| **Total mini-agave** | **~500MB - 1GB** |

### vs Full Agave
| | Full Agave | Mini Agave |
|---|---|---|
| RAM | 88GB + 103GB swap | **~1GB** |
| Disco | ~1TB (accounts + ledger) | **~0** (solo binario) |
| CPU | 18 cores saturados | **2-4 cores** |
| Latencia blockhash | 400ms (polling) | **~50ms** (gRPC push) |
| Latencia accounts | 30s (hydrator polling) | **~0ms** (gRPC push) |
| Sync con mainnet | 43K slots behind | **Tiempo real** (gRPC) |

### Disco
- Binario mini-agave: ~10MB
- Sin ledger, sin accounts DB, sin snapshots
- Ahorra ~1TB de disco

---

## Stack de Dependencias

```toml
[dependencies]
# gRPC (ya en workspace)
yellowstone-grpc-client = "1.14.0"
yellowstone-grpc-proto  = "1.13.0"
tonic                   = "0.11"

# Shred processing (ya en spy-node)
solana-sdk              = { workspace = true }
solana-ledger           = { workspace = true }

# State management (ya en workspace)
dashmap                 = "5"
crossbeam-channel       = "0.5"
parking_lot             = { workspace = true }

# RPC server
axum                    = "0.7"
tower                   = "0.4"

# Networking
reqwest                 = { workspace = true }  # fallback RPC proxy
tokio                   = { workspace = true }

# Serialization
serde                   = { workspace = true }
serde_json              = { workspace = true }
bincode                 = { workspace = true }
bs58                    = { workspace = true }

# Observability
tracing                 = { workspace = true }
tracing-subscriber      = { workspace = true }

# Performance
mimalloc                = { version = "0.1", default-features = false }
core_affinity           = "0.8"
```

Todas las dependencias ya están en el workspace — **no se necesita ninguna nueva**.

---

## Fases de Implementación

### Fase 1: gRPC Account Cache + Blockhash (3-4 horas)
**Objetivo**: Reemplazar pool_hydrator polling + blockhash polling con gRPC push

1. Crear `mini-agave/` como nuevo crate en el workspace
2. Implementar `grpc_subscriber.rs`:
   - Conectar a Helius/Triton gRPC
   - Suscribir a ~6000 pool vault accounts
   - Suscribir a blocks_meta para blockhash
   - Almacenar en `DashMap<Pubkey, AccountData>`
3. Implementar `rpc_server.rs`:
   - `getLatestBlockhash` → desde cache
   - `getMultipleAccounts` → desde cache, fallback a upstream
   - Proxy genérico para métodos no cacheados
4. Configurar helios-bot para usar `POOL_RPC_URL=http://127.0.0.1:8082` (mini-agave)

**Resultado**: Pool hydration en tiempo real, blockhash ~50ms, 0 rate limits

### Fase 2: gRPC Transaction Feed (2-3 horas)
**Objetivo**: Reemplazar WS logsSubscribe con gRPC transaction stream

1. Adaptar `geyser_feed.rs` para emitir señales al signal_bus de helios
2. Suscribir a transacciones de los 6 programas DEX
3. Parsear swaps en tiempo real (swap_decoder ya existe)
4. Emitir SpySignal al mismo canal que usa el shred pipeline

**Resultado**: Detección de swaps DEX con menor latencia que WebSocket

### Fase 3: ShredStream Nativo (1-2 horas)
**Objetivo**: Recibir shreds via gRPC en vez de Turbine directo

1. Activar `jito_shredstream_client.rs` existente
2. Configurar endpoint (ERPC Frankfurt si Jito no whitelisted)
3. Forward shreds → spy-node UDP ingest (ya implementado)
4. Medir latencia: target <100ms vs 669ms actual

**Resultado**: Shreds 6-13× más rápido

### Fase 4: Consolidación + Desactivar Agave (1 hora)
**Objetivo**: Mini-agave reemplaza completamente al Agave validator

1. Service systemd `mini-agave.service`
2. `sim-server` apunta a mini-agave como upstream para blockhash
3. `helios-bot` usa mini-agave para pool state
4. Desactivar `solana-spy.service` → liberar 88GB RAM + 1TB disco
5. Métricas: account cache hits, blockhash age, gRPC reconnects

**Resultado**: VPS libre para helios-bot + sim-server + mini-agave (~3GB total vs 88GB)

### Fase 5: Optimización de Latencia (ongoing)
1. Medir latencia end-to-end con mini-agave
2. Añadir account prefetch: pre-suscribir accounts de rutas frecuentes
3. Blockhash prediction: usar slot timing para anticipar próximo blockhash
4. TPU leader schedule cache desde gRPC slots

---

## Latencia Esperada (con mini-agave)

```
ANTES (Agave full + polling):
  Shred arrival:     669ms (Turbine single source)
  Pool state:        30,000ms (hydrator polling cada 30s)
  Blockhash:         400ms (RPC polling)
  Simulación:        80-150ms (HTTP/2 proxy)
  ────────────────────────────────────────
  Worst case:        ~31,000ms

DESPUÉS (mini-agave + gRPC + ShredStream):
  Shred arrival:     50-100ms (ShredStream gRPC)
  Pool state:        ~0ms (gRPC push, ya en cache)
  Blockhash:         ~0ms (gRPC push, ya en cache)
  Simulación:        80-150ms (mismo proxy, o skip con FAST_ARB)
  ────────────────────────────────────────
  Worst case FAST_ARB: ~100ms (shred) + 20ms (build) + 2ms (TPU) = ~122ms
  vs actual:           669ms + 20ms + 2ms = ~691ms
```

**Mejora**: ~5.6× más rápido en el path crítico (shred → on-wire)

---

## Riesgos y Mitigaciones

| Riesgo | Mitigación |
|---|---|
| gRPC provider caído | Fallback automático: Helius → Triton → PublicNode RPC polling |
| Account cache miss (pool no suscrito) | Fallback a upstream RPC (Helius HTTP/2), ~80ms |
| ShredStream no whitelisted (Jito) | ERPC Frankfurt (7d trial), RPCFast (sin auth) |
| Rate limits en gRPC provider | Helius paid plan incluye gRPC; Triton free tier suficiente para 6K accounts |
| Blockhash stale | Detector: si age >2 slots → fallback a RPC polling |
| Reconexión gRPC lenta | Backoff exponencial + keep alive cada 10s + multi-provider |

---

## Configuración Propuesta

```env
# mini-agave.env
GRPC_ENDPOINT=https://mainnet.helius-rpc.com  # Helius gRPC (paid plan)
GRPC_TOKEN=<helius-api-key>
SHREDSTREAM_ENDPOINT=shreds-fra.erpc.global    # ERPC Frankfurt (o Jito cuando whitelisted)
SHREDSTREAM_TOKEN=<erpc-token>
LISTEN_RPC_PORT=8082                           # RPC local para helios-bot
UPSTREAM_RPC_URL=https://mainnet.helius-rpc.com/?api-key=...  # fallback
POOL_ACCOUNTS_FILE=/root/spy_node/pool_accounts.json  # lista de accounts a suscribir
LOG_LEVEL=info
```

---

## Resumen Ejecutivo

| Métrica | Agave Full | Mini Agave |
|---|---|---|
| RAM | 88GB (+103GB swap) | **~1GB** |
| Disco | ~1TB | **~10MB** |
| CPU | 18 cores | **2-4 cores** |
| Sync | 43K slots behind | **Tiempo real** |
| Blockhash | 400ms polling | **~50ms push** |
| Pool state | 30s polling | **~0ms push** |
| Shreds | 669ms Turbine | **50-100ms ShredStream** |
| Latencia arb | ~691ms | **~122ms** |
| Dependencias nuevas | — | **0** (todo en workspace) |
| Tiempo implementación | — | **~8-10 horas** (4 fases) |

**Mini-agave no es un validator** — es un cliente gRPC inteligente con cache local y servidor RPC. Obtiene exactamente la información que helios necesita (pools, blockhash, shreds, swaps DEX) sin replicar los ~400M accounts de Solana.
