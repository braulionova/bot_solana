//! geyser_local.rs — UDP receiver for the local Geyser plugin.
//!
//! Receives vault balance updates from helios-geyser-plugin via UDP on port 8855.
//! Each packet is 41 bytes: [pubkey 32B][amount 8B][side 1B].
//! Updates PoolStateCache.update_reserve_by_vault() at ~0ms latency.
//!
//! Env: GEYSER_LOCAL_PORT (default 8855)

use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info, warn};

use market_engine::pool_state::PoolStateCache;

const PACKET_SIZE: usize = 41;
const DEFAULT_PORT: u16 = 8855;

/// Spawn the local Geyser UDP receiver thread.
pub fn spawn_geyser_local(pool_cache: Arc<PoolStateCache>) {
    let port: u16 = std::env::var("GEYSER_LOCAL_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    thread::Builder::new()
        .name("geyser-local".into())
        .spawn(move || {
            let socket = match UdpSocket::bind(format!("127.0.0.1:{port}")) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, port, "geyser-local: failed to bind UDP socket");
                    return;
                }
            };
            // Non-blocking with 100ms timeout for stats logging.
            socket
                .set_read_timeout(Some(Duration::from_millis(100)))
                .ok();

            info!(port, "geyser-local: UDP receiver started");

            // Build vault index once, refresh periodically.
            let mut vault_index: HashMap<Pubkey, (Pubkey, bool)> = HashMap::new();
            let mut last_index_refresh = Instant::now();
            let mut updates_total = 0u64;
            let mut reserves_changed = 0u64;
            let mut last_log = Instant::now();
            let mut buf = [0u8; PACKET_SIZE + 64]; // extra room

            loop {
                // Refresh vault index every 60s.
                if last_index_refresh.elapsed() > Duration::from_secs(60) || vault_index.is_empty()
                {
                    vault_index = pool_cache.build_vault_index();
                    last_index_refresh = Instant::now();
                    if vault_index.is_empty() {
                        std::thread::sleep(Duration::from_secs(5));
                        continue;
                    }
                }

                match socket.recv(&mut buf) {
                    Ok(n) if n >= PACKET_SIZE => {
                        let pubkey_bytes: [u8; 32] =
                            buf[..32].try_into().unwrap_or([0; 32]);
                        let amount = u64::from_le_bytes(
                            buf[32..40].try_into().unwrap_or([0; 8]),
                        );
                        let vault_pubkey = Pubkey::from(pubkey_bytes);

                        // Look up pool + side from vault index.
                        if let Some(&(pool_address, is_vault_a)) =
                            vault_index.get(&vault_pubkey)
                        {
                            updates_total += 1;
                            let changed = pool_cache.update_reserve_by_vault(
                                &pool_address,
                                is_vault_a,
                                amount,
                            );
                            if changed {
                                reserves_changed += 1;
                                debug!(
                                    pool = %pool_address,
                                    side = if is_vault_a { "A" } else { "B" },
                                    amount,
                                    "geyser-local: reserve updated"
                                );
                            }
                        }
                    }
                    Ok(_) => {} // short packet, ignore
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(e) => {
                        warn!(error = %e, "geyser-local: recv error");
                    }
                }

                // Stats every 30s.
                if last_log.elapsed() > Duration::from_secs(30) {
                    let elapsed = last_log.elapsed().as_secs_f64().max(0.001);
                    if updates_total > 0 {
                        info!(
                            updates_total,
                            reserves_changed,
                            rate = format!("{:.1}/s", updates_total as f64 / elapsed),
                            "geyser-local: throughput"
                        );
                    }
                    updates_total = 0;
                    reserves_changed = 0;
                    last_log = Instant::now();
                }
            }
        })
        .expect("spawn geyser-local");
}
