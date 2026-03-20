// shred_mirror — Dual Turbine Feed: mirrors UDP shreds from Agave TVU to helios.
//
// Uses AF_PACKET SOCK_RAW socket to sniff all UDP packets arriving at the Agave
// validator's TVU port. Forwards the UDP payload (raw shred bytes) to helios's
// TVU port. Zero impact on the Agave validator.
//
// Usage:
//   MIRROR_SRC_PORT=8802 MIRROR_DST=127.0.0.1:8002 shred_mirror
//
// Requires: CAP_NET_RAW or root.

use std::env;
use std::net::{SocketAddr, UdpSocket};

const ETH_HDR_LEN: usize = 14;

// BPF filter for: udp dst port <PORT>
// Offsets are for raw ethernet frames (SOCK_RAW delivers full frame).
fn build_bpf_filter(port: u16) -> Vec<libc::sock_filter> {
    let port_val = port as u32;
    vec![
        // (0) ldh [12] — EtherType
        libc::sock_filter { code: 0x28, jt: 0, jf: 0, k: 12 },
        // (1) jeq #0x0800 (IPv4) → continue, else reject
        libc::sock_filter { code: 0x15, jt: 0, jf: 5, k: 0x0800 },
        // (2) ldb [23] — IP protocol (eth=14 + ip_proto_offset=9 → 23)
        libc::sock_filter { code: 0x30, jt: 0, jf: 0, k: 23 },
        // (3) jeq #17 (UDP) → continue, else reject
        libc::sock_filter { code: 0x15, jt: 0, jf: 3, k: 17 },
        // (4) ldh [36] — UDP dst port (eth=14 + ip=20 + udp_dst_offset=2 → 36)
        libc::sock_filter { code: 0x28, jt: 0, jf: 0, k: 36 },
        // (5) jeq #port → accept, else reject
        libc::sock_filter { code: 0x15, jt: 0, jf: 1, k: port_val },
        // (6) ret #65535 — accept full packet
        libc::sock_filter { code: 0x06, jt: 0, jf: 0, k: 65535 },
        // (7) ret #0 — reject
        libc::sock_filter { code: 0x06, jt: 0, jf: 0, k: 0 },
    ]
}

fn main() {
    let src_port: u16 = env::var("MIRROR_SRC_PORT")
        .unwrap_or_else(|_| "8802".to_string())
        .parse()
        .expect("MIRROR_SRC_PORT must be a valid port");

    let dst: SocketAddr = env::var("MIRROR_DST")
        .unwrap_or_else(|_| "127.0.0.1:8002".to_string())
        .parse()
        .expect("MIRROR_DST must be ip:port");

    let log_interval: u64 = env::var("MIRROR_LOG_INTERVAL_SECS")
        .unwrap_or_else(|_| "30".to_string())
        .parse()
        .unwrap_or(30);

    eprintln!(
        "[shred-mirror] src_port={} dst={} — capturing AF_PACKET SOCK_RAW, forwarding shreds",
        src_port, dst
    );

    // ── AF_PACKET SOCK_RAW — delivers full ethernet frames including header ──
    let raw_fd = unsafe {
        libc::socket(
            libc::AF_PACKET,
            libc::SOCK_RAW,
            (libc::ETH_P_IP as u16).to_be() as libc::c_int,
        )
    };
    if raw_fd < 0 {
        eprintln!(
            "[shred-mirror] FATAL: cannot open AF_PACKET socket (need root or CAP_NET_RAW): {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(1);
    }

    // ── Attach BPF filter ────────────────────────────────────────────────────
    let filter = build_bpf_filter(src_port);
    let bpf_prog = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr() as *mut libc::sock_filter,
    };
    let ret = unsafe {
        libc::setsockopt(
            raw_fd,
            libc::SOL_SOCKET,
            libc::SO_ATTACH_FILTER,
            &bpf_prog as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::sock_fprog>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        eprintln!(
            "[shred-mirror] FATAL: SO_ATTACH_FILTER failed: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(1);
    }

    // ── Output UDP socket ────────────────────────────────────────────────────
    let out_sock = UdpSocket::bind("0.0.0.0:0").expect("bind output socket");
    out_sock
        .connect(dst)
        .expect("connect output socket to dst");

    eprintln!("[shred-mirror] AF_PACKET SOCK_RAW socket open, BPF filter attached. Mirroring...");

    // ── Main loop: receive raw ethernet frames, extract UDP payload, forward ─
    // SOCK_RAW delivers: [14B eth hdr][20+B IP hdr][8B UDP hdr][payload]
    let mut buf = [0u8; 2048];
    let mut total_pkts: u64 = 0;
    let mut total_forwarded: u64 = 0;
    let mut total_errors: u64 = 0;
    let mut last_log = std::time::Instant::now();

    loop {
        let n = unsafe {
            libc::recvfrom(
                raw_fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if n <= 0 {
            continue;
        }
        let n = n as usize;
        total_pkts += 1;

        // Need at least: 14 (eth) + 20 (IP) + 8 (UDP) = 42 bytes
        if n < ETH_HDR_LEN + 28 {
            continue;
        }

        // Skip ethernet header — IP starts at offset 14
        let ip_start = ETH_HDR_LEN;
        let ip_version = (buf[ip_start] >> 4) & 0x0F;
        if ip_version != 4 {
            continue;
        }
        let ihl = ((buf[ip_start] & 0x0F) as usize) * 4;
        if ihl < 20 || ip_start + ihl + 8 > n {
            continue;
        }

        // UDP header starts after IP header
        let udp_hdr = ip_start + ihl;
        let udp_len = u16::from_be_bytes([buf[udp_hdr + 4], buf[udp_hdr + 5]]) as usize;
        let payload_start = udp_hdr + 8; // Skip 8-byte UDP header
        let payload_end = (udp_hdr + udp_len).min(n);

        if payload_start >= payload_end {
            continue;
        }

        let payload = &buf[payload_start..payload_end];

        // Forward to helios TVU port
        match out_sock.send(payload) {
            Ok(_) => total_forwarded += 1,
            Err(_) => total_errors += 1,
        }

        // Periodic stats
        if last_log.elapsed().as_secs() >= log_interval {
            eprintln!(
                "[shred-mirror] total={} forwarded={} errors={} rate={}/s",
                total_pkts,
                total_forwarded,
                total_errors,
                total_forwarded / log_interval.max(1),
            );
            total_pkts = 0;
            total_forwarded = 0;
            total_errors = 0;
            last_log = std::time::Instant::now();
        }
    }
}
