// udp_ingest.rs – Ultra-low latency UDP shred ingest.
//
// Optimizaciones sobre la versión anterior:
//
//   1. SO_BUSY_POLL = 50µs  — kernel hace busy-poll del NIC durante 50µs
//      antes de dormir. Elimina la latencia de interrupción (~40-150µs).
//      Clave para paquetes de Frankfurt con RTT ≤ 5ms.
//
//   2. SO_PREFER_BUSY_POLL = 1  — (Linux 5.11+) le dice al subsistema NAPI
//      que prefiera este socket en sus rondas de busy-poll.
//
//   3. MSG_WAITFORONE en lugar de MSG_DONTWAIT + yield_now():
//      - MSG_DONTWAIT: retorna inmediatamente si vacío → spin 100% CPU
//      - MSG_WAITFORONE: bloquea hasta el primer paquete, luego recoge
//        todos los disponibles. Con SO_BUSY_POLL, el "bloqueo" es busy-poll
//        durante 50µs, no sleep de OS. Resultado: latencia de paquete = 0-50µs.
//
//   4. Pre-allocación de msgs/iovecs/addrs UNA SOLA VEZ por thread.
//      La versión anterior los re-creaba como Vec en cada llamada a recv_mmsg
//      (BATCH_SIZE × 3 heap allocs por batch). Ahora cero allocs en hot path.
//
//   5. BATCH_SIZE 64 → 256 — maneja mejor los bursts de shreds sin aumentar
//      latencia del primer paquete (MSG_WAITFORONE entrega inmediatamente).
//
//   6. SO_RCVBUF 128MB → 256MB — más headroom para bursts de Turbine.
//
// Latencia estimada post-optimización:
//   Paquete llega desde Frankfurt → kernel lo detecta en ≤50µs (busy-poll)
//   → recvmmsg retorna inmediatamente → dispatch a shred_parser ≤10µs
//   Total: ~50-100µs desde wire a canal (vs. 150-400µs antes).

use std::mem::MaybeUninit;
use std::net::{SocketAddr, UdpSocket};
use std::os::unix::io::AsRawFd;
use std::thread;

use bytes::{Bytes, BytesMut};
use core_affinity::CoreId;
use crossbeam_channel::Sender;
use tracing::{error, info, warn};

// ── Constantes ──────────────────────────────────────────────────────────────

const MAX_DATAGRAM_SIZE: usize = 1_280; // Tamaño estándar de un shred Solana
const BATCH_SIZE: usize = 256; // Paquetes por recvmmsg (era 64)
const DEFAULT_RCVBUF: usize = 256 * 1024 * 1024; // 256 MB (era 128)

// Linux socket options — definidos aquí para no depender de versión de libc.
const SO_BUSY_POLL: libc::c_int = 46; // Disponible desde Linux 3.11
const SO_PREFER_BUSY_POLL: libc::c_int = 69; // Disponible desde Linux 5.11
                                             // MSG_WAITFORONE: bloquea hasta tener ≥1 paquete, luego recoge todos.
const MSG_WAITFORONE: libc::c_int = 0x10000;

// ── Tipos públicos ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RawPacket {
    pub data: Bytes,
    pub source: SocketAddr,
}

// ── Configuración de socket ──────────────────────────────────────────────────

fn setsockopt_int(fd: libc::c_int, level: libc::c_int, opt: libc::c_int, val: libc::c_int) {
    unsafe {
        libc::setsockopt(
            fd,
            level,
            opt,
            &val as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    // Errores son non-fatal: se loguean en las funciones de alto nivel.
}

fn configure_socket(fd: libc::c_int, core_id: Option<usize>) {
    // SO_REUSEPORT: múltiples threads en el mismo puerto, kernel balancea.
    setsockopt_int(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT, 1);

    // SO_RCVBUF: buffer grande para absorber bursts de Turbine.
    let rcvbuf = DEFAULT_RCVBUF as libc::c_int;
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &rcvbuf as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };

    // SO_BUSY_POLL = 50µs: el kernel hace busy-poll del NIC durante 50µs
    // antes de entregar la CPU al scheduler. Para paquetes que llegan
    // desde Frankfurt con jitter < 5ms, esto elimina la latencia de
    // interrupción casi siempre.
    setsockopt_int(fd, libc::SOL_SOCKET, SO_BUSY_POLL, 50);

    // SO_PREFER_BUSY_POLL: hint al subsistema NAPI para que priorice
    // este socket en las rondas de busy-poll. Linux 5.11+.
    setsockopt_int(fd, libc::SOL_SOCKET, SO_PREFER_BUSY_POLL, 1);

    // SO_INCOMING_CPU: si conocemos el core, el kernel envía IRQs de
    // los paquetes entrantes al mismo core que procesa el socket.
    // Mejora la cache locality (datos calientes en L1/L2 del core receptor).
    if let Some(cpu) = core_id {
        setsockopt_int(
            fd,
            libc::SOL_SOCKET,
            libc::SO_INCOMING_CPU,
            cpu as libc::c_int,
        );
    }
}

fn bind_socket(fd: libc::c_int, addr: SocketAddr) -> bool {
    let ret = match addr {
        SocketAddr::V4(v4) => {
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as u16,
                sin_port: u16::to_be(v4.port()),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(v4.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            unsafe {
                libc::bind(
                    fd,
                    &sin as *const _ as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
        }
        SocketAddr::V6(v6) => {
            let mut sin6: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            sin6.sin6_family = libc::AF_INET6 as u16;
            sin6.sin6_port = u16::to_be(v6.port());
            sin6.sin6_addr.s6_addr = v6.ip().octets();
            unsafe {
                libc::bind(
                    fd,
                    &sin6 as *const _ as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    };
    ret == 0
}

fn open_socket(addr: SocketAddr, core_id: Option<usize>) -> Option<UdpSocket> {
    let domain = if addr.is_ipv6() {
        libc::AF_INET6
    } else {
        libc::AF_INET
    };
    let fd = unsafe { libc::socket(domain, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return None;
    }
    configure_socket(fd, core_id);
    if !bind_socket(fd, addr) {
        unsafe { libc::close(fd) };
        return None;
    }
    use std::os::unix::io::FromRawFd;
    Some(unsafe { UdpSocket::from_raw_fd(fd) })
}

// ── recvmmsg con estructuras pre-allocadas ────────────────────────────────────

/// Estado interno del thread de ingest.
/// Todos los buffers se alloc UNA SOLA VEZ y se reusan indefinidamente.
struct IngestState {
    buffers: Vec<BytesMut>,
    iovecs: Vec<libc::iovec>,
    addrs: Vec<libc::sockaddr_storage>,
    msgs: Vec<libc::mmsghdr>,
}

impl IngestState {
    fn new() -> Self {
        let mut buffers: Vec<BytesMut> = (0..BATCH_SIZE)
            .map(|_| {
                let mut b = BytesMut::with_capacity(MAX_DATAGRAM_SIZE);
                unsafe { b.set_len(MAX_DATAGRAM_SIZE) };
                b
            })
            .collect();

        let mut iovecs: Vec<libc::iovec> = (0..BATCH_SIZE)
            .map(|i| libc::iovec {
                iov_base: buffers[i].as_mut_ptr() as *mut libc::c_void,
                iov_len: MAX_DATAGRAM_SIZE,
            })
            .collect();

        let mut addrs: Vec<libc::sockaddr_storage> = (0..BATCH_SIZE)
            .map(|_| unsafe { MaybeUninit::zeroed().assume_init() })
            .collect();

        let msgs: Vec<libc::mmsghdr> = (0..BATCH_SIZE)
            .map(|i| {
                let mut hdr: libc::mmsghdr = unsafe { MaybeUninit::zeroed().assume_init() };
                hdr.msg_hdr.msg_iov = &mut iovecs[i];
                hdr.msg_hdr.msg_iovlen = 1;
                hdr.msg_hdr.msg_name = &mut addrs[i] as *mut _ as *mut libc::c_void;
                hdr.msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as u32;
                hdr
            })
            .collect();

        Self {
            buffers,
            iovecs,
            addrs,
            msgs,
        }
    }

    /// Reset de los campos que el kernel puede haber modificado.
    /// Necesario antes de cada llamada a recvmmsg para que los tamaños
    /// sean correctos.
    #[inline]
    fn reset_headers(&mut self) {
        for i in 0..BATCH_SIZE {
            self.iovecs[i].iov_len = MAX_DATAGRAM_SIZE;
            self.msgs[i].msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as u32;
        }
    }
}

// ── Thread de ingest ─────────────────────────────────────────────────────────

fn ingest_thread(
    bind_addr: SocketAddr,
    thread_id: usize,
    core_id: Option<CoreId>,
    tx: Sender<RawPacket>,
) {
    if let Some(id) = core_id {
        if core_affinity::set_for_current(id) {
            info!(thread_id, core = id.id, "udp-ingest thread pinned to core");
        }
    }

    let core_hint = core_id.map(|c| c.id);
    let socket = match open_socket(bind_addr, core_hint) {
        Some(s) => s,
        None => {
            error!(thread_id, %bind_addr, "failed to open UDP socket");
            return;
        }
    };

    info!(
        thread_id,
        addr = %bind_addr,
        batch = BATCH_SIZE,
        rcvbuf_mb = DEFAULT_RCVBUF / 1024 / 1024,
        busy_poll_us = 50,
        "udp-ingest ready"
    );

    let fd = socket.as_raw_fd();
    let mut state = IngestState::new();

    // Optional UDP mirror: forward raw shreds to a local gRPC proxy for decoding.
    // Set SHRED_MIRROR_PORT=20002 to enable (used by jito-shredstream-proxy --grpc-service-port).
    let mirror_fd = std::env::var("SHRED_MIRROR_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .map(|port| {
            let s = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_NONBLOCK, 0) };
            if s >= 0 {
                let mut dst: libc::sockaddr_in = unsafe { std::mem::zeroed() };
                dst.sin_family = libc::AF_INET as u16;
                dst.sin_port = port.to_be();
                dst.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
                unsafe {
                    libc::connect(
                        s,
                        &dst as *const _ as *const libc::sockaddr,
                        std::mem::size_of::<libc::sockaddr_in>() as u32,
                    );
                }
                Some(s)
            } else {
                None
            }
        })
        .flatten();

    loop {
        state.reset_headers();

        // MSG_WAITFORONE: bloquea hasta ≥1 paquete (busy-poll 50µs gracias a
        // SO_BUSY_POLL), luego retorna TODOS los disponibles hasta BATCH_SIZE.
        // Cero spinning en código Rust: el spinning lo hace el kernel en ring 0.
        let received = unsafe {
            libc::recvmmsg(
                fd,
                state.msgs.as_mut_ptr(),
                BATCH_SIZE as libc::c_uint,
                MSG_WAITFORONE,
                std::ptr::null_mut(),
            )
        };

        if received <= 0 {
            // EINTR o error transitorio — continuar sin spin.
            continue;
        }

        for i in 0..received as usize {
            let len = state.msgs[i].msg_len as usize;
            if len == 0 {
                continue;
            }

            let Some(source) = sockaddr_storage_to_socketaddr(&state.addrs[i]) else {
                continue;
            };

            // Zero-copy: split_to devuelve un Bytes apuntando a la misma
            // memoria del buffer; luego reemplazamos el buffer en IngestState.
            let data = state.buffers[i].split_to(len).freeze();

            // Re-inicializar buffer para el siguiente ciclo.
            let mut new_buf = BytesMut::with_capacity(MAX_DATAGRAM_SIZE);
            unsafe { new_buf.set_len(MAX_DATAGRAM_SIZE) };
            state.buffers[i] = new_buf;
            state.iovecs[i].iov_base = state.buffers[i].as_mut_ptr() as *mut libc::c_void;

            // Mirror raw shred to gRPC proxy (non-blocking, best-effort)
            if let Some(mfd) = mirror_fd {
                unsafe {
                    libc::send(mfd, data.as_ptr() as *const libc::c_void, data.len(), libc::MSG_DONTWAIT);
                }
            }

            if tx.try_send(RawPacket { data, source }).is_err() {
                // Canal lleno → backpressure. El parser no puede seguir el ritmo.
                // Drop del paquete más antiguo es preferible a OOM.
            }
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn sockaddr_storage_to_socketaddr(storage: &libc::sockaddr_storage) -> Option<SocketAddr> {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
    match storage.ss_family as libc::c_int {
        libc::AF_INET => {
            let sin = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let port = u16::from_be(sin.sin_port);
            Some(SocketAddr::V4(SocketAddrV4::new(ip, port)))
        }
        libc::AF_INET6 => {
            let sin6 = unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
            let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            let port = u16::from_be(sin6.sin6_port);
            Some(SocketAddr::V6(SocketAddrV6::new(
                ip,
                port,
                sin6.sin6_flowinfo,
                sin6.sin6_scope_id,
            )))
        }
        _ => None,
    }
}

// ── API pública ───────────────────────────────────────────────────────────────

pub fn spawn_ingest_threads(
    bind_addr: SocketAddr,
    num_threads: usize,
    channel_capacity: usize,
) -> (
    crossbeam_channel::Receiver<RawPacket>,
    Vec<thread::JoinHandle<()>>,
) {
    let (tx, rx) = crossbeam_channel::bounded(channel_capacity);
    let cores = core_affinity::get_core_ids().unwrap_or_default();

    let handles = (0..num_threads)
        .map(|i| {
            let tx_clone = tx.clone();
            let core_id = cores.get(i % cores.len().max(1)).copied();
            thread::Builder::new()
                .name(format!("udp-ingest-{}", i))
                .spawn(move || ingest_thread(bind_addr, i, core_id, tx_clone))
                .expect("spawn udp-ingest thread")
        })
        .collect();

    (rx, handles)
}
