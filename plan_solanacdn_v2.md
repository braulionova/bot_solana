# Plan SolanaCDN + Yellowstone gRPC + Richat — Nodo Ligero Multi-Fuente
## Módulo de Ingesta para Helios Gold V2

---

## 1. Objetivo

Construir un binario Rust ligero (`helios-ingest`) que agregue múltiples fuentes de datos de Solana mainnet en tiempo real, sin correr un validador completo. El binario combina cuatro fuentes de shreds/transacciones en un bus unificado con dedup, alimentando el Strategy Engine de Helios Gold V2.

**No es un validador. No es un RPC node.** Es un nodo de ingesta puro optimizado para detección temprana de oportunidades de arbitraje con requisitos de hardware mínimos (~4-6 cores, ~8-16GB RAM).

---

## 2. Arquitectura General

```
┌─────────────────────────────────────────────────────────────────────────┐
│                        helios-ingest (Rust binary)                      │
│                                                                         │
│  ┌────────────┐ ┌────────────┐ ┌─────────────────┐ ┌────────────────┐  │
│  │ Fuente 1:  │ │ Fuente 2:  │ │  Fuente 3:      │ │  Fuente 4:     │  │
│  │ Spy Node   │ │ SolanaCDN  │ │  Yellowstone    │ │  Richat        │  │
│  │ (Turbine)  │ │ (Pipe Mesh)│ │  gRPC (SaaS)    │ │  (Self-hosted) │  │
│  │ UDP shreds │ │ CDN shreds │ │  Parsed txs     │ │  gRPC + QUIC   │  │
│  └─────┬──────┘ └─────┬──────┘ └────────┬────────┘ └───────┬────────┘  │
│        │              │                  │                  │           │
│        ▼              ▼                  ▼                  ▼           │
│  ┌─────────────────────────────────────────────────────────────────────┐│
│  │                  Unified Event Bus (tokio mpsc)                     ││
│  │  - Dedup por tx signature + shred index                            ││
│  │  - Race: primera fuente que entrega gana                           ││
│  │  - Métricas: latencia por fuente por evento                        ││
│  │  - Filtro: solo program IDs relevantes pasan                       ││
│  │  - Source tagging: cada evento lleva source_id para analytics       ││
│  └────────────────────────────┬────────────────────────────────────────┘│
│                               │                                         │
│                               ▼                                         │
│  ┌─────────────────────────────────────────────────────────────────────┐│
│  │                  Strategy Engine (Helios Gold V2)                   ││
│  │  E1: Graduation Snipe        E2: Cross-DEX Cyclic Arb              ││
│  │  E3: Whale Backrun           E4: Liquidity Event Arb               ││
│  │  E5: Stale Oracle Exploit                                          ││
│  └────────────────────────────┬────────────────────────────────────────┘│
│                               │                                         │
│                               ▼                                         │
│  ┌─────────────────────────────────────────────────────────────────────┐│
│  │                  TX Sender (Módulo 5)                               ││
│  │  ┌──────────────┐  ┌──────────────┐  ┌───────────────────┐        ││
│  │  │ Path 1:      │  │ Path 2:      │  │ Path 3:           │        ││
│  │  │ Jet TPU QUIC │  │ Jito Bundle  │  │ RPC Fallback      │        ││
│  │  │ (direct to   │  │ (atomic arb) │  │ (sendTransaction) │        ││
│  │  │  leader)     │  │              │  │                   │        ││
│  │  └──────┬───────┘  └──────┬───────┘  └────────┬──────────┘        ││
│  │         │                 │                    │                   ││
│  │         ▼                 ▼                    ▼                   ││
│  │  ┌─────────────────────────────────────────────────────────┐      ││
│  │  │  Confirmation Tracker (gRPC signature subscribe)        │      ││
│  │  │  Landing rate + retry logic + Telegram alerts           │      ││
│  │  └─────────────────────────────────────────────────────────┘      ││
│  └─────────────────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 3. Las 4 Fuentes de Datos

### 3.1 Fuente 1: Spy Node (Turbine Shreds) — Ya diseñado

Componentes extraídos de Agave:
- Gossip Client (descubrimiento de peers)
- Shred Fetch Stage (recepción UDP)
- SigVerify (verificación de firmas)
- FEC Assembler (Reed-Solomon con rayon)
- Deshredder (Entry → Transactions)

**Latencia:** ~50-200μs desde recepción UDP hasta transacciones deserializadas.
**Ventaja:** Información pre-confirmación. Ves transacciones ANTES de que el bloque se confirme.
**Puerto:** UDP :8001 / :8002 (configurable).
**Overhead:** ~2-4 cores, ~4-8GB RAM (componentes Agave extraídos).

**Este módulo ya está diseñado en el Plan Maestro. No se reimplementa aquí.**

---

### 3.2 Fuente 2: SolanaCDN Relay Client (NUEVO)

SolanaCDN es un fork de Agave por Pipe Network que añade una capa CDN para propagación acelerada de shreds a través de una mesh global de 35,000+ nodos PoP. El cliente completo pesa lo mismo que un validador (~384GB RAM), pero solo necesitamos extraer el módulo de relay CDN.

**Repo:** `https://github.com/pipe-network/solanacdn`
**Licencia:** Open source, public good.

**Qué extraer del repo:**
- El cliente que conecta a la mesh de Pipe (35,000+ PoPs)
- El módulo que recibe shreds acelerados via la red overlay
- El forwarder que inyecta shreds al puerto TVU

**Qué NO necesitamos:**
- El validador Agave completo
- Consensus logic, block production, vote submission
- Ledger storage, account state

**Overhead:** <1 core CPU, ~200-500MB RAM.
**Latencia:** P50 ~78ms cross-region (vs ~300ms gossip estándar). 3.8x más rápido que Turbine estándar.
**Costo:** Gratis.

#### 3.2.1 Pasos de Implementación — SolanaCDN Client

```
Paso 1: Clonar repo
  $ git clone https://github.com/pipe-network/solanacdn
  $ cd solanacdn

Paso 2: Identificar módulos CDN
  Buscar en el código:
  - Archivos/módulos relacionados con "cdn", "pipe", "relay", "overlay"
  - La configuración flag que activa la capa CDN
  - El código que conecta a los PoPs de Pipe Network
  - El forwarder que envía shreds al TVU port
  Grep targets:
    $ grep -r "cdn" --include="*.rs" -l
    $ grep -r "pipe" --include="*.rs" -l
    $ grep -r "relay" --include="*.rs" -l
    $ grep -r "tvu" --include="*.rs" -l
    $ grep -r "overlay" --include="*.rs" -l

Paso 3: Extraer en crate independiente
  Crear: helios-ingest/crates/solanacdn-client/
  Copiar solo los módulos identificados.
  Resolver dependencias mínimas (probablemente tokio, tonic o quinn para QUIC).
  Eliminar dependencias de solana-runtime, solana-ledger, etc.

Paso 4: Adaptar interface
  El client debe exponer:
    pub struct CdnShredReceiver {
        config: CdnConfig,
        shred_sender: tokio::sync::mpsc::Sender<RawShred>,
    }

    impl CdnShredReceiver {
        pub async fn connect(config: CdnConfig) -> Result<Self>;
        pub async fn run(&self) -> Result<()>;  // loop de recepción
    }

Paso 5: Integrar con el Event Bus
  Los shreds recibidos del CDN se envían al mismo pipeline
  de FEC Assembly + Deshredding del Spy Node, o directamente
  al bus si ya vienen como entries/transactions parseadas.
```

#### 3.2.2 Configuración

```toml
[cdn]
enabled = true
pipe_endpoint = "auto"          # auto-discovery via DNS/gossip
local_tvu_port = 8003           # puerto local para recibir shreds
reconnect_interval_ms = 5000
metrics_enabled = true
```

---

### 3.3 Fuente 3: Yellowstone gRPC Client — SaaS (NUEVO)

Yellowstone gRPC (Dragon's Mouth) es un plugin Geyser que streama datos parseados directamente desde la memoria del validador. A diferencia de las fuentes 1 y 2 (shreds crudos), esta fuente entrega transacciones ya deserializadas con account state, lo que ahorra ciclos de CPU en el pipeline.

**Ventajas sobre shreds crudos:**
- Transacciones ya parseadas (no hay que deshredear)
- Filtros server-side por program ID (solo recibes lo relevante)
- Account updates en tiempo real (estado de pools)
- Latencia sub-50ms desde memoria del validador
- Slot replay para recuperar eventos perdidos durante desconexiones

**Proveedores evaluados:**

| Proveedor | Precio | Ventaja | Desventaja |
|-----------|--------|---------|------------|
| Helius | Desde $49/mes | Ya tenemos API key, docs Rust sólidas | Límites de filtros en plan bajo |
| Triton (Dragon's Mouth) | Desde $99/mes | Creadores de Yellowstone, intra-slot updates, 400ms ventaja DeFi | Más caro |
| Shyft | Desde $49/mes | RabbitStream (pre-confirmed txs desde shreds ~10ms antes que gRPC estándar), unmetered, 7 regiones | Docs Rust menos maduras |
| Chainstack | Desde $49/mes | Jito ShredStream por defecto en nodos, slot replay 150 slots | Límite 50 accounts por stream |
| ERPC | Desde $49/mes | Multi-región (FRA, AMS, NYC, CHI, TYO, SGP), usa Richat internamente | Más nuevo en el mercado |

**Recomendación:** Helius (ya tenemos relación) o Shyft (RabbitStream da edge extra).

#### 3.3.1 Pasos de Implementación — Yellowstone gRPC Client

```
Paso 1: Agregar dependencias
  # Cargo.toml
  [dependencies]
  yellowstone-grpc-client = "2.1"    # o última versión
  yellowstone-grpc-proto = "2.1"
  tokio = { version = "1", features = ["full"] }
  tonic = { version = "0.12", features = ["tls", "tls-roots"] }
  futures = "0.3"
  tracing = "0.1"

Paso 2: Crear módulo grpc_source
  Archivo: helios-ingest/crates/grpc-client/src/lib.rs

  Estructura principal:
    pub struct GrpcSource {
        endpoint: String,
        token: String,
        program_filters: Vec<Pubkey>,
        event_sender: tokio::sync::mpsc::Sender<IngestEvent>,
    }

    impl GrpcSource {
        pub async fn connect(&self) -> Result<()>;
        pub async fn subscribe(&self, filters: SubscribeRequest) -> Result<()>;
        pub async fn run_with_reconnect(&self) -> Result<()>;
    }

Paso 3: Definir filtros por estrategia

  Los filtros se configuran para recibir SOLO lo relevante:

  E1 (Graduation Snipe):
    - Program ID: PumpSwap (6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P)
    - Program ID: Pump.fun migration
    - Filtro: transacciones que involucren la cuenta de migración

  E2 (Cross-DEX Cyclic Arb):
    - Program ID: Raydium AMM V4 (675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8)
    - Program ID: Orca Whirlpool (whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc)
    - Program ID: Meteora DLMM (LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo)
    - Filtro: account updates de pool states monitoreados

  E3 (Whale Backrun):
    - Filtro: transacciones > umbral de tamaño en pools target
    - Account subscribe: wallets de whales conocidos

  E4 (Liquidity Event Arb):
    - Filtro: instrucciones addLiquidity / removeLiquidity en DEXes target

  E5 (Stale Oracle):
    - Program ID: Pyth (FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH)
    - Program ID: Switchboard
    - Filtro: account updates de price feeds monitoreados

Paso 4: Implementar handler de eventos
  Cada mensaje del stream se clasifica por tipo:
    - Transaction → extraer program IDs, decodificar instrucciones
    - Account Update → actualizar cache local de pool states
    - Slot → tracking de confirmaciones

Paso 5: Integrar con Event Bus
  Los eventos parseados se envían al bus unificado con:
    IngestEvent {
        source: Source::YellowstoneGrpc,
        timestamp: Instant::now(),
        slot: u64,
        event_type: EventType::Transaction | EventType::AccountUpdate,
        signature: Option<Signature>,
        data: EventData,
    }
```

#### 3.3.2 Configuración

```toml
[grpc]
enabled = true
provider = "helius"
endpoint = "https://mainnet.helius-rpc.com:443"
token = "${HELIUS_API_KEY}"
reconnect_backoff_ms = 1000
reconnect_max_ms = 30000
commitment = "processed"

[grpc.filters]
programs = [
    "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",   # PumpSwap
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8",   # Raydium AMM V4
    "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",     # Orca Whirlpool
    "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",     # Meteora DLMM
    "FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH",    # Pyth Oracle
]
subscribe_accounts = true
subscribe_transactions = true
subscribe_slots = true
```

---

### 3.4 Fuente 4: Richat — Self-Hosted gRPC/QUIC Streaming (NUEVO)

#### 3.4.1 ¿Qué es Richat?

Richat es un framework de streaming open-source desarrollado por Lamports Dev, diseñado para entregar datos de blockchain Solana con baja latencia y alta confiabilidad. Es el sucesor funcional de Yellowstone gRPC para operadores que quieren self-hosted streaming con capacidades superiores.

**Repo:** `https://github.com/lamports-dev/richat`
**Licencia:** Open source.
**Creador:** Lamports Dev (mismos desarrolladores de `yellowstone-grpc-proto`, `alpamayo`, `solfees`).
**Adopción:** ERPC (Validators DAO) lo adoptó como default reemplazando Yellowstone en dic 2025. SLV v0.9.902 lo usa como default para deployment de nodos gRPC Geyser.

**Diferencias clave vs Yellowstone gRPC (SaaS):**

| Aspecto | Yellowstone gRPC (Fuente 3) | Richat (Fuente 4) |
|---------|----------------------------|-------------------|
| Modelo | SaaS — pagas a un proveedor | Self-hosted — corres tu propia instancia |
| Costo recurrente | $49-99+/mes | $0 (solo costo del servidor) |
| Dependencia | Proveedor externo (Helius, Triton, etc.) | Tu propia infraestructura |
| Latencia | Sub-50ms (depende del proveedor y región) | Potencialmente menor (sin proxy intermedio) |
| Protocolo | gRPC (HTTP/2) | gRPC + QUIC nativo (menor latencia, sin HOL blocking) |
| Multiplexing | No (single stream) | Sí — consume múltiples fuentes gRPC y unifica en un solo stream |
| Filtrado | Server-side en el proveedor | Local — filtrado por nivel en topologías jerárquicas |
| Dedup | Depende del proveedor | Built-in — suprime procesamiento redundante |
| Resiliencia | Depende del proveedor | Tú controlas failover, backup sources |
| Compatibilidad | Protocolo Yellowstone estándar | 100% compatible con yellowstone-grpc-proto |

#### 3.4.2 Arquitectura de Richat

Richat tiene una arquitectura modular con varios componentes:

```
┌─────────────────────────────────────────────────────────────────┐
│                     Richat Streaming System                      │
│                                                                  │
│  ┌──────────────┐     ┌──────────────┐                          │
│  │ agave node 1 │     │ agave node 2 │    (upstream sources)    │
│  │  richat-     │     │  richat-     │    O proveedores gRPC    │
│  │  plugin-agave│     │  plugin-agave│    externos (Helius etc) │
│  └──────┬───────┘     └──────┬───────┘                          │
│         │                    │                                   │
│         ▼                    ▼                                   │
│  ┌─────────────────────────────────────┐                        │
│  │         richat-server               │                        │
│  │  ┌─────────────────────────────┐    │                        │
│  │  │ Receiver Runtime            │    │                        │
│  │  │  - messages storage         │    │                        │
│  │  │  - multiplexing sources     │    │                        │
│  │  │  - dedup & filtering        │    │                        │
│  │  └──────────┬──────────────────┘    │                        │
│  │             │                       │                        │
│  │  ┌──────────▼──────────────────┐    │                        │
│  │  │ Server Runtime              │    │                        │
│  │  │  ┌───────┐  ┌────────────┐  │    │                        │
│  │  │  │ gRPC  │  │ Solana     │  │    │                        │
│  │  │  │server │  │ PubSub     │  │    │                        │
│  │  │  │:10000 │  │ (WebSocket)│  │    │                        │
│  │  │  └───────┘  └────────────┘  │    │                        │
│  │  │  ┌────────────────────────┐ │    │                        │
│  │  │  │ QUIC server           │ │    │                        │
│  │  │  │ :10001                │ │    │                        │
│  │  │  └────────────────────────┘ │    │                        │
│  │  └─────────────────────────────┘    │                        │
│  └─────────────────────────────────────┘                        │
└─────────────────────────────────────────────────────────────────┘
```

**Capacidades clave para Helios Gold:**

1. **Multiplexing:** Consume múltiples fuentes gRPC upstream y las entrega como un solo stream unificado. Puedes conectar Richat a 2-3 proveedores gRPC (Helius + Triton + Shyft) y Richat los unifica, deduplica y te da un solo stream limpio.

2. **QUIC nativo:** Además de gRPC (HTTP/2), Richat soporta streaming sobre QUIC, que elimina head-of-line blocking. Si un paquete se pierde en un stream, los otros streams siguen fluyendo. Para MEV donde cada ms cuenta, esto es significativo.

3. **Topologías jerárquicas:** Puedes encadenar instancias de Richat con filtrado en cada nivel. Ejemplo: Richat L1 recibe TODO desde Geyser → Richat L2 filtra solo tus program IDs → tu bot consume de L2.

4. **Dedup integrado:** Suprime procesamiento redundante y retransmisiones innecesarias a nivel de delivery.

5. **Compatible con Yellowstone gRPC:** Richat es compatible con los endpoints gRPC de proveedores comerciales. Puedes usarlo como proxy/aggregador frente a proveedores SaaS sin cambiar tu código cliente.

#### 3.4.3 Dos Modos de Uso para Helios Gold

**Modo A: Richat como Aggregador de Fuentes gRPC (RECOMENDADO FASE 1)**

No necesitas correr un nodo Agave. Usas Richat como proxy inteligente que consume de proveedores SaaS y unifica:

```
┌──────────────────────────┐
│  Helius gRPC             │──┐
│  (endpoint + token)      │  │
└──────────────────────────┘  │
                              ▼
┌──────────────────────────┐  ┌──────────────────────┐
│  Triton gRPC             │──│  Richat Server       │──→ helios-ingest
│  (endpoint + token)      │  │  (local, tu server)  │    Event Bus
└──────────────────────────┘  │  - Multiplexing      │
                              │  - Dedup             │
┌──────────────────────────┐  │  - QUIC output       │
│  Shyft gRPC              │──│  - Filtrado local    │
│  (endpoint + token)      │  └──────────────────────┘
└──────────────────────────┘
```

**Ventajas:**
- No necesitas 384GB RAM para un nodo Agave
- Redundancia: si Helius cae, Triton y Shyft siguen entregando
- Dedup automático: no procesas la misma tx 3 veces
- Métricas: puedes medir qué proveedor entrega primero
- QUIC output: tu bot consume via QUIC en vez de gRPC, eliminando HOL blocking
- Overhead: ~1-2GB RAM, 1-2 cores

**Desventaja:**
- Sigues pagando a proveedores SaaS (pero con mejor resiliencia)
- Latencia limitada por el proveedor más lento (pero solo usas el más rápido por dedup)

**Modo B: Richat Full Self-Hosted (FASE AVANZADA)**

Corres tu propio nodo Agave `--no-voting` con el plugin `richat-plugin-agave`, y Richat streama directamente desde tu validador local:

```
┌──────────────────────────────────────┐
│  Tu servidor dedicado (Hetzner AX102) │
│                                       │
│  agave-validator --no-voting          │
│    + richat-plugin-agave              │
│         │                             │
│         ▼                             │
│  richat-server                        │
│    gRPC + QUIC server                 │
│         │                             │
│         ▼                             │
│  helios-ingest                        │
│    Event Bus → Strategy Engine        │
└──────────────────────────────────────┘
```

**Ventajas:**
- $0 costo recurrente por gRPC (solo costo del servidor)
- Latencia mínima absoluta (Geyser → Richat → bot, todo local, sin red)
- Control total sobre filtros, throughput, priorización
- QUIC nativo para delivery interno

**Desventaja:**
- Requiere correr un nodo Agave non-voting (mínimo 256GB RAM, 24 cores, 2TB+ NVMe)
- Hardware significativamente más caro (~€104+/mes por un AX102, o más para specs completas)
- Mantenimiento operativo del nodo (updates, snapshots, monitoreo)

**Recomendación:** Empezar con Modo A (aggregador). Migrar a Modo B cuando el volumen de trading justifique el costo del hardware dedicado.

#### 3.4.4 Pasos de Implementación — Richat Modo A (Aggregador)

```
Paso 1: Clonar y compilar Richat
  $ git clone https://github.com/lamports-dev/richat
  $ cd richat
  $ cargo build --release --bin richat-server

  Binario resultante: target/release/richat-server
  Nota: NO necesitas compilar richat-plugin-agave (eso es para Modo B).

Paso 2: Examinar estructura del repo
  $ ls richat/
  Buscar:
    - Directorio de configuración y ejemplos
    - README.md para instrucciones de setup como aggregador
    - Opciones de línea de comando del binario
  $ ./target/release/richat-server --help
  $ cat README.md
  $ find . -name "*.toml" -o -name "*.json" | head -20

Paso 3: Configurar como aggregador de fuentes gRPC
  Crear config basado en la documentación del repo.
  El config debe especificar:
    - Upstream sources (endpoints gRPC de proveedores)
    - Server binds (gRPC :10000, QUIC :10001)
    - Filtros
    - Métricas Prometheus

  NOTA: La estructura exacta del config puede variar según la versión.
  Consultar README y config-examples del repo para la versión actual.

Paso 4: Deploy como systemd service

  $ sudo tee /etc/systemd/system/richat.service << 'EOF'
  [Unit]
  Description=Richat gRPC Aggregator for Helios Gold V2
  After=network-online.target
  Wants=network-online.target

  [Service]
  Type=simple
  User=helios
  ExecStart=/usr/local/bin/richat-server --config /etc/richat/config.json
  Restart=always
  RestartSec=5
  LimitNOFILE=1000000
  Environment="HELIUS_API_KEY=xxx"
  Environment="TRITON_TOKEN=xxx"

  [Install]
  WantedBy=multi-user.target
  EOF

  $ sudo systemctl enable --now richat

Paso 5: Crear crate cliente en helios-ingest

  Archivo: helios-ingest/crates/richat-client/src/lib.rs

  El cliente se conecta al Richat server local via gRPC o QUIC:

    pub struct RichatSource {
        endpoint: String,           // "127.0.0.1:10000" (gRPC) o ":10001" (QUIC)
        protocol: Protocol,         // gRPC | QUIC
        filters: SubscribeRequest,
        event_sender: mpsc::Sender<IngestEvent>,
    }

    pub enum Protocol {
        Grpc,
        Quic,
    }

    impl RichatSource {
        pub async fn connect(config: RichatConfig) -> Result<Self>;

        pub async fn run(&self) -> Result<()> {
            // Loop principal:
            // 1. Conectar al richat-server local
            // 2. Enviar SubscribeRequest con filtros
            // 3. Por cada mensaje recibido:
            //    a. Clasificar (Transaction | AccountUpdate | Slot)
            //    b. Wrappear en IngestEvent con source = Source::Richat
            //    c. Enviar al Event Bus
            // 4. En caso de desconexión: reconexión con backoff
        }
    }

  NOTA: Richat es compatible con yellowstone-grpc-proto, así que puedes
  reusar los mismos tipos SubscribeRequest/SubscribeUpdate del crate
  yellowstone-grpc-proto. La diferencia es solo el endpoint al que conectas.

Paso 6: Integrar con Event Bus
  Igual que Fuente 3, pero con source tag diferente:
    IngestEvent {
        source: Source::Richat,
        timestamp: Instant::now(),
        slot: u64,
        event_type: EventType,
        signature: Option<Signature>,
        data: EventData,
    }
```

#### 3.4.5 Configuración

```toml
[richat]
enabled = true
mode = "aggregator"                 # aggregator | self_hosted
local_grpc_endpoint = "http://127.0.0.1:10000"
local_quic_endpoint = "127.0.0.1:10001"
protocol = "quic"                   # grpc | quic (quic para menor latencia)
reconnect_backoff_ms = 1000
reconnect_max_ms = 15000

# Upstream sources (solo para mode = "aggregator")
[[richat.upstream]]
provider = "helius"
endpoint = "https://mainnet.helius-rpc.com:443"
token = "${HELIUS_API_KEY}"

[[richat.upstream]]
provider = "triton"
endpoint = "https://your-triton-endpoint:443"
token = "${TRITON_TOKEN}"

# Filtros locales
[richat.filters]
programs = [
    "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",   # PumpSwap
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8",   # Raydium AMM V4
    "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",     # Orca Whirlpool
    "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",     # Meteora DLMM
    "FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH",    # Pyth Oracle
]
```

#### 3.4.6 Richat vs Yellowstone gRPC Directo — Cuándo usar cuál

```
¿Tienes 1 solo proveedor gRPC?
  → Usa Yellowstone gRPC directo (Fuente 3). Richat no agrega valor.

¿Tienes 2+ proveedores gRPC?
  → Usa Richat como aggregador. Dedup + multiplexing + failover automático.

¿Quieres QUIC en vez de gRPC para menor latencia?
  → Richat es la única opción. Yellowstone directo es solo gRPC (HTTP/2).

¿Quieres medir qué proveedor entrega más rápido?
  → Richat con métricas Prometheus te da eso out-of-the-box.

¿Tienes servidor dedicado con 256GB+ RAM y quieres $0/mes en gRPC?
  → Richat Modo B (self-hosted con tu propio nodo Agave).
```

---

## 4. Unified Event Bus

El bus unificado es el corazón de `helios-ingest`. Recibe eventos de las 4 fuentes y los entrega deduplicados al Strategy Engine.

### 4.1 Estructura del Evento

```rust
pub struct IngestEvent {
    pub source: Source,
    pub received_at: Instant,
    pub slot: u64,
    pub event_type: EventType,
    pub signature: Option<Signature>,   // None para AccountUpdates sin tx
    pub data: EventData,
}

pub enum Source {
    SpyNode,            // Fuente 1: Turbine shreds
    SolanaCdn,          // Fuente 2: Pipe Network CDN
    YellowstoneGrpc,    // Fuente 3: SaaS provider directo
    Richat,             // Fuente 4: Richat aggregador/self-hosted
}

pub enum EventType {
    Transaction,
    AccountUpdate,
    SlotUpdate,
}

pub enum EventData {
    RawShreds(Vec<RawShred>),               // De Fuentes 1, 2
    ParsedTransaction(TransactionInfo),      // De Fuentes 3, 4
    AccountUpdate(AccountUpdateInfo),        // De Fuentes 3, 4
    SlotNotification(SlotInfo),              // De cualquier fuente
}
```

### 4.2 Lógica de Dedup

```rust
pub struct EventBus {
    seen_signatures: LruCache<Signature, SourceTiming>,
    seen_shred_indices: LruCache<(u64, u32), Source>,  // (slot, shred_index)
    strategy_sender: mpsc::Sender<StrategyEvent>,
    metrics: BusMetrics,
}

impl EventBus {
    pub fn process(&mut self, event: IngestEvent) {
        match &event.signature {
            Some(sig) => {
                if let Some(existing) = self.seen_signatures.get(sig) {
                    // Ya vimos esta tx — registrar métrica de latencia comparativa
                    self.metrics.record_duplicate(
                        event.source,
                        existing.first_source,
                        event.received_at - existing.first_seen,
                    );
                    return; // DROP duplicado
                }
                // Primera vez que vemos esta tx
                self.seen_signatures.put(*sig, SourceTiming {
                    first_source: event.source,
                    first_seen: event.received_at,
                });
            }
            None => {} // AccountUpdates se procesan siempre (latest wins)
        }

        // Convertir y enviar al Strategy Engine
        if let Some(strategy_event) = self.classify_event(&event) {
            let _ = self.strategy_sender.try_send(strategy_event);
        }
    }
}
```

### 4.3 Métricas del Bus (Prometheus)

```
# Qué fuente entrega primero por tipo de evento
helios_ingest_first_source_total{source="spy_node", event_type="transaction"} 4521
helios_ingest_first_source_total{source="richat", event_type="transaction"} 2103
helios_ingest_first_source_total{source="solanacdn", event_type="transaction"} 891
helios_ingest_first_source_total{source="yellowstone", event_type="transaction"} 342

# Latencia promedio por fuente (ms desde evento on-chain)
helios_ingest_latency_ms{source="spy_node", quantile="p50"} 0.12
helios_ingest_latency_ms{source="solanacdn", quantile="p50"} 78
helios_ingest_latency_ms{source="richat", quantile="p50"} 45
helios_ingest_latency_ms{source="yellowstone", quantile="p50"} 48

# Duplicados descartados por fuente
helios_ingest_duplicates_total{source="richat"} 15203
helios_ingest_duplicates_total{source="yellowstone"} 18402

# Eventos procesados por segundo
helios_ingest_events_per_second{source="spy_node"} 1250
helios_ingest_events_per_second{source="richat"} 890
```

---

## 5. Módulo TX Sender — Envío de Transacciones Live Mainnet

### 5.1 El Problema

gRPC/Yellowstone/Richat son **solo lectura** — reciben datos del blockchain pero no envían transacciones. Para cerrar el loop de arbitraje, necesitas un módulo de envío que mantenga conexiones QUIC live con los leaders y entregue transacciones firmadas directamente al TPU, sin pasar por el RPC.

El envío de transacciones en Solana funciona así: el leader schedule es determinista y conocido por adelantado cada epoch (~2 días). Transacciones se envían directamente al leader actual y al siguiente via QUIC a su puerto TPU. Los leaders aplican SWQoS (Stake-weighted QoS): 80% de capacidad TPU reservada para validadores con stake, 20% para unstaked. Como Helios Gold es unstaked, maximizar landing rate requiere envío multi-path.

### 5.2 Arquitectura del TX Sender

```
┌──────────────────────────────────────────────────────────────────────┐
│                      helios-tx-sender (Rust)                          │
│                                                                       │
│  ┌────────────────────────────────────────────────────────────────┐  │
│  │  Path 1: Yellowstone Jet TPU Client (PRIMARIO para txs directas)│  │
│  │                                                                 │  │
│  │  gRPC stream ──→ Leader Schedule Live ──→ QUIC Connections      │  │
│  │    (Helius/Triton)   (current + next)      (pre-calentadas)     │  │
│  │                                                                 │  │
│  │  TX firmada ──→ Route to correct leader ──→ TPU QUIC port       │  │
│  │                                                                 │  │
│  │  Features:                                                      │  │
│  │  - Conexiones QUIC mantenidas live con leaders actuales          │  │
│  │  - Rotation automática cuando cambia el leader                  │  │
│  │  - Shield blocklist: evitar leaders que hacen sandwich           │  │
│  │  - Callback de tracking por signature                           │  │
│  └────────────────────────────────────────────────────────────────┘  │
│                                                                       │
│  ┌────────────────────────────────────────────────────────────────┐  │
│  │  Path 2: Jito Bundle Engine (PRIMARIO para arb atómico)        │  │
│  │                                                                 │  │
│  │  Bundle (múltiples IXs atómicas) ──→ Jito Block Engine         │  │
│  │    flash_borrow → swap → swap → repay → verify_profit          │  │
│  │    + jito_tip                                                   │  │
│  │                                                                 │  │
│  │  Usado para: E1 graduation snipe, E2 cyclic arb,               │  │
│  │              E3 whale backrun (cualquier operación atómica)     │  │
│  └────────────────────────────────────────────────────────────────┘  │
│                                                                       │
│  ┌────────────────────────────────────────────────────────────────┐  │
│  │  Path 3: RPC sendTransaction (FALLBACK)                        │  │
│  │                                                                 │  │
│  │  Helius/Triton RPC ──→ sendTransaction JSON-RPC                │  │
│  │  Solo cuando Path 1 y 2 fallan o para txs no-urgentes          │  │
│  └────────────────────────────────────────────────────────────────┘  │
│                                                                       │
│  ┌────────────────────────────────────────────────────────────────┐  │
│  │  Confirmation Tracker                                          │  │
│  │                                                                 │  │
│  │  gRPC subscribe signatures ──→ Track landing status            │  │
│  │  - Timeout + retry logic                                       │  │
│  │  - Métricas: landing rate, latencia, slot delta                │  │
│  │  - Resubmit via path alternativo si timeout                    │  │
│  └────────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────────┘
```

### 5.3 Path 1: Yellowstone Jet TPU Client

Yellowstone Jet TPU Client es un crate de Triton que extrae la lógica de envío de su motor Jet en un crate Rust standalone. Maneja SWQoS, QUIC, y cambios de Agave out of the box. Usa gRPC internamente para trackear el leader schedule en tiempo real y mantiene conexiones QUIC pre-calentadas con los leaders.

**Repo:** `https://github.com/rpcpool/yellowstone-jet-tpu-client`

#### 5.3.1 Dependencias

```toml
[dependencies]
# Jet TPU Client
yellowstone-jet-tpu-client = { version = "0.1.0", features = ["yellowstone-grpc", "shield"] }
yellowstone-shield-store = "0.9.1"
yellowstone-grpc-client = "10.2.0"

# Solana core (v3.0 para compatibilidad con Jet)
solana-client = "3.0"
solana-keypair = "3.0"
solana-pubkey = "3.0"
solana-signature = "3.0"
solana-transaction = "3.0"
solana-message = "3.0"
solana-system-interface = { version = "3.0", features = ["bincode"] }
solana-commitment-config = "3.0"
solana-signer = "3.0"

# Jito (Path 2)
jito-sdk-rust = "0.3"

# Async
tokio = { version = "1", features = ["full"] }
futures = "0.3"
anyhow = "1"
tracing = "0.1"
```

**NOTA:** Jet TPU Client usa solana crates v3.0 (Agave 2.3+). Si el resto de helios-ingest usa v2.1, puede haber conflictos. Soluciones:
- Opción A: Mover todo el workspace a solana v3.0
- Opción B: tx-sender como binario separado comunicado via mpsc/IPC
- Opción C: Usar `[patch.crates-io]` con patches de Triton

#### 5.3.2 Estructura del TX Sender

```rust
// helios-ingest/crates/tx-sender/src/lib.rs

use yellowstone_jet_tpu_client::{
    core::TpuSenderResponse,
    yellowstone_grpc::sender::{
        Endpoints, create_yellowstone_tpu_sender_with_callback,
    },
};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_keypair::Keypair;
use solana_transaction::versioned::VersionedTransaction;
use tokio::sync::mpsc;
use std::sync::Arc;

/// Evento que el Strategy Engine envía al TX Sender
pub enum SendRequest {
    /// Enviar via TPU QUIC directo (Path 1)
    DirectTpu {
        tx: VersionedTransaction,
        priority: TxPriority,
    },
    /// Enviar como Jito Bundle (Path 2)
    JitoBundle {
        txs: Vec<VersionedTransaction>,
        tip_lamports: u64,
    },
    /// Fallback via RPC (Path 3)
    RpcFallback {
        tx: VersionedTransaction,
    },
}

pub enum TxPriority {
    Critical,   // Graduation snipe, whale backrun — enviar a leader + next
    Normal,     // Cyclic arb — enviar a leader actual
    Low,        // Rebalance, cleanup — RPC fallback OK
}

pub struct TxSender {
    jet_sender: YellowstoneTpuSender,
    request_rx: mpsc::Receiver<SendRequest>,
    rpc: Arc<RpcClient>,
    jito_client: JitoClient,
    metrics: TxSenderMetrics,
}

impl TxSender {
    pub async fn new(
        config: TxSenderConfig,
        request_rx: mpsc::Receiver<SendRequest>,
    ) -> Result<Self> {
        let endpoints = Endpoints {
            rpc: config.rpc_endpoint.clone(),
            grpc: config.grpc_endpoint.clone(),
            x_token: config.grpc_token.clone(),
        };

        let identity = Arc::new(
            Keypair::read_from_file(&config.identity_keypair_path)?
        );

        // Jet sender con callback para tracking de confirmaciones
        let (jet_sender, mut response_rx) =
            create_yellowstone_tpu_sender_with_callback(
                endpoints, identity,
            ).await?;

        // Spawn response handler
        tokio::spawn(async move {
            while let Some(response) = response_rx.recv().await {
                match response {
                    TpuSenderResponse::Sent { signature, slot } => {
                        tracing::info!("TX landed: {} slot {}", signature, slot);
                    }
                    TpuSenderResponse::Failed { signature, error } => {
                        tracing::warn!("TX failed: {} - {}", signature, error);
                    }
                }
            }
        });

        Ok(Self {
            jet_sender,
            request_rx,
            rpc: Arc::new(RpcClient::new(config.rpc_endpoint)),
            jito_client: JitoClient::new(config.jito_endpoint),
            metrics: TxSenderMetrics::default(),
        })
    }

    /// Loop principal — procesa requests del Strategy Engine
    pub async fn run(&mut self) -> Result<()> {
        while let Some(request) = self.request_rx.recv().await {
            match request {
                SendRequest::DirectTpu { tx, priority } => {
                    // Jet maneja: gRPC → leader lookup → Shield check → QUIC send
                    if let Err(e) = self.jet_sender.send(tx).await {
                        tracing::error!("Direct TPU failed: {}", e);
                        self.metrics.direct_failed.inc();
                    } else {
                        self.metrics.direct_sent.inc();
                    }
                }
                SendRequest::JitoBundle { txs, tip_lamports } => {
                    // Bundle atómico: flash_borrow + swaps + repay + tip
                    match self.jito_client.send_bundle(txs, tip_lamports).await {
                        Ok(id) => {
                            tracing::info!("Jito bundle: {}", id);
                            self.metrics.jito_sent.inc();
                        }
                        Err(e) => {
                            tracing::error!("Jito failed: {}", e);
                            self.metrics.jito_failed.inc();
                        }
                    }
                }
                SendRequest::RpcFallback { tx } => {
                    match self.rpc.send_transaction(&tx).await {
                        Ok(sig) => {
                            tracing::info!("RPC fallback: {}", sig);
                            self.metrics.rpc_sent.inc();
                        }
                        Err(e) => {
                            tracing::error!("RPC failed: {}", e);
                            self.metrics.rpc_failed.inc();
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
```

#### 5.3.3 Shield — Protección Anti-MEV

Yellowstone Shield permite crear blocklists de validators on-chain. Si el leader actual hace sandwich, el TX Sender salta ese slot:

```rust
use yellowstone_shield_store::{PolicyStore, PolicyStoreConfig};

// Shield evalúa cada leader antes de enviar:
//   Leader actual = Validator X
//     → ¿X está en blocklist?
//       → SÍ: No enviar, esperar leader+1
//       → NO: Enviar via QUIC al TPU de X
```

#### 5.3.4 Cuándo usar cada Path

```
Strategy Engine genera TX
    │
    ├─ ¿Es arbitraje atómico (multi-IX con flash loan)?
    │   └─ SÍ → Path 2: Jito Bundle
    │        (E1 graduation, E2 cyclic, E3 whale backrun)
    │
    ├─ ¿Es TX simple de alta prioridad?
    │   └─ SÍ → Path 1: Jet TPU Client (QUIC directo)
    │        (E4 liquidity event, E5 oracle lag)
    │
    └─ ¿Falló Path 1/2? ¿O baja prioridad?
        └─ SÍ → Path 3: RPC sendTransaction
             (rebalances, cleanup, retries)
```

#### 5.3.5 Confirmation Tracker

```rust
pub struct ConfirmationTracker {
    grpc_client: GeyserGrpcClient,
    pending: DashMap<Signature, PendingTx>,
    timeout: Duration,  // 30s default
}

pub struct PendingTx {
    pub sent_at: Instant,
    pub path_used: SendPath,
    pub retry_count: u32,
    pub max_retries: u32,       // 2 default
    pub original_request: SendRequest,
}

// Flujo:
// 1. Suscribir signature via gRPC
// 2. Si confirmed → remover de pending, registrar métrica
// 3. Si failed → retry via path alternativo
// 4. Si timeout → resubmit con nuevo blockhash
// 5. Si max_retries → drop + alert Telegram
```

#### 5.3.6 Configuración

```toml
[tx_sender]
enabled = true
identity_keypair_path = "/home/helios/.config/solana/id.json"

# Path 1: Jet TPU Client
[tx_sender.jet]
grpc_endpoint = "https://mainnet.helius-rpc.com:443"
grpc_token = "${HELIUS_API_KEY}"
rpc_endpoint = "https://mainnet.helius-rpc.com"
shield_enabled = true
shield_policy_address = ""      # Policy on-chain con blocklist

# Path 2: Jito Bundles
[tx_sender.jito]
block_engine_url = "https://mainnet.block-engine.jito.wtf"
default_tip_lamports = 10000    # ~0.00001 SOL

# Path 3: RPC Fallback
[tx_sender.rpc]
endpoint = "https://mainnet.helius-rpc.com"
max_retries = 3
retry_interval_ms = 500

# Confirmation
[tx_sender.confirmation]
timeout_ms = 30000
max_retries = 2
```

#### 5.3.7 Métricas del TX Sender (Prometheus)

```
# Enviados por path
helios_tx_sent_total{path="jet_tpu"} 4521
helios_tx_sent_total{path="jito_bundle"} 890
helios_tx_sent_total{path="rpc_fallback"} 123

# Landing rate por path
helios_tx_landed_total{path="jet_tpu"} 3200      # 70.8%
helios_tx_landed_total{path="jito_bundle"} 756    # 84.9%
helios_tx_landed_total{path="rpc_fallback"} 89    # 72.4%

# Latencia de landing (ms)
helios_tx_landing_latency_ms{path="jet_tpu", quantile="p50"} 420
helios_tx_landing_latency_ms{path="jito_bundle", quantile="p50"} 800

# Shield blocks (leaders evitados por MEV)
helios_shield_blocked_total 342

# Jito profit tracking
helios_jito_profit_lamports_total 15800000
helios_jito_tips_paid_lamports_total 890000
```

---

## 6. Estructura del Proyecto

```
helios-ingest/
├── Cargo.toml                      # Workspace root
├── config/
│   ├── helios-ingest.toml          # Configuración principal
│   └── richat-config.json          # Config para Richat server (Fuente 4)
├── crates/
│   ├── spy-node/                   # Fuente 1: Turbine shreds (ya diseñado)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── gossip.rs
│   │       ├── shred_fetch.rs
│   │       ├── sigverify.rs
│   │       ├── fec_assembler.rs
│   │       └── deshredder.rs
│   ├── solanacdn-client/           # Fuente 2: SolanaCDN relay
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── pipe_connector.rs   # Conexión a la mesh de Pipe
│   │       ├── shred_receiver.rs   # Recepción de shreds via CDN
│   │       └── config.rs
│   ├── grpc-client/                # Fuente 3: Yellowstone gRPC directo
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── subscriber.rs       # SubscribeRequest + stream handler
│   │       ├── filters.rs          # Filtros por estrategia
│   │       ├── reconnect.rs        # Backoff exponencial
│   │       └── config.rs
│   ├── richat-client/              # Fuente 4: Richat gRPC/QUIC
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── grpc_transport.rs   # Conexión via gRPC al richat-server local
│   │       ├── quic_transport.rs   # Conexión via QUIC (menor latencia)
│   │       ├── subscriber.rs       # Reutiliza yellowstone-grpc-proto types
│   │       └── config.rs
│   ├── event-bus/                  # Bus unificado
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── dedup.rs            # LRU dedup por signature
│   │       ├── classifier.rs       # Clasificación por program ID → estrategia
│   │       ├── metrics.rs          # Prometheus metrics
│   │       └── types.rs            # IngestEvent, Source, EventType
│   ├── tx-sender/                  # Módulo 5: Envío de transacciones
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── jet_tpu.rs          # Path 1: Yellowstone Jet TPU Client (QUIC)
│   │       ├── jito_bundle.rs      # Path 2: Jito Bundle Engine
│   │       ├── rpc_fallback.rs     # Path 3: RPC sendTransaction
│   │       ├── confirmation.rs     # Confirmation tracker via gRPC
│   │       ├── shield.rs           # Yellowstone Shield blocklist
│   │       ├── types.rs            # SendRequest, TxPriority, PendingTx
│   │       ├── metrics.rs          # Prometheus metrics del sender
│   │       └── config.rs
│   └── common/                     # Tipos compartidos
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs
│           ├── program_ids.rs      # Constantes de program IDs
│           └── solana_types.rs     # Re-exports de tipos Solana
└── src/
    └── main.rs                     # Orquestador: spawn ingesta + tx-sender
```

---

## 7. Dependencias Principales (Cargo.toml workspace)

```toml
[workspace]
members = [
    "crates/spy-node",
    "crates/solanacdn-client",
    "crates/grpc-client",
    "crates/richat-client",
    "crates/event-bus",
    "crates/tx-sender",
    "crates/common",
]

[workspace.dependencies]
# Solana
solana-sdk = "2.1"
solana-gossip = "2.1"
solana-ledger = "2.1"
solana-streamer = "2.1"
solana-perf = "2.1"

# Yellowstone gRPC (compatible con Richat)
yellowstone-grpc-client = "2.1"
yellowstone-grpc-proto = "2.1"

# Async runtime
tokio = { version = "1", features = ["full"] }
futures = "0.3"

# gRPC
tonic = { version = "0.12", features = ["tls", "tls-roots"] }
prost = "0.13"

# QUIC (para Richat QUIC transport)
quinn = "0.11"

# Data structures
dashmap = "6"
lru = "0.12"
crossbeam-channel = "0.5"

# Serialization
serde = { version = "1", features = ["derive"] }
toml = "0.8"
serde_json = "1"

# Metrics
prometheus = "0.13"

# Logging
tracing = "0.1"
tracing-subscriber = "0.3"

# Crypto / FEC
rayon = "1.10"
reed-solomon-erasure = "6"

# Error handling
anyhow = "1"
thiserror = "2"

# TX Sender (Path 1: Jet TPU Client)
yellowstone-jet-tpu-client = { version = "0.1.0", features = ["yellowstone-grpc", "shield"] }
yellowstone-shield-store = "0.9.1"

# TX Sender (Path 2: Jito Bundles)
jito-sdk-rust = "0.3"

# Binary serialization
bincode = "1.3"
bs58 = "0.5"
```

---

## 8. Orquestador (main.rs)

```rust
// helios-ingest/src/main.rs — Pseudocódigo del orquestador principal

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Cargar configuración
    let config = Config::load("config/helios-ingest.toml")?;

    // 2. Crear canal del Event Bus
    let (bus_tx, bus_rx) = tokio::sync::mpsc::channel::<IngestEvent>(10_000);
    let (strategy_tx, strategy_rx) = tokio::sync::mpsc::channel::<StrategyEvent>(5_000);

    // 3. Spawn Event Bus processor
    let bus = EventBus::new(strategy_tx.clone());
    tokio::spawn(async move { bus.run(bus_rx).await });

    // 4. Spawn fuentes (cada una es un tokio task independiente)
    if config.spy_node.enabled {
        let tx = bus_tx.clone();
        tokio::spawn(async move {
            SpyNode::new(config.spy_node, tx).run_with_reconnect().await
        });
    }

    if config.cdn.enabled {
        let tx = bus_tx.clone();
        tokio::spawn(async move {
            CdnShredReceiver::new(config.cdn, tx).run_with_reconnect().await
        });
    }

    if config.grpc.enabled {
        let tx = bus_tx.clone();
        tokio::spawn(async move {
            GrpcSource::new(config.grpc, tx).run_with_reconnect().await
        });
    }

    if config.richat.enabled {
        let tx = bus_tx.clone();
        tokio::spawn(async move {
            RichatSource::new(config.richat, tx).run_with_reconnect().await
        });
    }

    // 5. Spawn TX Sender (consume de strategy engine, envía txs)
    let (send_tx, send_rx) = tokio::sync::mpsc::channel::<SendRequest>(1_000);
    if config.tx_sender.enabled {
        tokio::spawn(async move {
            let mut sender = TxSender::new(config.tx_sender, send_rx).await.unwrap();
            sender.run().await
        });
    }

    // 6. Spawn Strategy Engine (consume de strategy_rx, emite a send_tx)
    // → Definido en el Plan Maestro de Helios Gold V2
    // → strategy_rx (eventos del bus) → decisiones → send_tx (al TX Sender)

    // 7. Spawn Prometheus metrics server
    tokio::spawn(metrics::serve(config.general.metrics_bind));

    // 7. Esperar señal de shutdown
    tokio::signal::ctrl_c().await?;
    Ok(())
}
```

---

## 9. Comparativa de Fuentes — Resumen

| Aspecto | F1: Spy Node | F2: SolanaCDN | F3: Yellowstone gRPC | F4: Richat |
|---------|-------------|---------------|---------------------|-----------|
| **Tipo de dato** | Shreds crudos | Shreds acelerados | Txs parseadas | Txs parseadas (aggregadas) |
| **Latencia** | ~50-200μs (pre-confirm) | P50 ~78ms | Sub-50ms | Sub-50ms (QUIC: menor) |
| **Overhead** | 2-4 cores, 4-8GB | <1 core, 200-500MB | <1 core, ~100MB | 1-2 cores, 1-2GB (Modo A) |
| **Costo mensual** | $0 | $0 | $49-99+/mes | $0 extra sobre F3 (Modo A) |
| **Ventaja principal** | Info pre-confirmación | Redundancia shreds | Txs ya parseadas | Multiplexing + QUIC + dedup |
| **Punto débil** | Complejo de implementar | Depende de Pipe Network | Depende de proveedor | Componente adicional que operar |
| **Best for** | Whale backrun, pre-exec | Failover de F1 | Graduation snipe, pool state | Resiliencia multi-proveedor |
| **Implementación** | Ya diseñado (Plan Maestro) | Fork quirúrgico del repo | yellowstone-grpc-client crate | richat-server + yellowstone-grpc-proto |

---

## 10. Roadmap de Implementación

### Fase 1: Foundation (Semanas 1-2)
- [ ] Crear workspace helios-ingest con estructura de crates
- [ ] Implementar crate `common` (types, program IDs)
- [ ] Implementar crate `event-bus` (dedup, classifier, metrics)
- [ ] Implementar crate `grpc-client` (Fuente 3 — Yellowstone gRPC directo)
- [ ] Test end-to-end: gRPC → Event Bus → log de eventos filtrados

### Fase 2: Richat Aggregador (Semanas 3-4)
- [ ] Clonar, compilar y estudiar repo Richat
- [ ] Configurar richat-server como aggregador con 2 upstream sources
- [ ] Deploy richat-server como systemd service
- [ ] Implementar crate `richat-client` (Fuente 4 — gRPC y QUIC transports)
- [ ] Benchmark: Richat QUIC vs gRPC directo vs Yellowstone SaaS
- [ ] Si Richat aggregador funciona bien → desactivar Fuente 3 directa

### Fase 3: SolanaCDN (Semanas 5-6)
- [ ] Clonar y analizar repo SolanaCDN — mapear módulos CDN
- [ ] Implementar crate `solanacdn-client` (Fuente 2 — fork quirúrgico)
- [ ] Test: shreds del CDN llegan al Event Bus y se deduplican correctamente
- [ ] Métricas comparativas CDN vs Spy Node vs Richat

### Fase 4: Spy Node Integration (Semanas 7-8)
- [ ] Integrar crate `spy-node` (Fuente 1) del Plan Maestro existente
- [ ] Conectar al Event Bus con dedup cross-fuente
- [ ] 4 fuentes alimentando el bus simultáneamente
- [ ] Dashboard Prometheus/Grafana con latencias por fuente

### Fase 5: TX Sender (Semanas 9-10)
- [ ] Integrar `yellowstone-jet-tpu-client` en crate `tx-sender`
- [ ] Implementar Path 1 (Jet TPU QUIC) con gRPC leader tracking
- [ ] Implementar Path 2 (Jito Bundle) para arbitrajes atómicos
- [ ] Implementar Path 3 (RPC fallback)
- [ ] Confirmation Tracker con retry logic
- [ ] Configurar Yellowstone Shield blocklist

### Fase 6: Strategy Engine Connection (Semanas 11-12)
- [ ] Conectar Event Bus al Strategy Engine de Helios Gold V2
- [ ] Conectar Strategy Engine al TX Sender (ingesta → decisión → envío)
- [ ] Implementar classifier: programa ID → estrategia correspondiente
- [ ] Test con las 5 estrategias recibiendo eventos filtrados
- [ ] Alertas Telegram para eventos de alta prioridad

### Fase 7: Optimización (Semanas 13-14)
- [ ] Evaluar QUIC vs gRPC para Richat (benchmark latencia en producción)
- [ ] Benchmark landing rates por path del TX Sender
- [ ] Tuning de LRU cache sizes en dedup
- [ ] Evaluar ROI de Richat Modo B (self-hosted con nodo Agave)
- [ ] Documentar latencias reales y win rates por fuente y path

---

## 11. Configuración Completa de Ejemplo

```toml
# helios-ingest.toml — Configuración completa

[general]
log_level = "info"
metrics_bind = "127.0.0.1:9090"

# ============================================================
# FUENTE 1: Spy Node (Turbine Shreds)
# ============================================================
[spy_node]
enabled = true
gossip_entrypoints = [
    "entrypoint.mainnet-beta.solana.com:8001",
    "entrypoint2.mainnet-beta.solana.com:8001",
    "entrypoint3.mainnet-beta.solana.com:8001",
]
shred_listen_port = 8001
sigverify_threads = 2
fec_assembler_threads = 2

# ============================================================
# FUENTE 2: SolanaCDN (Pipe Network)
# ============================================================
[cdn]
enabled = true
pipe_endpoint = "auto"
local_tvu_port = 8003
reconnect_interval_ms = 5000
metrics_enabled = true

# ============================================================
# FUENTE 3: Yellowstone gRPC (SaaS directo)
# Desactivar si Richat aggregador está activo
# ============================================================
[grpc]
enabled = false
provider = "helius"
endpoint = "https://mainnet.helius-rpc.com:443"
token = "${HELIUS_API_KEY}"
commitment = "processed"
reconnect_backoff_ms = 1000
reconnect_max_ms = 30000

# ============================================================
# FUENTE 4: Richat (Aggregador gRPC/QUIC)
# Reemplaza Fuente 3 cuando hay múltiples proveedores
# ============================================================
[richat]
enabled = true
mode = "aggregator"
local_grpc_endpoint = "http://127.0.0.1:10000"
local_quic_endpoint = "127.0.0.1:10001"
protocol = "quic"
reconnect_backoff_ms = 1000
reconnect_max_ms = 15000

[[richat.upstream]]
provider = "helius"
endpoint = "https://mainnet.helius-rpc.com:443"
token = "${HELIUS_API_KEY}"

[[richat.upstream]]
provider = "triton"
endpoint = "https://your-triton-endpoint:443"
token = "${TRITON_TOKEN}"

# ============================================================
# FILTROS GLOBALES
# ============================================================
[filters]
programs = [
    "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",   # PumpSwap
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8",   # Raydium AMM V4
    "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",     # Orca Whirlpool
    "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",     # Meteora DLMM
    "FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH",    # Pyth Oracle
]

# ============================================================
# EVENT BUS
# ============================================================
[bus]
channel_capacity = 10000
dedup_cache_size = 100000
dedup_shred_cache_size = 50000
strategy_channel_capacity = 5000

# ============================================================
# STRATEGY ENGINE MAPPING
# ============================================================
[strategy]
graduation_programs = ["6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"]
arb_programs = [
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8",
    "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
    "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",
]
oracle_programs = ["FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH"]

# ============================================================
# TX SENDER (Módulo 5)
# ============================================================
[tx_sender]
enabled = true
identity_keypair_path = "/home/helios/.config/solana/id.json"

# Path 1: Jet TPU Client (QUIC directo a leaders)
[tx_sender.jet]
grpc_endpoint = "https://mainnet.helius-rpc.com:443"
grpc_token = "${HELIUS_API_KEY}"
rpc_endpoint = "https://mainnet.helius-rpc.com"
shield_enabled = true
shield_policy_address = ""

# Path 2: Jito Bundles (arbitrajes atómicos)
[tx_sender.jito]
block_engine_url = "https://mainnet.block-engine.jito.wtf"
default_tip_lamports = 10000

# Path 3: RPC Fallback
[tx_sender.rpc]
endpoint = "https://mainnet.helius-rpc.com"
max_retries = 3
retry_interval_ms = 500

# Confirmation Tracker
[tx_sender.confirmation]
timeout_ms = 30000
max_retries = 2
```

---

## 12. Requisitos de Hardware — Resumen por Configuración

| Configuración | CPU | RAM | Disco | Red | Costo/mes |
|--------------|-----|-----|-------|-----|-----------|
| **Mínimo viable** (F3 sola) | 2 cores | 4GB | 50GB | 100 Mbps | ~$49 (gRPC) + VPS |
| **Recomendado** (F1+F2+F4 Modo A) | 6 cores | 16GB | 100GB | 1 Gbps | ~$99-149 (gRPC×2) + VPS |
| **Óptimo** (F1+F2+F4 Modo A, AX102) | 16 cores | 128GB | 2TB NVMe | 1 Gbps | ~$149 (gRPC×2) + €104 (AX102) |
| **Full self-hosted** (F1+F2+F4 Modo B) | 24+ cores | 384GB+ | 4TB+ NVMe | 10 Gbps | €200+/mes + $0 gRPC |

---

## 13. Notas para Claude Code

### Orden de Implementación
1. `crates/common` y `crates/event-bus` — son la base
2. `crates/grpc-client` — el más straightforward, permite testear el bus
3. `crates/richat-client` — reutiliza tipos de grpc-client, apunta a richat-server local
4. `crates/tx-sender` — Path 1 (Jet TPU) primero, luego Path 2 (Jito), luego Path 3 (RPC)
5. `crates/solanacdn-client` — requiere análisis del repo SolanaCDN primero
6. `crates/spy-node` — el más complejo, ya tiene diseño del Plan Maestro

### Nota sobre versiones de Solana crates
El crate `tx-sender` usa `yellowstone-jet-tpu-client` que depende de solana v3.0 (Agave 2.3+).
Los crates de ingesta pueden usar solana v2.1. Para resolver el conflicto:
- Preferido: Mover todo a v3.0 cuando sea estable
- Alternativa: tx-sender como proceso separado comunicado via Unix socket o TCP con el bus
- Temporal: `[patch.crates-io]` con los patches de Triton (ver repo yellowstone-jet-tpu-client)

### Convenciones
- `anyhow::Result` para binario principal, `thiserror` para crates library
- Todos los canales: `tokio::sync::mpsc` con capacidad configurable
- Reconexión con backoff exponencial en todas las fuentes
- Métricas Prometheus en cada crate via `metrics.rs` local
- Config via archivos TOML parseados con `serde` + `toml`
- Logging con `tracing` (no `log`)
- Tests: cada crate tiene tests unitarios + integration tests con mocks

### Variables de Entorno
```bash
HELIUS_API_KEY=xxx          # Para Fuente 3 y/o Richat upstream
TRITON_TOKEN=xxx            # Para Richat upstream (si se usa Triton)
SHYFT_TOKEN=xxx             # Opcional: si se añade Shyft como upstream
```

### Systemd Services
```
helios-ingest.service    — Binario principal (ingesta + bus + strategy + tx-sender)
richat.service           — Richat server (solo si Fuente 4 Modo A está activo)
```

---

## 14. Referencias

### Ingesta (Fuentes 1-4)
- SolanaCDN: https://www.solanacdn.com / https://github.com/pipe-network/solanacdn
- Richat: https://github.com/lamports-dev/richat
- Richat Telegram: https://t.me/lamportsdev
- Yellowstone gRPC: https://github.com/rpcpool/yellowstone-grpc
- Yellowstone gRPC Proto: https://github.com/lamports-dev/yellowstone-grpc-proto
- Helius gRPC Docs: https://www.helius.dev/docs/grpc
- Triton Dragon's Mouth: https://docs.triton.one/project-yellowstone/dragons-mouth-grpc-subscriptions
- Shyft gRPC + RabbitStream: https://docs.shyft.to/solana-yellowstone-grpc/docs
- Chainstack Geyser: https://docs.chainstack.com/docs/yellowstone-grpc-geyser-plugin
- ERPC (usa Richat): https://erpc.global
- Jito ShredStream: https://github.com/jito-labs/shredstream-proxy
- Lamports Dev (Richat creators): https://github.com/lamports-dev

### TX Sender (Módulo 5)
- Yellowstone Jet TPU Client: https://blog.triton.one/introducing-yellowstone-jet-tpu-client-a-high-performance-solana-tpu-client-in-rust/
- Yellowstone Shield (MEV protection): https://blog.triton.one/protect-your-transactions-from-mev-with-yellowstone-shield-and-the-tpu-client/
- Agave TPU docs: https://docs.anza.xyz/validator/tpu
- Agave tpu-client-next (v2.3): https://www.helius.dev/blog/agave-v23-update--all-you-need-to-know
- SWQoS guide: https://solana.com/developers/guides/advanced/stake-weighted-qos
- SWQoS deep dive: https://www.helius.dev/blog/stake-weighted-quality-of-service-everything-you-need-to-know
- Jito Block Engine: https://www.jito.wtf
- Solana tx retry guide: https://solana.com/developers/guides/advanced/retry
- Solana sendTransaction RPC: https://solana.com/docs/rpc/http/sendtransaction
