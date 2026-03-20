/// shred_latency_probe — Mide la latencia real del pipeline de shreds.
///
/// Métricas que produce:
///
///   1. VENTAJA SOBRE RPC (ms):
///      Cuántos ms antes de que getSlot(processed) avance al slot S
///      recibimos el primer shred de ese slot.
///      Valor positivo = llegamos ANTES de que RPC confirme.
///      Objetivo ≥ 400ms (debemos ver shreds 1 slot entero antes de RPC).
///
///   2. LATENCIA DESDE INICIO DE SLOT (ms):
///      Tiempo desde que "debería haber empezado" el slot (según reloj de
///      pared + timing esperado de 400ms/slot) hasta que recibimos el primer
///      shred. Refleja el delay de Turbine fanout + red.
///
///   3. LATENCIA DE PIPELINE (µs):
///      Tiempo desde recvmmsg() hasta shred completamente parseado.
///      Refleja el costo del software local.
///
///   4. DISTRIBUCIÓN DE PRIMER ÍNDICE:
///      Qué índice tiene el primer shred que recibimos de cada slot.
///      Índice 0 = estamos entre los primeros en la red (fanout layer 1/2).
///      Índice > 8 = llegamos tarde (fanout layer 3+).
///
///   5. COBERTURA FEC:
///      % de datos shreds recibidos por slot antes de que el siguiente slot
///      empiece. < 50% → probablemente necesitamos Reed-Solomon recovery.
///
/// Uso:
///   cargo build --release --bin shred_latency_probe
///   ./target/release/shred_latency_probe [opciones]
///
/// Opciones (variables de entorno o flags):
///   --port 8002        Puerto UDP a escuchar (default 8002)
///   --duration 60      Segundos de captura (default 60)
///   --rpc-url <url>    RPC para comparar con getSlot
///   --jito-port 20000  También mide el puerto de Jito ShredStream
use std::collections::HashMap;
use std::mem::MaybeUninit;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;

// ── Offsets de shred (Agave 2.x ShredCommonHeader, verificado 2026-03-10) ────
// Old: slot=75, index=71, fec=67 (Agave ≤1.18 format)
// New: slot=65, index=73, version=77, fec=79 (Agave 2.x, live mainnet)
const SHRED_VARIANT_OFFSET: usize = 64;
const SLOT_OFFSET: usize = 65;
const INDEX_OFFSET: usize = 73;
const MIN_SHRED_SIZE: usize = 84; // up to and including fec_set_index

// ── SO_BUSY_POLL / SO_PREFER_BUSY_POLL ─────────────────────────────────────
const SO_BUSY_POLL: libc::c_int = 46;
const SO_PREFER_BUSY_POLL: libc::c_int = 69;

const BATCH_SIZE: usize = 256;
const MAX_DATAGRAM: usize = 1_280;

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "shred_latency_probe")]
struct Args {
    #[arg(long, env = "SPY_TVU_PORT", default_value = "8002")]
    port: u16,

    #[arg(long, env = "SPY_JITO_PORT", default_value = "20000")]
    jito_port: u16,

    #[arg(long, default_value = "60")]
    duration: u64,

    #[arg(
        long,
        env = "RPC_URL",
        default_value = "https://api.mainnet-beta.solana.com"
    )]
    rpc_url: String,

    /// Imprimir log de cada slot nuevo
    #[arg(long, default_value = "false")]
    verbose: bool,
}

// ── Estructuras de datos ─────────────────────────────────────────────────────

#[derive(Debug)]
struct SlotRecord {
    first_shred_recv: Instant,       // cuándo recibimos el 1er shred
    first_shred_recv_us: u64,        // µs desde inicio del probe
    first_index: u32,                // índice del 1er shred
    data_count: u32,                 // shreds de datos recibidos
    code_count: u32,                 // shreds de código recibidos
    rpc_advance_at: Option<Instant>, // cuándo RPC avanzó a este slot
}

impl SlotRecord {
    fn new(recv: Instant, recv_us: u64, index: u32, is_data: bool) -> Self {
        Self {
            first_shred_recv: recv,
            first_shred_recv_us: recv_us,
            first_index: index,
            data_count: if is_data { 1 } else { 0 },
            code_count: if is_data { 0 } else { 1 },
            rpc_advance_at: None,
        }
    }
}

#[derive(Default)]
struct PipelineStats {
    parse_latencies_us: Vec<u64>, // µs UDP recv → shred parsed
    advantage_ms: Vec<i64>,       // ms antes de que RPC avance (positivo = antes)
    slot_latency_ms: Vec<u64>,    // ms desde inicio esperado del slot
    first_indices: Vec<u32>,      // índice del primer shred por slot
    packets_total: u64,
    shreds_valid: u64,
    shreds_invalid: u64,
    slots_seen: u64,
}

fn quantile(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn quantile_u(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ── Socket helpers ────────────────────────────────────────────────────────────

fn open_probe_socket(port: u16) -> Option<UdpSocket> {
    let domain = libc::AF_INET;
    let fd = unsafe { libc::socket(domain, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return None;
    }

    let one: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEPORT,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        // Busy poll para baja latencia
        let bp: libc::c_int = 50;
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            SO_BUSY_POLL,
            &bp as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            SO_PREFER_BUSY_POLL,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        // Buffer de recepción
        let rcvbuf: libc::c_int = 64 * 1024 * 1024; // 64 MB para el probe
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &rcvbuf as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }

    let addr = libc::sockaddr_in {
        sin_family: libc::AF_INET as u16,
        sin_port: u16::to_be(port),
        sin_addr: libc::in_addr { s_addr: 0 }, // 0.0.0.0
        sin_zero: [0; 8],
    };
    let ret = unsafe {
        libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if ret != 0 {
        unsafe {
            libc::close(fd);
        }
        eprintln!(
            "  bind() failed on port {}: {}",
            port,
            std::io::Error::last_os_error()
        );
        return None;
    }
    Some(unsafe { UdpSocket::from_raw_fd(fd) })
}

// ── Parser inline del shred ───────────────────────────────────────────────────

#[inline(always)]
fn parse_shred_header(data: &[u8]) -> Option<(u64 /*slot*/, u32 /*index*/, bool /*is_data*/)> {
    if data.len() < MIN_SHRED_SIZE {
        return None;
    }
    let variant = data[SHRED_VARIANT_OFFSET];
    // Agave 2.x: 0xa5=LegacyData, 0x5a=LegacyCode; Merkle: bit7=1→Data, bit7=0→Code
    let is_data = match variant {
        0xa5 => true,
        0x5a => false,
        _ => variant >> 7 == 1,
    };
    // Sanity: exclude gossip/repair packets that accidentally pass the size check
    if variant == 0 || (variant != 0xa5 && variant != 0x5a && variant >> 4 == 0) {
        // variant=0 or upper nibble=0 with non-legacy = unlikely shred
        return None;
    }
    let slot = u64::from_le_bytes(data[SLOT_OFFSET..SLOT_OFFSET + 8].try_into().ok()?);
    let index = u32::from_le_bytes(data[INDEX_OFFSET..INDEX_OFFSET + 4].try_into().ok()?);
    // Sanity: slot should be in realistic range for mainnet (> 1M, < 2T)
    if slot < 1_000_000 || slot > 2_000_000_000_000 {
        return None;
    }
    Some((slot, index, is_data))
}

// ── Función de captura en un puerto ──────────────────────────────────────────

fn capture_loop(
    port: u16,
    start: Instant,
    end_us: u64,
    shared: Arc<Mutex<HashMap<u64, SlotRecord>>>,
    stats: Arc<Mutex<PipelineStats>>,
    verbose: bool,
    port_label: &'static str,
) {
    let Some(socket) = open_probe_socket(port) else {
        eprintln!("[{port_label}] ERROR: no se pudo abrir puerto {port} (¿ya está en uso sin SO_REUSEPORT?)");
        return;
    };
    println!("[{port_label}] Capturando shreds en puerto {port} …");

    let fd = socket.as_raw_fd();

    // Pre-alloc de buffers y estructuras para recvmmsg
    let mut bufs: Vec<[u8; MAX_DATAGRAM]> = vec![[0u8; MAX_DATAGRAM]; BATCH_SIZE];
    let mut iovecs: Vec<libc::iovec> = bufs
        .iter_mut()
        .map(|b| libc::iovec {
            iov_base: b.as_mut_ptr() as *mut _,
            iov_len: MAX_DATAGRAM,
        })
        .collect();
    let mut addrs: Vec<libc::sockaddr_storage> = (0..BATCH_SIZE)
        .map(|_| unsafe { MaybeUninit::zeroed().assume_init() })
        .collect();
    let mut msgs: Vec<libc::mmsghdr> = (0..BATCH_SIZE)
        .map(|i| {
            let mut h: libc::mmsghdr = unsafe { MaybeUninit::zeroed().assume_init() };
            h.msg_hdr.msg_iov = &mut iovecs[i];
            h.msg_hdr.msg_iovlen = 1;
            h.msg_hdr.msg_name = &mut addrs[i] as *mut _ as *mut _;
            h.msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as u32;
            h
        })
        .collect();

    loop {
        let now_us = start.elapsed().as_micros() as u64;
        if now_us >= end_us {
            break;
        }

        // Reset de campos que el kernel modifica
        for i in 0..BATCH_SIZE {
            iovecs[i].iov_len = MAX_DATAGRAM;
            msgs[i].msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as u32;
        }

        let recv_start = Instant::now();
        let n = unsafe {
            libc::recvmmsg(
                fd,
                msgs.as_mut_ptr(),
                BATCH_SIZE as libc::c_uint,
                0x10000, /* MSG_WAITFORONE */
                std::ptr::null_mut(),
            )
        };
        if n <= 0 {
            continue;
        }

        let recv_done = Instant::now();
        let batch_recv_us = recv_done.duration_since(recv_start).as_micros() as u64;

        let mut local_stats_packets = 0u64;
        let mut local_stats_valid = 0u64;
        let mut local_stats_invalid = 0u64;

        for i in 0..n as usize {
            let len = msgs[i].msg_len as usize;
            if len == 0 {
                continue;
            }

            local_stats_packets += 1;
            let data = &bufs[i][..len];

            let parse_start = Instant::now();
            let Some((slot, index, is_data)) = parse_shred_header(data) else {
                local_stats_invalid += 1;
                continue;
            };
            let parse_us = parse_start.elapsed().as_micros() as u64;
            local_stats_valid += 1;

            let now_us_pkt = start.elapsed().as_micros() as u64;

            {
                let mut map = shared.lock().unwrap();
                let entry = map.entry(slot).or_insert_with(|| {
                    if verbose {
                        let elapsed_ms = now_us_pkt / 1000;
                        println!(
                            "[{port_label}] +{:5}ms  nuevo slot {:>12}  idx={:>3}  {}",
                            elapsed_ms,
                            slot,
                            index,
                            if is_data { "DATA" } else { "CODE" }
                        );
                    }
                    SlotRecord::new(recv_done, now_us_pkt, index, is_data)
                });
                if is_data {
                    entry.data_count += 1;
                } else {
                    entry.code_count += 1;
                }
            }

            {
                let mut st = stats.lock().unwrap();
                st.parse_latencies_us.push(parse_us);
            }
        }

        {
            let mut st = stats.lock().unwrap();
            st.packets_total += local_stats_packets;
            st.shreds_valid += local_stats_valid;
            st.shreds_invalid += local_stats_invalid;
        }

        let _ = batch_recv_us; // suprime warning
    }

    println!("[{port_label}] Captura finalizada en puerto {port}");
}

// ── RPC polling thread ─────────────────────────────────────────────────────

fn rpc_poll_thread(
    rpc_url: String,
    shared: Arc<Mutex<HashMap<u64, SlotRecord>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::processed());
    let mut last_slot = 0u64;

    while !stop.load(Ordering::Relaxed) {
        if let Ok(slot) = rpc.get_slot() {
            if slot > last_slot {
                let now = Instant::now();
                // Marcar todos los slots desde last_slot+1 hasta slot
                let mut map = shared.lock().unwrap();
                for s in (last_slot + 1)..=slot {
                    if let Some(rec) = map.get_mut(&s) {
                        if rec.rpc_advance_at.is_none() {
                            rec.rpc_advance_at = Some(now);
                        }
                    }
                }
                last_slot = slot;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();
    let duration_us = args.duration * 1_000_000;

    println!();
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║     Helios Gold V2 — Shred Latency Probe                 ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    println!();
    println!("  Puerto TVU:          {}", args.port);
    println!("  Puerto Jito:         {}", args.jito_port);
    println!("  Duración:            {} segundos", args.duration);
    println!(
        "  RPC:                 {}",
        &args.rpc_url[..args.rpc_url.find('?').unwrap_or(args.rpc_url.len())]
    );
    println!();

    let start = Instant::now();
    let shared: Arc<Mutex<HashMap<u64, SlotRecord>>> = Arc::new(Mutex::new(HashMap::new()));
    let stats_tvu = Arc::new(Mutex::new(PipelineStats::default()));
    let stats_jito = Arc::new(Mutex::new(PipelineStats::default()));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // RPC polling thread
    {
        let shared2 = shared.clone();
        let stop2 = stop.clone();
        let rpc = args.rpc_url.clone();
        thread::Builder::new()
            .name("rpc-poll".into())
            .spawn(move || rpc_poll_thread(rpc, shared2, stop2))
            .unwrap();
    }

    // Capture thread TVU
    {
        let sh = shared.clone();
        let st = stats_tvu.clone();
        let verbose = args.verbose;
        let port = args.port;
        thread::Builder::new()
            .name("capture-tvu".into())
            .spawn(move || capture_loop(port, start, duration_us, sh, st, verbose, "TVU"))
            .unwrap();
    }

    // Capture thread Jito
    {
        let sh = shared.clone();
        let st = stats_jito.clone();
        let verbose = args.verbose;
        let port = args.jito_port;
        thread::Builder::new()
            .name("capture-jito".into())
            .spawn(move || capture_loop(port, start, duration_us, sh, st, verbose, "Jito"))
            .unwrap();
    }

    // Progress bar
    let dur = Duration::from_secs(args.duration);
    let bar_width = 50usize;
    loop {
        let elapsed = start.elapsed();
        if elapsed >= dur {
            break;
        }
        let pct = elapsed.as_secs_f64() / dur.as_secs_f64();
        let filled = (pct * bar_width as f64) as usize;
        let slots_seen = shared.lock().unwrap().len();
        let pkts =
            stats_tvu.lock().unwrap().packets_total + stats_jito.lock().unwrap().packets_total;
        print!(
            "\r  [{:>width$}{}] {:3}%  {:>5}s  {:>8} pkts  {:>6} slots",
            "█".repeat(filled),
            "░".repeat(bar_width - filled),
            (pct * 100.0) as u32,
            elapsed.as_secs(),
            pkts,
            slots_seen,
            width = filled,
        );
        use std::io::Write;
        std::io::stdout().flush().ok();
        thread::sleep(Duration::from_millis(500));
    }
    println!("\r  [{}] 100%  done{:40}", "█".repeat(bar_width), "");

    stop.store(true, Ordering::Relaxed);
    thread::sleep(Duration::from_millis(500)); // dar tiempo a RPC de anotar slots

    // ── Análisis de resultados ────────────────────────────────────────────────

    let map = shared.lock().unwrap();
    let mut advantages_ms: Vec<i64> = Vec::new();
    let mut slot_latencies_ms: Vec<u64> = Vec::new();
    let mut first_indices: Vec<u32> = Vec::new();
    let mut rpc_covered = 0usize;
    let mut ahead_of_rpc = 0usize;
    let mut behind_rpc = 0usize;

    for (_slot, rec) in map.iter() {
        first_indices.push(rec.first_index);

        if let Some(rpc_at) = rec.rpc_advance_at {
            rpc_covered += 1;
            let first_shred_at = rec.first_shred_recv;
            // advantage = tiempo que RPC tardó DESPUÉS de que nosotros ya teníamos el shred
            // (positivo = llegamos antes que RPC)
            let adv_ms = rpc_at.duration_since(first_shred_at).as_millis() as i64;
            // Si el shred llegó DESPUÉS de RPC, signed_duration_since sería negativo
            let adv = if rpc_at >= first_shred_at {
                adv_ms // positivo: fuimos primero
            } else {
                -(first_shred_at.duration_since(rpc_at).as_millis() as i64) // negativo: llegamos tarde
            };
            advantages_ms.push(adv);
            if adv >= 0 {
                ahead_of_rpc += 1;
            } else {
                behind_rpc += 1;
            }
        }
    }

    let total_slots = map.len();

    let st_tvu = stats_tvu.lock().unwrap();
    let st_jito = stats_jito.lock().unwrap();
    let total_pkts = st_tvu.packets_total + st_jito.packets_total;
    let total_valid = st_tvu.shreds_valid + st_jito.shreds_valid;

    // Ordenar para percentiles
    advantages_ms.sort_unstable();
    first_indices.sort_unstable();
    let mut parse_lats: Vec<u64> = st_tvu
        .parse_latencies_us
        .iter()
        .chain(st_jito.parse_latencies_us.iter())
        .copied()
        .collect();
    parse_lats.sort_unstable();

    let pkt_rate = total_pkts as f64 / args.duration as f64;
    let slot_rate = total_slots as f64 / args.duration as f64;

    println!();
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!(
        "  RESULTADOS FINALES  ({} segundos de captura)",
        args.duration
    );
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
    println!("  THROUGHPUT");
    println!(
        "    Paquetes totales:   {:>10}  ({:.0}/s)",
        total_pkts, pkt_rate
    );
    println!(
        "    Shreds válidos:     {:>10}  ({:.1}%)",
        total_valid,
        if total_pkts > 0 {
            total_valid as f64 / total_pkts as f64 * 100.0
        } else {
            0.0
        }
    );
    println!(
        "    Slots únicos:       {:>10}  ({:.1}/s, esperado ~2.5/s)",
        total_slots, slot_rate
    );
    println!();

    if !advantages_ms.is_empty() {
        println!("  VENTAJA SOBRE RPC (ms, positivo = llegamos antes que RPC)");
        println!("    Slots con datos RPC:  {}/{}", rpc_covered, total_slots);
        println!(
            "    Llegamos ANTES:       {} slots ({:.0}%)",
            ahead_of_rpc,
            ahead_of_rpc as f64 / rpc_covered as f64 * 100.0
        );
        println!(
            "    Llegamos DESPUÉS:     {} slots ({:.0}%)",
            behind_rpc,
            behind_rpc as f64 / rpc_covered as f64 * 100.0
        );
        println!("    p50:  {:>+7} ms", quantile(&advantages_ms, 0.50));
        println!("    p75:  {:>+7} ms", quantile(&advantages_ms, 0.75));
        println!("    p95:  {:>+7} ms", quantile(&advantages_ms, 0.95));
        println!("    p99:  {:>+7} ms", quantile(&advantages_ms, 0.99));
        println!(
            "    max:  {:>+7} ms",
            advantages_ms.last().copied().unwrap_or(0)
        );
        println!();
        println!("  INTERPRETACIÓN:");
        let p50 = quantile(&advantages_ms, 0.50);
        if p50 > 300 {
            println!("    ✅ EXCELENTE: llegamos {p50}ms antes que RPC (Turbine layer 1-2)");
        } else if p50 > 50 {
            println!("    ✅ BUENO: llegamos {p50}ms antes que RPC (Turbine layer 2-3)");
        } else if p50 > 0 {
            println!("    ⚠️  MARGINAL: solo {p50}ms de ventaja (Turbine leaf/late)");
        } else {
            println!(
                "    ❌ TARDE: RPC nos gana por {}ms (no estamos en el árbol Turbine)",
                -p50
            );
        }
    } else {
        println!("  VENTAJA SOBRE RPC: sin datos suficientes (RPC no respondió o 0 slots)");
    }
    println!();

    if !first_indices.is_empty() {
        println!("  ÍNDICE DEL PRIMER SHRED RECIBIDO POR SLOT");
        println!("    (índice 0 = primero en el FEC set = más temprano en la red)");
        let p50i = quantile_u(
            &first_indices.iter().map(|x| *x as u64).collect::<Vec<_>>(),
            0.50,
        );
        let p95i = quantile_u(
            &first_indices.iter().map(|x| *x as u64).collect::<Vec<_>>(),
            0.95,
        );
        println!(
            "    p50:  índice {:>3}  {}",
            p50i,
            if p50i == 0 {
                "← muy buen acceso"
            } else if p50i < 4 {
                "← buen acceso"
            } else {
                "← acceso tardío"
            }
        );
        println!("    p95:  índice {:>3}", p95i);
        // Distribución de índices 0-15
        let idx_dist: HashMap<u32, u32> = first_indices.iter().fold(HashMap::new(), |mut m, &i| {
            *m.entry(i.min(16)).or_insert(0) += 1;
            m
        });
        let mut buckets: Vec<(u32, u32)> = idx_dist.into_iter().collect();
        buckets.sort_by_key(|(k, _)| *k);
        print!("    Distribución:  ");
        for (idx, count) in buckets.iter().take(12) {
            print!("[{idx}]={count} ");
        }
        println!();
    }
    println!();

    if !parse_lats.is_empty() {
        println!("  LATENCIA DE PIPELINE (µs, recv → shred parseado)");
        println!("    p50:  {:>5} µs", quantile_u(&parse_lats, 0.50));
        println!("    p95:  {:>5} µs", quantile_u(&parse_lats, 0.95));
        println!("    p99:  {:>5} µs", quantile_u(&parse_lats, 0.99));
        println!(
            "    max:  {:>5} µs",
            parse_lats.last().copied().unwrap_or(0)
        );
    }
    println!();
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
}
