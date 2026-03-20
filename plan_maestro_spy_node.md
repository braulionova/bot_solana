# Plan Maestro — Helios Gold V2 + Spy Node Rust

> Arquitectura integrada para arbitraje en Solana basada en shreds, detección temprana, flash loans y estrategias de inteligencia operativa
>
> **Pipeline:** `Spy Node → Signal Processor → Route Engine → Flash Loan Selector → Atomic Executor`
>
> Versión integrada • Marzo 2026 • Braulio

---

# PARTE I — VISIÓN Y ARQUITECTURA

## 1. Objetivo Integrado

El sistema no intenta sustituir un validador completo. Copia únicamente la parte del stack que aporta ventaja competitiva al arbitraje: **ingestión temprana, parsing eficiente, decodificación de transacciones, cálculo de edge y envío rápido**.

Unificar en un solo plan la estrategia de spy node ligero en Rust con la estrategia Helios Gold V2 basada en arbitraje atómico y flash loans. El resultado buscado es una infraestructura capaz de **detectar oportunidades temprano**, evaluarlas offchain en milisegundos y ejecutarlas en una transacción segura que solo se confirma si existe beneficio neto.

> El 50% del volumen de DEXes en Solana es arbitraje. La mayoría lo capturan bots con infraestructura de millones de dólares co-locados con validadores de alto stake. Helios Gold V2 no compite en velocidad pura contra Jump/Wintermute. **Compite en inteligencia**: estar donde ellos NO están, saber lo que ellos NO monitorean, y ejecutar cuando ellos NO compiten.

---

## 2. Tesis Operativa

La observación debe ocurrir **fuera de la cadena** y lo más cerca posible del flujo de shreds. La lógica de decisión también debe permanecer offchain para mantener flexibilidad y velocidad. La cadena se usa para la parte que sí necesita atomicidad: préstamo flash, secuencia de swaps, repago, verificación de profit mínimo y entrega del remanente al bot.

Con flash loans, el capital propio se reduce a **5-10 SOL** de working capital para fees y tips. El volumen principal del arbitraje lo aporta el protocolo de lending (MarginFi: 0% fee, Kamino: ~$2.8B TVL, Save/Solend: ~$300M) dentro de la atomicidad de Solana. Si la transacción falla o no hay profit, todo revierte y no se pierde nada.

---

## 3. Arquitectura de Cuatro Capas

| Capa | Función Principal | Resultado |
|---|---|---|
| **Spy Node Rust** | Recibe shreds via Turbine, deduplica, verifica firmas, reconstruye entries con FEC/Reed-Solomon, decodifica TXs | Señales compactas de mercado con latencia pre-ejecución |
| **Motor Offchain** | Filtra por DEX/programa, detecta whales y graduaciones, calcula rutas óptimas con petgraph + Bellman-Ford, decide si disparar | Oportunidad validada con tamaño óptimo y ruta concreta |
| **Programa Onchain** | Ejecuta secuencia atómica: flash borrow → swap multi-hop → flash repay → verify min_profit | Trade seguro atómico: profit o revert completo |
| **Sender / Bundle** | Envía via Jito Block Engine como bundle atómico. Simula antes de enviar. Control de reintentos y tips | Máxima probabilidad de inclusión al menor costo |

### 3.1 Flujo End-to-End

```
[Turbine + Jito ShredStream]
       │
       ▼
┌───────────────────────────────────────────────┐
│ CAPA 1: SPY NODE                                 │
│ UDP Ingest → Shred Parser → SigVerify           │
│ → Dedup LRU → FEC Assembly → Deshredder         │
│ → TX Decoder + ALT Cache                        │
└───────────────────────────────────────────────┘
       │ crossbeam channel: SpySignal
       ▼
┌───────────────────────────────────────────────┐
│ CAPA 2: MOTOR OFFCHAIN                           │
│ Signal Processor (DEX filter, whale detect)      │
│ → Route Engine (petgraph + Bellman-Ford)         │
│ → Scoring System (profit/cost * confidence)      │
│ → Flash Loan Selector (MarginFi > Port > Save)  │
└───────────────────────────────────────────────┘
       │ OpportunityBundle {route, flash_provider, amount}
       ▼
┌───────────────────────────────────────────────┐
│ CAPA 3: PROGRAMA ONCHAIN (helios-arb)            │
│ IX1: flash_borrow (MarginFi/Save/Kamino)         │
│ IX2..N: swap_hops (Raydium/Orca/Meteora/PumpSwap)│
│ IXN+1: flash_repay (principal + fee)             │
│ IXN+2: verify_min_profit (o REVERT)              │
└───────────────────────────────────────────────┘
       │
       ▼
┌───────────────────────────────────────────────┐
│ CAPA 4: SENDER (Jito Block Engine)               │
│ simulateTransaction → sendBundle → telemetría   │
└───────────────────────────────────────────────┘
```

---

# PARTE II — SPY NODE: DISEÑO TÉCNICO

## 4. Spy Node Ligero en Rust

El spy node debe permanecer **pequeño y especializado**. No replay completo, no snapshots, no RPC local, no indexación pesada. Solo ingestión, parsing, dedup, reassembly parcial, decoder y emisión de señales.

### 4.1 Validador Completo vs Spy Node

| Recurso | Validador Completo | Spy Node Helios |
|---|---|---|
| CPU | 24+ cores / 48 threads | 4-8 cores |
| RAM | 128-384 GB | 16-32 GB |
| Storage | 2-4 TB NVMe | 50-100 GB SSD |
| Bandwidth | 1-10 Gbps | 100-500 Mbps |
| SOL requerido | ~1.1 SOL/día (votos) | 0 SOL |
| Costo mensual | $500-2000 | $15-50 (Contabo) |
| Etapas TVU usadas | Todas (Fetch→Verify→Retransmit→Blockstore→Replay→Vote) | Solo Fetch→Verify→FEC→Deshred |

### 4.2 Módulos del Spy Node

| Módulo | Descripción |
|---|---|
| **`udp_ingest`** | Sockets UDP con `SO_REUSEPORT` (4-8 threads), buffers kernel ampliados (134MB `rmem_max`), `recv_mmsg` para batch reception. Bind en puerto TVU `:8002`. Fuente secundaria: Jito ShredStream en `:20000`. |
| **`shred_parser`** | Lee headers de shreds: slot, index, `fec_set_index`, `shred_type` (data/code), variant (Legacy/Merkle). Usa crates de `solana-ledger` para deserialización. |
| **`sig_verifier`** | Verifica firma del líder (ed25519 legacy o Merkle root). Obtiene leader pubkey del gossip service. Descarta shreds con firma inválida. |
| **`deduper`** | LRU cache de 10K entries con clave `hash(slot + index + type)`. Filtra duplicados y shreds fuera de ventana de slot relevante. |
| **`fec_assembler`** | Agrupa shreds en FEC sets. Aplica Reed-Solomon erasure recovery (32 data + 32 coding típico). Tolera 50% de pérdida de paquetes. Emite entry batches completos. |
| **`tx_decoder`** | Concatena payloads de data shreds, deserializa entries (bincode), extrae `VersionedTransaction`. Resuelve ALTs con caché local pre-cargado de DEXes principales. |
| **`signal_bus`** | Canal crossbeam tipado que emite `SpySignal` al motor offchain con latencia <1ms. |

### 4.3 Gossip Client (Modo Spy)

El nodo opera en **modo spy** (`shred_version = 0`). Se conecta a entrypoints de mainnet-beta, publica su socket TVU via gossip, y mantiene el CRDS (Cluster Replicated Data Store) para descubrir peers y leader schedule. No descarga snapshots ni ejecuta replay.

```rust
// Crates extraídos de Agave (anza-xyz/agave):
solana-gossip    → GossipService, ClusterInfo, CrdsValue
solana-ledger    → Shred types, Reed-Solomon, FEC recovery
solana-sdk       → Pubkey, Signature, Keypair, Transaction
solana-streamer  → UDP socket management, PacketBatch
solana-perf      → CUDA runtime, PinnedVec, recyclers
```

### 4.4 Ventaja de Latencia

En el pipeline del validador, los shreds son la señal más temprana posible:

| Fuente | Latencia vs Shreds | Uso |
|---|---|---|
| Shreds brutos (Spy Node) | 0 ms (referencia) | Helios Gold V2 → detección |
| Deshred decoded (Shreder/Triton) | +1-3 ms | Alternativa comercial |
| Geyser gRPC post-replay | +50-200 ms | Yellowstone/Chainstack → confirmación |
| RPC `getTransaction` | +200-500 ms | Verificación tardía |

---

# PARTE III — LAS 5 ESTRATEGIAS DE ARBITRAJE

## 5. Filosofía: Inteligencia sobre Velocidad

No se trata de ser el bot más rápido, sino de **estar donde los bots institucionales NO están**: pools de baja liquidez, tokens recién graduados, momentos específicos del ciclo de vida de un token, y rutas multi-hop que los bots simples no detectan.

**Las 3 ventajas competitivas de un bot independiente:**
1. **Nicho de baja liquidez**: Bots institucionales requieren >$50 profit/trade, Helios es rentable con $2-5
2. **Velocidad de adaptación**: Integrar PumpSwap en horas vs semanas para institucionales
3. **Tolerancia al riesgo de meme coins**: El 1.4% que sobrevive de Pump.fun genera spreads enormes en sus primeras horas

### 5.1 Resumen de Estrategias

| Estrategia | Frecuencia | Flash Loan Size | Profit/Trade | % Ingreso |
|---|---|---|---|---|
| **E1: Graduation Snipe** | 5-15/día | $500-5K | $10-50 | 30-40% |
| **E2: Cross-DEX Cyclic Arb** | 20-80/día | $1K-50K | $2-20 | 25-35% |
| **E3: Whale Backrun** | 2-8/día | $5K-100K | $20-200 | 15-25% |
| **E4: Liquidity Event Arb** | 3-10/día | $500-10K | $5-30 | 10-15% |
| **E5: Stale Oracle Exploit** | 1-5/día | $1K-20K | $3-15 | 5-10% |

---

## 6. E1: Graduation Snipe (PumpSwap → Multi-DEX)

Cuando un token en Pump.fun completa su bonding curve (~$69K market cap), se gradúa instantáneamente a PumpSwap. En los primeros **30-120 segundos**, el precio en PumpSwap diverge del precio en otros venues donde el token pueda estar listado. El Spy Node detecta la graduación via shreds monitoreando el Program ID de PumpSwap y la cuenta de migración.

#### Parámetros E1

| Parámetro | Valor | Razón |
|---|---|---|
| Spread mínimo | 2% | Cubre fees + flash fee + tip + margen |
| Trade size (flash) | $200-5,000 | No agotar pool recién creado |
| Ventana temporal | 0-120s post-grad | Después spread se cierra |
| Max tokens/hora | 5 | Limitar exposición a rugs en serie |
| Blacklist auto | LP no locked, mint authority activa | Seguridad básica |

#### Ejecución con Flash Loan

```
TX Atómica (Jito Bundle):
  IX1: flash_borrow(500 USDC, MarginFi)    // 0% fee
  IX2: swap(USDC → TOKEN, PumpSwap)        // precio bajo post-grad
  IX3: swap(TOKEN → USDC, Raydium/Orca)    // precio alto legacy
  IX4: flash_repay(500 USDC, MarginFi)     // devuelve exacto
  IX5: verify_profit(min = 5 USDC)         // o REVERT
  IX6: jito_tip(0.003 SOL)

Spread 5% sobre 500 USDC = 25 USDC profit bruto
Menos: swap fees (~2.5 USDC) + Jito tip (~$0.40)
Profit neto: ~$22
Capital propio usado: ~$0.40 (solo tip)
```

---

## 7. E2: Cross-DEX Cyclic Arbitrage

Arbitraje triangular o cíclico entre 3+ pools en diferentes DEXes. El grafo de rutas usa `petgraph` con detección de ciclos negativos (Bellman-Ford sobre log-precios). Cuando un swap grande mueve el precio de un pool (detectado via shreds), se re-evalúan todos los ciclos que pasan por ese pool.

### Nichos específicos

- **Meme coin multi-venue**: BONK/WIF/POPCAT en Raydium V4 + CLMM + Orca Whirlpool + Meteora DLMM + PumpSwap. Spreads pequeños (<$5) que institucionales ignoran.
- **Stablecoin triangles**: USDC → USDT → USDe → USDC. Spreads 0.01-0.1%, extremadamente frecuentes, riesgo casi cero. Con flash loan de $50K: $5-50/ciclo.
- **LST circuits**: mSOL → jitoSOL → bSOL → SOL. Spreads cuando hay grandes unstakes. Detectable via whale monitoring + epoch boundaries.

### Ejemplo con Flash Loan

```
Ciclo: SOL → BONK (Raydium) → USDC (Orca) → SOL (Meteora)
Spread: 0.8%

Flash borrow 500 SOL de MarginFi (0% fee)
  → Hop 1: 500 SOL → ~50B BONK (Raydium)
  → Hop 2: 50B BONK → ~66,750 USDC (Orca)
  → Hop 3: 66,750 USDC → ~504 SOL (Meteora)
  → Repay 500 SOL a MarginFi
  → Profit: 4 SOL (~$520) - fees (~0.8 SOL)
  → Neto: ~3.2 SOL ($416)
```

---

## 8. E3: Whale Backrun

Cuando una whale ejecuta un swap grande, el impacto de precio crea una oportunidad instantánea. El Spy Node detecta la TX de la whale via shreds (pre-ejecución) y prepara un backrun posicionado inmediatamente DESPUÉS. **Esto es 100% legal** (no es sandwich/frontrun).

### Detección

- **Umbral dinámico**: TX es whale si mueve >1% de reservas del pool. Pool $100K = threshold $1K.
- **Watchlist PostgreSQL**: Top 500 wallets por volumen, actualizada diariamente. Cruzada con Yellowstone gRPC.
- **Detección por tamaño**: Sin watchlist, calcular tamaño relativo del swap vs reservas del pool en caché.

### Ejemplo con Flash Loan

```
Whale vende 10,000 SOL por USDC en Raydium
  → Impacto: SOL -2% en Raydium vs Orca

Backrun con flash loan:
  IX1: flash_borrow(50,000 USDC, Kamino)
  IX2: buy SOL en Raydium (precio deprimido -2%)
  IX3: sell SOL en Orca (precio no afectado)
  IX4: flash_repay(50,000 + 50 USDC fee)
  → Profit: ~$950 (2% de 50K - fees - flash fee)

Bundle Jito: [whale_tx, backrun_tx] atómico
```

---

## 9. E4: Liquidity Event Arbitrage

Eventos de liquidez predecibles donde los precios divergen temporalmente:

| Evento | Frecuencia | Ventana | Spread Típico |
|---|---|---|---|
| Graduación PumpSwap | 50-200/día | 0-120 seg | 2-15% |
| Add/Remove LP grande | 10-30/día | 1-30 seg | 0.5-3% |
| CLMM range cross | 5-20/día | 5-60 seg | 0.3-2% |
| Epoch boundary LST | Cada ~2.5 días | 1-5 min | 0.1-0.5% |
| Token unlock/vesting | Variable | 5-30 min | 1-5% |

---

## 10. E5: Stale Oracle / Price Lag

Oracles (Pyth, Switchboard) actualizan con 1-5 segundos de lag vs mercado spot. Cuando hay movimiento brusco en un DEX, el oracle aún refleja precio anterior. El bot detecta `|precio_spot - precio_oracle| > 0.3%` y explota la divergencia arbitrando entre DEX spot y cualquier pool/vault que use oracle retrasado. Con flash loan amplifica el volumen sin riesgo de inventario.

---

# PARTE IV — FLASH LOANS Y PROGRAMA ONCHAIN

## 11. Flash Loans en Solana

A diferencia de Ethereum (callbacks reentrantes), Solana usa **instruction introspection**. El protocolo verifica que existe una instrucción de repago más adelante en la misma TX atómica ANTES de liberar fondos. Patrón: `FlashBorrow (IX1) → [operaciones] → FlashRepay (IXN)`. Si cualquier IX falla, toda la TX revierte.

### 11.1 Proveedores

| Protocolo | TVL | Flash Fee | Patrón |
|---|---|---|---|
| **MarginFi** | ~$160M | **0%** | `start_flashloan` / `end_flashloan` (requiere MarginfiAccount) |
| **Save (Solend)** | ~$300M | 0.3% | `FlashBorrowReserveLiquidity` / `FlashRepayReserveLiquidity` |
| **Kamino K-Lend** | ~$2.8B | Variable | Similar a MarginFi V2 |
| **Port Finance** | ~$50M | 0.09% | `flash_borrow` / `flash_repay` |

### 11.2 Flash Loan Aggregator

Selección automática: prioriza **menor fee** (MarginFi 0% primero), luego mayor disponibilidad. Si ningún proveedor cubre el monto, split entre múltiples. Fallback a capital propio para oportunidades pequeñas donde el overhead de flash loan no se justifica.

```
Prioridad: MarginFi (0%) > Port Finance (0.09%) > Save/Solend (0.3%) > Kamino (variable)
```

### 11.3 Cuándo Usar Capital Propio vs Flash Loan

| Escenario | Fuente | Ventaja | Desventaja |
|---|---|---|---|
| Oportunidad pequeña y frecuente (<$50 profit) | Capital propio | Menor overhead, menos cuentas | Escala limitada |
| Oportunidad grande con buena liquidez | Flash loan | Escala sin inmovilizar capital | Más compute, más cuentas |
| Mercado volátil con riesgo de fallo | Capital propio o skip | Control simple del riesgo | Menor captura |
| Ruta compleja multi-hop | Flash loan + programa onchain | Atomicidad y validación profit | Mayor costo operativo/TX |

---

## 12. Programa Onchain: `helios-arb`

El programa actúa como **executor, no como detector**. Su responsabilidad es recibir parámetros validados offchain, verificar límites, ejecutar la secuencia y revertir si el resultado no cubre principal + fees + tips + umbral de profit.

**Responsabilidades del programa:**
- Validar cuentas esperadas
- Ejecutar flash borrow via CPI
- Ejecutar swap hops via CPI (Raydium/Orca/Meteora/PumpSwap)
- Flash repay
- Comprobar profit neto ≥ `min_profit`
- Distribuir ganancias

**Responsabilidades fuera del programa (offchain):**
- Escucha de shreds
- Búsqueda de rutas
- Análisis de mercado
- Simulación continua
- Política de envío
- Scoring y priorización

### Estructura del Programa (Anchor)

```rust
#[program]
pub mod helios_arb {
    pub fn execute_flash_arb(
        ctx: Context<FlashArb>,
        flash_amount: u64,
        min_profit: u64,
        route: RouteParams,
    ) -> Result<()> {
        let pre = token_balance(&ctx.accounts.user_token);
        flash_borrow_cpi(&ctx.accounts.flash_provider, flash_amount)?;
        for hop in route.hops.iter() {
            match hop.dex {
                Raydium => raydium_swap_cpi(..),
                Orca => orca_swap_cpi(..),
                Meteora => meteora_swap_cpi(..),
                PumpSwap => pumpswap_cpi(..),
            }?;
        }
        flash_repay_cpi(&ctx.accounts.flash_provider, amount+fee)?;
        let profit = token_balance(&ctx.accounts.user_token) - pre;
        require!(profit >= min_profit, HeliosError::InsufficientProfit);
        Ok(())
    }
}
```

---

# PARTE V — QUÉ POOLS, CUÁNDO, SCORING

## 13. Qué Pools Monitorear

### 13.1 Tier 1: Siempre Monitoreados

| Par | DEXes | Uso Principal |
|---|---|---|
| SOL/USDC | Raydium, Orca, Meteora, Jupiter | Base cyclic arb + oracle lag |
| SOL/USDT | Raydium, Orca | Triangles con SOL/USDC |
| mSOL/SOL, jitoSOL/SOL | Orca, Raydium, Meteora | LST arbitrage en epoch |
| BONK/SOL, WIF/SOL | Todos los DEXes | Meme coin multi-venue arb |

### 13.2 Tier 2: Auto-Descubiertos

- **Graduaciones PumpSwap**: Cada token nuevo se monitorea 24h. Si tiene pool en otro DEX, entra en monitoreo activo.
- **Pools $10K-100K liquidez** con >$10K volumen/día: Sweet spot: suficiente volumen para generar ineficiencias, muy pequeño para institucionales.

### 13.3 Tier 3: Stablecoins (Income Base)

- USDC → USDT → USDe → USDC
- SOL → mSOL → jitoSOL → SOL
- Profit pequeño pero extremadamente consistente. **Funciona en cualquier mercado.**

---

## 14. Cuándo Ejecutar

| Periodo UTC | Actividad | Estrategia Prioritaria |
|---|---|---|
| 00:00-06:00 | Asia activa, US dormido | E2 (Cyclic) + E5 (Oracle lag) |
| 06:00-12:00 | Europa activa, LP rebalanceos | E4 (Liquidity events) + E2 |
| 12:00-18:00 | US+EU overlap, máximo volumen | E3 (Whale backrun) + E1 (Graduaciones pico) |
| 18:00-00:00 | US prime time, meme coins explotan | E1 (Graduation snipe) + E3 |

**Condiciones de mercado:**
- **Bull market**: Máximas oportunidades en TODAS las estrategias. Priorizar E1 y E3.
- **Mercado lateral**: E2 y E5 funcionan mejor (ineficiencias se acumulan sin ser corregidas).
- **Bear market**: Reducir size. Enfocarse en E2 stablecoin triangles (resistentes a bear).

---

## 15. Sistema de Scoring

```
score = (expected_profit / cost) * confidence * urgency * (1 - risk_factor)
```

| Score | Acción |
|---|---|
| **> 5.0** | Ejecutar inmediatamente con flash loan máximo |
| **2.0-5.0** | Ejecutar si capital idle disponible |
| **1.0-2.0** | Solo si hay capacidad ociosa. Low priority |
| **< 1.0** | Descartar |

---

# PARTE VI — ECONOMÍA Y OPERACIONES

## 16. Economía del Sistema

### 16.1 Costos por Trade

| Componente | Costo |
|---|---|
| Flash fee (MarginFi) | 0% del préstamo |
| Flash fee (Save/Solend) | 0.3% del préstamo |
| DEX swap fees (por hop) | 0.01-0.5% (variable por pool) |
| Jito tip (competitivo) | 0.001-0.05 SOL |
| Priority fee | ~5,000 lamports |
| Slippage real | 0.1-0.5% adicional |

**Fórmula de decisión:**
```
Profit neto = salida estimada - principal - flash fee - priority fee
              - Jito tip - slippage buffer - coste de fallo esperado

Solo ejecutar si profit neto > 2x costos totales.
```

### 16.2 Proyección de Ingresos

| Estrategia | Trades/Día | Profit Diario | Profit Mensual |
|---|---|---|---|
| E1: Graduation Snipe | 5-15 | $50-750 | $1,500-22,500 |
| E2: Cyclic Arb | 20-80 | $40-1,600 | $1,200-48,000 |
| E3: Whale Backrun | 2-8 | $40-1,600 | $1,200-48,000 |
| E4: Liquidity Events | 3-10 | $15-300 | $450-9,000 |
| E5: Oracle Lag | 1-5 | $3-75 | $90-2,250 |
| **TOTAL** | **31-118** | **$148-4,325** | **$4,440-129,750** |

- **Working capital necesario**: 5-10 SOL (~$650-1,300) para Jito tips y priority fees
- **Estimación conservadora realista**: $3,000-15,000/mes consistentemente
- **Meses excepcionales** (bull + alta actividad): $30K+
- **Meses muertos** (bear, poca actividad): $500-2,000
- **Break-even operativo**: ~$90/mes (Contabo $30 + Chainstack $49 + fees). Alcanzable la primera semana solo con stablecoin triangles (E2).

### 16.3 Costos Operativos

| Costo | Mensual |
|---|---|
| Contabo VPS | $15-30 |
| Chainstack RPC (fallback/confirmación) | $49 |
| Jito tips (estimado) | 5-15% del profit bruto |
| Priority fees | <$10 |
| **Total fijo** | **~$75-90** |

---

# PARTE VII — RIESGO E INFRAESTRUCTURA

## 17. Gestión de Riesgo

### 17.1 Reglas Inviolables

1. **Max loss diario**: 5% del working capital. Si se pierde 0.5 SOL en tips fallidos en un día, pausar 24h.
2. **Never hold**: Ninguna posición dura más de 1 bloque (~400ms). Todo atómico via Jito bundles.
3. **Simulación obligatoria**: `simulateTransaction` antes de cada envío. Si falla o profit < umbral, descartar.
4. **Token blacklist**: Automática: mint authority activa, freeze authority, LP no locked, top holder >30% supply.
5. **Max flash amount**: 10% de liquidez disponible del proveedor (no agotar pool de lending).

### 17.2 Riesgos y Mitigaciones

| Riesgo | Impacto | Mitigación |
|---|---|---|
| Sin stake = última capa Turbine | +10-50ms latencia | Jito ShredStream redundante + colocación geográfica |
| Cambios protocolo (Rotor/Alpenglow) | Incompatibilidad | Usar crates Agave actualizables como dependencias |
| Packet loss UDP | FEC sets incompletos | Reed-Solomon tolera 50% loss + ShredStream redundancia |
| Front-run del flash arb | TX falla, costo 0 | Jito bundle + DontFront flag + tip competitivo |
| Rug pull token en E1/E4 | TX revierte, costo 0 | Atómico: si no puedes vender, todo revierte |
| Flash pool sin liquidez | Oportunidad perdida | Aggregator con fallback a múltiples proveedores |
| ALTs no cacheadas | TXs V0 sin resolver | Pre-cargar ALTs de DEXes principales + fallback RPC |

---

## 18. Infraestructura

**Red**: puertos UDP 8000-10000 abiertos + 20000 para ShredStream. Kernel tuning: `net.core.rmem_max=134217728`, `net.core.netdev_max_backlog=30000`.

**Despliegue**: systemd service en Contabo con `restart=always`.

| Perfil | RAM | Uso |
|---|---|---|
| MVP (testing) | 8 GB | Spy node básico + parser + telemetría |
| Punto dulce (operación) | 16 GB | Spy node estable + executor en misma máquina |
| Producción cómoda | 32 GB | Más mercados, caches mayores, observabilidad completa |

### 18.1 Métricas Clave

| KPI | Target | Alerta |
|---|---|---|
| Shreds recibidos/seg | >500 | <200 por >30s |
| FEC recovery rate | >95% | <85% |
| Latencia shred-to-signal (p50) | <3ms | >10ms |
| Win rate trades | >85% | <70% |
| Gossip peers activos | >500 | <200 |
| Profit diario | >$100 | <$30 por 3 días |

**Alertas via Telegram Bot**: trade >$10 profit instantáneo, resumen cada 4h, reporte diario 00:00 UTC, alerta pérdida >3% drawdown.

---

# PARTE VIII — ROADMAP DE IMPLEMENTACIÓN

## 19. Fases de Implementación

| Fase | Semanas | Spy Node | Estrategia + Flash Loans |
|---|---|---|---|
| **1: MVP** | 1-3 | Gossip spy + UDP ingest + shred parser + dedup + métricas de tráfico | Definir pool watchlist, pre-cargar datos de pools |
| **2: Decoder** | 3-5 | SigVerify + FEC assembly + Reed-Solomon + deshredder + TX decoder | Implementar E2 (cyclic arb) con capital propio en devnet |
| **3: Routes** | 5-7 | Signal processor (filtro DEX, whale detect, graduation detect) | Route engine (petgraph + Bellman-Ford) + scoring system |
| **4: Flash Loans** | 7-9 | Integración Spy Node → Motor Offchain via crossbeam | CPI a MarginFi + Save + Kamino. Flash aggregator. Programa helios-arb en devnet |
| **5: Execution** | 9-11 | Optimización hot paths (profiling perf/flamegraph) | TX Builder + Jito bundles. E1 (graduation) + E3 (whale backrun). Testing mainnet mínimo |
| **6: Production** | 11-14 | Deploy 24/7 en Contabo. Prometheus metrics | E4 + E5. Telegram alerts. Tuning parámetros. Scaling gradual del flash amount |

---

## 20. Stack Tecnológico

| Componente | Tecnología |
|---|---|
| Spy Node | Rust (`solana-gossip`, `solana-ledger`, `solana-streamer`, `solana-perf`) |
| Route Engine | Rust (`petgraph`, `crossbeam-channel`, `dashmap`, `lru`) |
| On-Chain Program | Rust + Anchor (`helios-arb`) |
| Flash Loan CPIs | MarginFi SDK + Save SDK + Kamino SDK |
| DEX CPIs | Raydium + Orca Whirlpool + Meteora DLMM + PumpSwap |
| Execution | Jito Block Engine (`jito-sdk-rust`) |
| Shred Sources | Turbine nativo + Jito ShredStream proxy |
| Confirmación | Yellowstone gRPC via Chainstack |
| Database | PostgreSQL (pool states, whale wallets, metrics) |
| Monitoring | Prometheus + Telegram Bot |
| Hosting | Contabo VPS (Ubuntu 24, Nginx, systemd) |

---

## 21. Conclusión

La estrategia combinada correcta es: **escuchar shreds temprano** con un spy node pequeño en Rust, transformar esa observación en señales de mercado con latencia muy baja, decidir fuera de la cadena con un motor de rutas, y ejecutar de forma atómica mediante el programa onchain `helios-arb`, usando flash loans para amplificar cada oportunidad con **zero capital en riesgo**.

Helios Gold V2 es un sistema donde la **inteligencia de la estrategia** (qué pools, qué rutas, cuándo) importa más que la velocidad pura. Las 5 estrategias cubren un espectro completo: desde stablecoin triangles de bajo riesgo para ingreso base diario, hasta graduation snipes amplificados por flash loans para los días buenos.

- El **Spy Node** provee la ventaja de latencia
- Los **Flash Loans** eliminan la restricción de capital
- El **Programa Onchain** garantiza atomicidad
- La **disciplina de riesgo** protege el working capital

> Esta arquitectura mantiene el sistema liviano donde conviene, agresivo donde genera edge y disciplinado en el control del riesgo. Es la forma más coherente de unir observación temprana, ejecución atómica y amplificación de capital en un solo roadmap técnico operativo.
>
> **Capital en riesgo real: 5-10 SOL de working capital para fees. Eso es todo.**
