//! metrics.rs – Atomic counters for the spy-node pipeline.
//!
//! Exposes a minimal Prometheus-compatible text exposition on a TCP port.
//! No external crate dependency – uses `std::net::TcpListener` and
//! `std::sync::atomic::AtomicU64` only.
//!
//! Endpoint: GET http://0.0.0.0:9090/metrics

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Counters
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Metrics {
    // UDP layer
    pub udp_recv_total: AtomicU64,
    pub udp_bytes_total: AtomicU64,
    // Shred parser
    pub shreds_parsed: AtomicU64,
    pub shreds_dropped: AtomicU64,
    pub data_shreds: AtomicU64,
    pub code_shreds: AtomicU64,
    // Deduper
    pub dedup_hits: AtomicU64,
    // FEC
    pub fec_sets_complete: AtomicU64,
    pub fec_sets_partial: AtomicU64,
    pub rs_recoveries_ok: AtomicU64,
    pub rs_recoveries_err: AtomicU64,
    // TX decoder
    pub entries_decoded: AtomicU64,
    pub transactions_decoded: AtomicU64,
    // Signal bus
    pub signals_emitted: AtomicU64,
    pub whale_signals: AtomicU64,
    pub graduation_signals: AtomicU64,
    pub liquidity_signals: AtomicU64,
    // Latency histogram buckets (slot-detected-at delay, μs)
    pub latency_lt_1ms: AtomicU64,
    pub latency_lt_5ms: AtomicU64,
    pub latency_lt_10ms: AtomicU64,
    pub latency_gt_10ms: AtomicU64,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    // ── UDP ──────────────────────────────────────────────────────────────────

    #[inline]
    pub fn udp_recv(&self) {
        self.udp_recv_total.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn udp_bytes(&self, n: u64) {
        self.udp_bytes_total.fetch_add(n, Ordering::Relaxed);
    }

    // ── Shred parser ─────────────────────────────────────────────────────────

    #[inline]
    pub fn shred_parsed(&self) {
        self.shreds_parsed.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn shred_dropped(&self) {
        self.shreds_dropped.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn data_shred(&self) {
        self.data_shreds.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn code_shred(&self) {
        self.code_shreds.fetch_add(1, Ordering::Relaxed);
    }

    // ── Dedup ────────────────────────────────────────────────────────────────

    #[inline]
    pub fn dedup_hit(&self) {
        self.dedup_hits.fetch_add(1, Ordering::Relaxed);
    }

    // ── FEC ──────────────────────────────────────────────────────────────────

    #[inline]
    pub fn fec_complete(&self) {
        self.fec_sets_complete.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn fec_partial(&self) {
        self.fec_sets_partial.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn rs_ok(&self) {
        self.rs_recoveries_ok.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn rs_err(&self) {
        self.rs_recoveries_err.fetch_add(1, Ordering::Relaxed);
    }

    // ── Decoder ──────────────────────────────────────────────────────────────

    #[inline]
    pub fn entries(&self, n: u64) {
        self.entries_decoded.fetch_add(n, Ordering::Relaxed);
    }
    #[inline]
    pub fn transactions(&self, n: u64) {
        self.transactions_decoded.fetch_add(n, Ordering::Relaxed);
    }

    // ── Signals ──────────────────────────────────────────────────────────────

    #[inline]
    pub fn signal(&self) {
        self.signals_emitted.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn whale(&self) {
        self.whale_signals.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn graduation(&self) {
        self.graduation_signals.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn liquidity(&self) {
        self.liquidity_signals.fetch_add(1, Ordering::Relaxed);
    }

    // ── Latency ──────────────────────────────────────────────────────────────

    pub fn observe_latency_us(&self, us: u64) {
        match us {
            0..=999 => self.latency_lt_1ms.fetch_add(1, Ordering::Relaxed),
            1000..=4999 => self.latency_lt_5ms.fetch_add(1, Ordering::Relaxed),
            5000..=9999 => self.latency_lt_10ms.fetch_add(1, Ordering::Relaxed),
            _ => self.latency_gt_10ms.fetch_add(1, Ordering::Relaxed),
        };
    }

    // ── Derived KPIs ─────────────────────────────────────────────────────────

    pub fn fec_recovery_rate(&self) -> f64 {
        let ok = self.rs_recoveries_ok.load(Ordering::Relaxed);
        let err = self.rs_recoveries_err.load(Ordering::Relaxed);
        let total = ok + err;
        if total == 0 {
            100.0
        } else {
            ok as f64 / total as f64 * 100.0
        }
    }

    // ── Prometheus text format ────────────────────────────────────────────────

    pub fn to_prometheus_text(&self) -> String {
        let mut out = String::with_capacity(2048);

        macro_rules! gauge {
            ($name:expr, $help:expr, $val:expr) => {
                out.push_str(&format!(
                    "# HELP {} {}\n# TYPE {} gauge\n{} {}\n",
                    $name, $help, $name, $name, $val
                ));
            };
        }
        macro_rules! counter {
            ($name:expr, $help:expr, $val:expr) => {
                out.push_str(&format!(
                    "# HELP {} {}\n# TYPE {} counter\n{} {}\n",
                    $name, $help, $name, $name, $val
                ));
            };
        }

        counter!(
            "helios_udp_recv_total",
            "UDP datagrams received",
            self.udp_recv_total.load(Ordering::Relaxed)
        );
        counter!(
            "helios_udp_bytes_total",
            "UDP bytes received",
            self.udp_bytes_total.load(Ordering::Relaxed)
        );
        counter!(
            "helios_shreds_parsed_total",
            "Shreds successfully parsed",
            self.shreds_parsed.load(Ordering::Relaxed)
        );
        counter!(
            "helios_shreds_dropped_total",
            "Shreds dropped (bad header/too short)",
            self.shreds_dropped.load(Ordering::Relaxed)
        );
        counter!(
            "helios_data_shreds_total",
            "Data shreds received",
            self.data_shreds.load(Ordering::Relaxed)
        );
        counter!(
            "helios_code_shreds_total",
            "Code (parity) shreds received",
            self.code_shreds.load(Ordering::Relaxed)
        );
        counter!(
            "helios_dedup_hits_total",
            "Duplicate shreds discarded",
            self.dedup_hits.load(Ordering::Relaxed)
        );
        counter!(
            "helios_fec_complete_total",
            "FEC sets completed",
            self.fec_sets_complete.load(Ordering::Relaxed)
        );
        counter!(
            "helios_fec_partial_total",
            "FEC sets expired incomplete",
            self.fec_sets_partial.load(Ordering::Relaxed)
        );
        counter!(
            "helios_rs_ok_total",
            "Reed-Solomon recoveries succeeded",
            self.rs_recoveries_ok.load(Ordering::Relaxed)
        );
        counter!(
            "helios_rs_err_total",
            "Reed-Solomon recoveries failed",
            self.rs_recoveries_err.load(Ordering::Relaxed)
        );
        counter!(
            "helios_entries_total",
            "Entry batches decoded",
            self.entries_decoded.load(Ordering::Relaxed)
        );
        counter!(
            "helios_transactions_total",
            "Transactions decoded",
            self.transactions_decoded.load(Ordering::Relaxed)
        );
        counter!(
            "helios_signals_total",
            "SpySignals emitted to market-engine",
            self.signals_emitted.load(Ordering::Relaxed)
        );
        counter!(
            "helios_whale_signals_total",
            "Whale swap signals emitted",
            self.whale_signals.load(Ordering::Relaxed)
        );
        counter!(
            "helios_graduation_signals_total",
            "Graduation signals emitted",
            self.graduation_signals.load(Ordering::Relaxed)
        );
        counter!(
            "helios_liquidity_signals_total",
            "Liquidity event signals emitted",
            self.liquidity_signals.load(Ordering::Relaxed)
        );

        gauge!(
            "helios_fec_recovery_rate",
            "FEC recovery rate (%)",
            format!("{:.2}", self.fec_recovery_rate())
        );

        // Latency histogram
        out.push_str("# HELP helios_latency_us Shred-to-signal latency histogram\n");
        out.push_str("# TYPE helios_latency_us histogram\n");
        out.push_str(&format!(
            "helios_latency_us_bucket{{le=\"1000\"}} {}\n",
            self.latency_lt_1ms.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "helios_latency_us_bucket{{le=\"5000\"}} {}\n",
            self.latency_lt_5ms.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "helios_latency_us_bucket{{le=\"10000\"}} {}\n",
            self.latency_lt_10ms.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "helios_latency_us_bucket{{le=\"+Inf\"}} {}\n",
            self.latency_gt_10ms.load(Ordering::Relaxed)
        ));

        out
    }
}

// ---------------------------------------------------------------------------
// HTTP server
// ---------------------------------------------------------------------------

/// Serve Prometheus metrics at `addr` (e.g. "0.0.0.0:9090").
/// Spawns a background thread; returns immediately.
pub fn serve_metrics(metrics: Arc<Metrics>, addr: &str) {
    let addr = addr.to_string();
    thread::Builder::new()
        .name("metrics-http".into())
        .spawn(move || {
            let listener = match TcpListener::bind(&addr) {
                Ok(l) => {
                    info!(addr = %addr, "[metrics] HTTP server listening");
                    l
                }
                Err(e) => {
                    error!(error = %e, addr = %addr, "[metrics] failed to bind");
                    return;
                }
            };
            listener.set_nonblocking(false).ok();
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => handle_metrics_request(s, &metrics),
                    Err(e) => warn!(error = %e, "[metrics] accept error"),
                }
            }
        })
        .expect("spawn metrics thread");
}

fn handle_metrics_request(mut stream: TcpStream, metrics: &Arc<Metrics>) {
    // Read request line (we don't care about the path).
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }

    let body = metrics.to_prometheus_text();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    let _ = stream.write_all(response.as_bytes());
}

// ---------------------------------------------------------------------------
// Stats logger
// ---------------------------------------------------------------------------

/// Spawn a background thread that logs a summary every `interval`.
pub fn spawn_stats_logger(metrics: Arc<Metrics>, interval: Duration) {
    thread::Builder::new()
        .name("stats-logger".into())
        .spawn(move || {
            let mut last = Instant::now();
            let mut last_shreds = 0u64;
            let mut last_signals = 0u64;

            loop {
                thread::sleep(interval);

                let now_shreds = metrics.shreds_parsed.load(Ordering::Relaxed);
                let now_signals = metrics.signals_emitted.load(Ordering::Relaxed);
                let elapsed = last.elapsed().as_secs_f64();

                let shreds_per_s = (now_shreds - last_shreds) as f64 / elapsed;
                let signals_per_s = (now_signals - last_signals) as f64 / elapsed;
                let fec_rate = metrics.fec_recovery_rate();

                info!(
                    shreds_per_s = format!("{:.1}", shreds_per_s),
                    signals_per_s = format!("{:.2}", signals_per_s),
                    fec_rate_pct = format!("{:.1}", fec_rate),
                    total_shreds = now_shreds,
                    total_signals = now_signals,
                    "[helios] pipeline stats"
                );

                // KPI alert: FEC rate below 85 %
                if fec_rate < 85.0
                    && (metrics.rs_recoveries_ok.load(Ordering::Relaxed)
                        + metrics.rs_recoveries_err.load(Ordering::Relaxed))
                        > 10
                {
                    warn!(
                        fec_rate = format!("{:.1}", fec_rate),
                        "[helios] LOW FEC RECOVERY RATE – check UDP packet loss"
                    );
                }

                last = Instant::now();
                last_shreds = now_shreds;
                last_signals = now_signals;
            }
        })
        .expect("spawn stats logger");
}
