# Plan de Reducción de Latencia — Helios Gold V2

## Estado actual: 27ms detect→on-wire (FAST_ARB blind send)

```
Shred UDP recv:          +50µs   (recvmmsg, CPU pinned)
Channel parsed_tx:       +5µs    (crossbeam bounded 512)
Shred parser:            +10µs
Sig verifier batch:      +50-200µs (Ed25519 ×64 amortizado)
Signal processor (FEC):  +2-5ms  (Reed-Solomon, bincode)
Route engine (BF/cross): +1-50ms (cache hit <1ms, BF run 10-50ms)
Executor re-quote:       +5-10µs
TX build:                +20-30µs
TX clone ×4:             +40-100µs  ← DESPERDICIO
TPU QUIC send:           +2ms    (red al líder)
────────────────────────────────────────
TOTAL:                   ~27ms
```

---

## FASE 1 — Fixes triviales (< 1 hora, +15-20% mejora)

### 1.1 Activar mimalloc global
**Estado**: declarado en Cargo.toml pero NUNCA activado como allocator.
**Impacto**: 10-20% reducción en latencia de allocaciones (glibc malloc → mimalloc).
```rust
// helios/src/main.rs (línea 1, antes de fn main)
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

### 1.2 Eliminar TX clone ×4 → Arc
**Estado**: 4 clones de VersionedTransaction (~1200B cada uno) por oportunidad.
**Impacto**: -40-100µs por TX enviada.
```rust
// executor/src/lib.rs ~línea 305
// ANTES:
let tx_for_tpu = tx.clone();
let tx_for_agave = tx.clone();
let tx_for_rebate = tx.clone();
let tx_for_fast = tx.clone();
// DESPUÉS:
let tx = Arc::new(tx);
// Pasar Arc::clone(&tx) a cada canal (~2ns vs ~1µs)
```

### 1.3 CPU pinning en parser/verifier/route-engine
**Estado**: solo UDP ingest pinned. 3 threads críticos sin pinning.
**Impacto**: -5-10% jitter por cache misses L1/L2.
```rust
// helios/src/main.rs, en cada thread::Builder::new().spawn()
let core_ids = core_affinity::get_core_ids().unwrap_or_default();
core_affinity::set_for_current(core_ids[N]); // N=2,3,4 (0,1 para UDP)
```

### 1.4 Reducir leader-sync sleep 200ms → 20ms
**Impacto**: -180ms de jitter máximo en rotación de líder.
```rust
// helios/src/main.rs ~línea 613
std::thread::sleep(Duration::from_millis(20)); // era 200
```

---

## FASE 2 — Optimizaciones de arquitectura (2-4 horas)

### 2.1 Executor en thread dedicado (sacar de tokio)
**Problema**: executor_loop() es BLOCKING dentro de tokio runtime → bloquea tasks async.
**Impacto**: desbloquea Jito/Helius polling, -50ms en phase2.
```
ANTES:  tokio::spawn(executor_loop(...))  // blocking in async
DESPUÉS: thread::Builder::new("executor").spawn(executor_loop(...))
         // Solo phase2 (Jito/Helius sends) queda en tokio
```

### 2.2 Canal kanal para SPSC hot paths
**Fuente**: github.com/fereidani/rust-channel-benchmarks
**Problema**: crossbeam bounded es bueno pero kanal usa direct-stack-copy (sender→receiver sin broker).
**Impacto**: -1-5µs por hop de canal en pipeline caliente.
```toml
# Cargo.toml
kanal = "0.1"
```
Reemplazar `crossbeam_channel::bounded` por `kanal::bounded` en:
- `parsed_tx/rx` (shred parser → verifier)
- `verified_tx/rx` (verifier → signal processor)
- `opp_tx/rx` (route engine → executor)

### 2.3 Índice de pools por par de tokens
**Problema**: cross_dex.rs itera 5000+ pools cada scan (~1-5ms).
**Impacto**: -1-3ms por scan de cross-dex.
```rust
// pool_state.rs: agregar índice secundario
pair_index: DashMap<(Pubkey, Pubkey), Vec<Pubkey>>, // (token_a, token_b) → pool_addresses
```

### 2.4 Pre-serializar TX bytes
**Problema**: bincode::serialize(&tx) se llama 2 veces (size check + log).
**Impacto**: -100-200µs al cachear bytes serializados.
```rust
let tx_bytes = bincode::serialize(&tx)?;
let tx_size = tx_bytes.len();
// Reusar tx_bytes para TPU send y logging
```

---

## FASE 3 — Multi-relayer submission (4-8 horas)

### 3.1 Agregar Nozomi + ZeroSlot + NextBlock
**Fuentes**:
- github.com/vvizardev/solana-relayer-adapter-rust
- github.com/roswelly/solana-block-engine-client
**Estado**: solo Jito + TPU + Agave + Helius. Faltan 3-4 relayers.
**Impacto**: +30-50% cobertura de landing (más paths al líder).
```
Canales actuales:    TPU QUIC (4 líderes) + Jito (4 regiones) + Agave + Helius
Canales propuestos:  + Nozomi + ZeroSlot + NextBlock + BloxRoute
```

### 3.2 Selección de región por latencia
**Fuente**: solana-block-engine-client (latency-based region auto-selection)
**Problema**: actualmente envío estático a Frankfurt/Amsterdam/NY/mainnet.
**Impacto**: -10-50ms al elegir región con menor RTT actual.
```rust
// Ping periódico a cada endpoint, ordenar por latencia
// Enviar primero al más rápido
```

---

## FASE 4 — Kernel bypass & zero-copy (1-2 días, avanzado)

### 4.1 AF_XDP para UDP shred ingest
**Fuentes**:
- github.com/aterlo/afxdp-rs
- github.com/sudachen/xdp-rs
**Problema**: recvmmsg pasa por kernel networking stack (syscall overhead).
**Impacto**: -10-20µs por batch de shreds, elimina copias kernel→userspace.
```
ANTES:  recvmmsg() → kernel buffer → copy → userspace
DESPUÉS: AF_XDP → shared ring buffer → zero-copy read
```
**Riesgo**: alta complejidad, requiere privilegios root, BPF filter.

### 4.2 bytemuck zero-copy DEX deserialization
**Fuente**: github.com/jozef-pridavok/arbitrage (arb-core crate)
**Problema**: swap_decoder.rs usa deserialización manual con slices.
**Impacto**: -2-5µs por swap decoding (elimina bounds checks).
```rust
use bytemuck::{Pod, Zeroable};
#[repr(C)]
#[derive(Pod, Zeroable, Copy, Clone)]
struct RaydiumSwapLayout { /* campos packed */ }
let swap: &RaydiumSwapLayout = bytemuck::from_bytes(&data[offset..]);
```

### 4.3 io_uring para RPC calls (phase2)
**Fuentes**:
- github.com/tokio-rs/io-uring
- github.com/bytedance/monoio
**Problema**: tokio usa epoll para I/O; io_uring permite batch + zero-copy.
**Impacto**: -10-30% en latencia de RPC polling (phase3 status checks).
**Riesgo**: requiere Linux 5.6+, API compleja.

---

## FASE 5 — Geyser plugin optimización (ya implementado, pendiente catchup)

### 5.1 Plugin activo, esperando Agave catchup
- .so compilado: `/root/spy_node/target/release/libhelios_geyser_plugin.so`
- 1336 vaults monitoreados
- UDP receptor en puerto 8855
- Latencia esperada: **~0ms** (in-process) vs 200ms (WS actual)

### 5.2 Cuando Agave catchee up
```
Latencia vault update: 200ms → ~0ms
Latencia total detect→on-wire: 27ms → ~10ms
Landing rate estimado: 2-5% → 15-30%
Ingreso mensual: $450-780 → $2,000-4,500
```

---

## Repos de referencia clave

| Repo | Técnica relevante | URL |
|---|---|---|
| **jozef-pridavok/arbitrage** | bytemuck zero-copy, Geyser+NATS | github.com/jozef-pridavok/arbitrage |
| **fereidani/rust-channel-benchmarks** | kanal > crossbeam en SPSC | github.com/fereidani/rust-channel-benchmarks |
| **vvizardev/solana-relayer-adapter-rust** | Multi-relayer (Nozomi/ZeroSlot/NextBlock) | github.com/vvizardev/solana-relayer-adapter-rust |
| **roswelly/solana-block-engine-client** | Latency-based region selection | github.com/roswelly/solana-block-engine-client |
| **blockworks-foundation/geyser-grpc-connector** | Fastest-wins multiplexing | github.com/blockworks-foundation/geyser-grpc-connector |
| **0xfnzero/solana-streamer** | Protocol parsers (PumpFun/Raydium/Orca) | github.com/0xfnzero/solana-streamer |
| **ValidatorsDAO/solana-stream** | UDP shreds (lowest latency path) | github.com/ValidatorsDAO/solana-stream |
| **hanshaze/solana-shredstream-decoder-rust** | Multi-threaded decode, SO_RCVBUF tuning | github.com/hanshaze/solana-shredstream-decoder-rust |
| **aterlo/afxdp-rs** | AF_XDP kernel bypass | github.com/aterlo/afxdp-rs |
| **rpcpool/yellowstone-grpc** | Pre-encoding, Rayon parallel | github.com/rpcpool/yellowstone-grpc |

---

## Roadmap de impacto

```
FASE 1 (trivial):     27ms → ~25ms   (+15-20% alloc speed, -100µs clones)
FASE 2 (arch):        25ms → ~20ms   (executor thread, kanal, pair index)
FASE 3 (relayers):    20ms → 20ms    (misma latencia, +50% landing coverage)
FASE 5 (Geyser):      20ms → ~10ms   (0ms vault updates vs 200ms WS)
FASE 4 (kernel):       10ms → ~7ms   (AF_XDP, bytemuck, io_uring)
```

| Fase | Latencia | Landing rate | Ingreso/mes |
|---|---|---|---|
| Actual | 27ms + 200ms WS | 2-5% | $450-780 |
| F1+F2 | ~20ms + 200ms WS | 3-7% | $600-1,100 |
| +F3 | ~20ms + 200ms WS | 5-10% | $900-1,600 |
| +F5 (Geyser) | ~10ms + 0ms | 15-30% | $2,000-4,500 |
| +F4 (kernel) | ~7ms + 0ms | 20-35% | $2,800-5,500 |
