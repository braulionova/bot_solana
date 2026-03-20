// main.rs – Helios Gold V2 Spy Node entry point.
//
// Pipeline:
//   UDP sockets (N threads)
//     → raw packets
//     → shred_parser (per-packet)
//     → sig_verifier (drop bad sigs)
//     → deduper (drop seen shreds)
//     → fec_assembler (group → entry batches)
//     → tx_decoder (entries → VersionedTransactions)
//     → signal_bus (SpySignal channel → market-engine)

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossbeam_channel::bounded;
use solana_sdk::signature::{read_keypair_file, Signer};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use spy_node::{
    config::Config,
    deduper::Deduper,
    fec_assembler::FecAssembler,
    gossip_client::GossipClient,
    jito_shredstream_client::spawn_jito_shredstream,
    shred_parser::parse_shred,
    sig_verifier::SigVerifier,
    signal_bus::{signal_channel, SpySignal},
    tx_decoder::{new_alt_cache, TxDecoder},
    udp_ingest::spawn_ingest_threads,
};

#[derive(Default)]
struct ProbeStats {
    parsed_total: AtomicU64,
    parsed_data: AtomicU64,
    parsed_code: AtomicU64,
    parse_errors: AtomicU64,
    dedup_pass: AtomicU64,
    data_complete: AtomicU64,
    last_in_slot: AtomicU64,
    blocks_emitted: AtomicU64,
    txs_decoded: AtomicU64,
}

fn main() -> Result<()> {
    // -----------------------------------------------------------------------
    // Tracing
    // -----------------------------------------------------------------------
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(true)
        .with_thread_names(true)
        .init();

    // -----------------------------------------------------------------------
    // Configuration
    // -----------------------------------------------------------------------
    let cfg = Config::parse();
    info!(config = ?cfg, "Helios Gold V2 spy-node starting");

    // -----------------------------------------------------------------------
    // Identity keypair
    // -----------------------------------------------------------------------
    let identity = read_keypair_file(&cfg.identity_keypair).unwrap_or_else(|e| {
        warn!(error = %e, "could not read identity keypair, generating ephemeral");
        solana_sdk::signature::Keypair::new()
    });
    info!(identity = %identity.pubkey(), "identity loaded");

    // -----------------------------------------------------------------------
    // Shared exit flag
    // -----------------------------------------------------------------------
    let exit = Arc::new(AtomicBool::new(false));

    // -----------------------------------------------------------------------
    // Shared state
    // -----------------------------------------------------------------------
    let alt_cache = new_alt_cache();
    let deduper = Arc::new(Deduper::new(cfg.dedup_cache_size));
    let probe_stats = Arc::new(ProbeStats::default());

    // Gossip / leader schedule client.
    let gossip = Arc::new(GossipClient::new(identity, &cfg).context("create gossip client")?);

    // Perform an initial leader-schedule fetch (best effort).
    if let Err(e) = gossip.refresh_leader_schedule() {
        warn!(error = %e, "initial leader schedule fetch failed; proceeding without");
    }

    // Sig verifier uses the gossip client's leader cache.
    let sig_verifier = Arc::new(SigVerifier::new(
        gossip.leader_for_slot(0), // will be updated per-shred
        cfg.sig_verify,
    ));

    // -----------------------------------------------------------------------
    // Channels
    // -----------------------------------------------------------------------
    let cap = cfg.channel_capacity;

    // Raw packet → shred parser
    // (raw channels come from spawn_ingest_threads directly)

    // Parsed (but not yet verified) shreds → sig verifier
    let (parsed_tx, parsed_rx) = bounded::<spy_node::shred_parser::ParsedShred>(cap);

    // Verified shreds → deduper / FEC assembler
    let (verified_tx, verified_rx) = bounded::<spy_node::shred_parser::ParsedShred>(cap);

    // Signal bus: FEC assembler / tx decoder → market engine
    let (signal_tx, signal_rx) = signal_channel(cap);

    // -----------------------------------------------------------------------
    // Spawn UDP ingest threads (TVU port)
    // -----------------------------------------------------------------------
    let (raw_rx_tvu, _tvu_handles) = spawn_ingest_threads(cfg.tvu_addr(), cfg.udp_threads, cap);

    // Spawn UDP ingest threads (Jito ShredStream port)
    let (raw_rx_jito, _jito_handles) = spawn_ingest_threads(
        cfg.jito_addr(),
        2, // fewer threads for secondary stream
        cap,
    );

    // -----------------------------------------------------------------------
    // Jito ShredStream gRPC registration
    // -----------------------------------------------------------------------
    if cfg.jito_shredstream_enabled {
        let dest_ip = cfg
            .jito_shredstream_dest_ip
            .clone()
            .or_else(|| cfg.gossip_host.clone())
            .context("JITO_SHREDSTREAM_DEST_IP (or GOSSIP_HOST) must be set when JITO_SHREDSTREAM_ENABLED=true")?;

        let regions: Vec<String> = cfg
            .jito_shredstream_regions
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        info!(
            url = %cfg.jito_block_engine_url,
            dest = %format!("{}:{}", dest_ip, cfg.jito_shred_port),
            ?regions,
            "launching Jito ShredStream client"
        );

        // Load the ShredStream auth keypair (whitelisted with Jito).
        // Falls back to identity_keypair if not configured.
        let kp_path = cfg
            .jito_shredstream_auth_keypair
            .as_deref()
            .unwrap_or(&cfg.identity_keypair);
        let kp = read_keypair_file(kp_path)
            .map_err(|e| anyhow::anyhow!("load shredstream keypair {}: {}", kp_path, e))?;

        spawn_jito_shredstream(
            Arc::new(kp),
            cfg.jito_block_engine_url.clone(),
            dest_ip,
            cfg.jito_shred_port,
            regions,
            exit.clone(),
        );
    } else {
        info!("Jito ShredStream disabled (set JITO_SHREDSTREAM_ENABLED=true to enable)");
    }

    // -----------------------------------------------------------------------
    // Spawn: shred parser thread (merges both raw streams)
    // -----------------------------------------------------------------------
    {
        let parsed_tx = parsed_tx.clone();
        let raw_rx_tvu = raw_rx_tvu;
        let probe_stats = probe_stats.clone();
        thread::Builder::new()
            .name("shred-parser-tvu".into())
            .spawn(move || {
                for pkt in raw_rx_tvu {
                    match parse_shred(pkt.data) {
                        Ok(shred) => {
                            probe_stats.parsed_total.fetch_add(1, Ordering::Relaxed);
                            match shred.shred_type {
                                spy_node::shred_parser::ShredType::Data => {
                                    probe_stats.parsed_data.fetch_add(1, Ordering::Relaxed);
                                }
                                spy_node::shred_parser::ShredType::Code => {
                                    probe_stats.parsed_code.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            let _ = parsed_tx.send(shred);
                        }
                        Err(e) => {
                            probe_stats.parse_errors.fetch_add(1, Ordering::Relaxed);
                            tracing::trace!(error = %e, "shred parse error");
                        }
                    }
                }
            })?;
    }
    {
        let parsed_tx = parsed_tx;
        let raw_rx_jito = raw_rx_jito;
        let probe_stats = probe_stats.clone();
        thread::Builder::new()
            .name("shred-parser-jito".into())
            .spawn(move || {
                for pkt in raw_rx_jito {
                    match parse_shred(pkt.data) {
                        Ok(shred) => {
                            probe_stats.parsed_total.fetch_add(1, Ordering::Relaxed);
                            match shred.shred_type {
                                spy_node::shred_parser::ShredType::Data => {
                                    probe_stats.parsed_data.fetch_add(1, Ordering::Relaxed);
                                }
                                spy_node::shred_parser::ShredType::Code => {
                                    probe_stats.parsed_code.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            let _ = parsed_tx.send(shred);
                        }
                        Err(e) => {
                            probe_stats.parse_errors.fetch_add(1, Ordering::Relaxed);
                            tracing::trace!(error = %e, "shred parse error (jito)");
                        }
                    }
                }
            })?;
    }

    // -----------------------------------------------------------------------
    // Spawn: sig verifier thread
    // -----------------------------------------------------------------------
    {
        let verifier = sig_verifier.clone();
        let parsed_rx = parsed_rx;
        let verified_tx = verified_tx.clone();
        thread::Builder::new()
            .name("sig-verifier".into())
            .spawn(move || {
                verifier.run_batch(parsed_rx, verified_tx);
            })?;
    }

    // -----------------------------------------------------------------------
    // Spawn: deduper + FEC assembler + TX decoder thread
    // -----------------------------------------------------------------------
    {
        let deduper = deduper.clone();
        let alt_cache = alt_cache.clone();
        let signal_tx = signal_tx.clone();
        let verified_rx = verified_rx;
        let probe_stats = probe_stats.clone();

        thread::Builder::new()
            .name("fec-assembler".into())
            .spawn(move || {
                let mut fec = FecAssembler::new();
                let decoder = TxDecoder::new(alt_cache);

                for shred in verified_rx {
                    // Dedup check.
                    if !deduper.accept(&shred) {
                        continue;
                    }
                    probe_stats.dedup_pass.fetch_add(1, Ordering::Relaxed);
                    if shred.data_complete {
                        probe_stats.data_complete.fetch_add(1, Ordering::Relaxed);
                    }
                    if shred.last_in_slot {
                        probe_stats.last_in_slot.fetch_add(1, Ordering::Relaxed);
                    }

                    let slot = shred.slot;

                    // Feed into FEC assembler.
                    if fec.ingest(&shred) {
                        // One or more FEC sets completed.
                        let complete = fec.drain_complete();
                        probe_stats
                            .blocks_emitted
                            .fetch_add(complete.len() as u64, Ordering::Relaxed);
                        for (slot, _fec_idx, payloads) in complete {
                            let txs = decoder.decode_batch(slot, &payloads);
                            probe_stats
                                .txs_decoded
                                .fetch_add(txs.len() as u64, Ordering::Relaxed);
                            for tx in txs {
                                let sig = SpySignal::NewTransaction {
                                    slot,
                                    tx,
                                    detected_at: std::time::Instant::now(),
                                };
                                if signal_tx.send(sig).is_err() {
                                    return;
                                }
                            }
                        }
                    }

                    // Prune old FEC state every 1000 shreds (approx).
                    if slot % 1000 == 0 {
                        fec.prune_old_slots(slot.saturating_sub(100));
                    }
                }
            })?;
    }

    {
        let probe_stats = probe_stats.clone();
        thread::Builder::new()
            .name("probe-stats".into())
            .spawn(move || {
                let mut prev_parsed_total = 0u64;
                let mut prev_parsed_data = 0u64;
                let mut prev_parsed_code = 0u64;
                let mut prev_parse_errors = 0u64;
                let mut prev_dedup_pass = 0u64;
                let mut prev_data_complete = 0u64;
                let mut prev_last_in_slot = 0u64;
                let mut prev_blocks_emitted = 0u64;
                let mut prev_txs_decoded = 0u64;

                loop {
                    thread::sleep(Duration::from_secs(10));
                    let parsed_total = probe_stats.parsed_total.load(Ordering::Relaxed);
                    let parsed_data = probe_stats.parsed_data.load(Ordering::Relaxed);
                    let parsed_code = probe_stats.parsed_code.load(Ordering::Relaxed);
                    let parse_errors = probe_stats.parse_errors.load(Ordering::Relaxed);
                    let dedup_pass = probe_stats.dedup_pass.load(Ordering::Relaxed);
                    let data_complete = probe_stats.data_complete.load(Ordering::Relaxed);
                    let last_in_slot = probe_stats.last_in_slot.load(Ordering::Relaxed);
                    let blocks_emitted = probe_stats.blocks_emitted.load(Ordering::Relaxed);
                    let txs_decoded = probe_stats.txs_decoded.load(Ordering::Relaxed);

                    info!(
                        parsed_total_delta = parsed_total - prev_parsed_total,
                        parsed_data_delta = parsed_data - prev_parsed_data,
                        parsed_code_delta = parsed_code - prev_parsed_code,
                        parse_errors_delta = parse_errors - prev_parse_errors,
                        dedup_pass_delta = dedup_pass - prev_dedup_pass,
                        data_complete_delta = data_complete - prev_data_complete,
                        last_in_slot_delta = last_in_slot - prev_last_in_slot,
                        blocks_emitted_delta = blocks_emitted - prev_blocks_emitted,
                        txs_decoded_delta = txs_decoded - prev_txs_decoded,
                        "probe stats"
                    );

                    prev_parsed_total = parsed_total;
                    prev_parsed_data = parsed_data;
                    prev_parsed_code = parsed_code;
                    prev_parse_errors = parse_errors;
                    prev_dedup_pass = dedup_pass;
                    prev_data_complete = data_complete;
                    prev_last_in_slot = last_in_slot;
                    prev_blocks_emitted = blocks_emitted;
                    prev_txs_decoded = txs_decoded;
                }
            })?;
    }

    // -----------------------------------------------------------------------
    // Spawn: gossip refresh loop
    // -----------------------------------------------------------------------
    {
        let gossip = gossip.clone();
        thread::Builder::new()
            .name("gossip-refresh".into())
            .spawn(move || {
                gossip.run_refresh_loop(Duration::from_secs(60));
            })?;
    }

    // -----------------------------------------------------------------------
    // Spawn: sig verifier leader updater (syncs gossip leader → verifier)
    // -----------------------------------------------------------------------
    {
        let gossip = gossip.clone();
        let verifier = sig_verifier.clone();
        thread::Builder::new()
            .name("leader-sync".into())
            .spawn(move || {
                let mut last_slot = 0u64;
                loop {
                    // Approximate current slot from wall clock (400 ms/slot).
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let approx_slot = now_ms / 400;
                    if approx_slot != last_slot {
                        let leader = gossip.leader_for_slot(approx_slot);
                        verifier.update_leader(leader);
                        last_slot = approx_slot;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            })?;
    }

    // -----------------------------------------------------------------------
    // Main thread: log signal stats
    // -----------------------------------------------------------------------
    info!("spy-node pipeline running – awaiting signals …");

    let mut signal_count: u64 = 0;
    let mut last_log = std::time::Instant::now();

    for signal in signal_rx {
        signal_count += 1;

        if last_log.elapsed() >= Duration::from_secs(10) {
            info!(signals_per_10s = signal_count, "signal bus stats");
            signal_count = 0;
            last_log = std::time::Instant::now();
        }

        match &signal {
            SpySignal::WhaleSwap {
                slot,
                pool,
                impact_pct,
                ..
            } => {
                info!(slot, %pool, impact_pct, "WHALE SWAP detected");
            }
            SpySignal::Graduation {
                slot,
                token_mint,
                pump_pool,
                source,
                ..
            } => {
                info!(slot, %token_mint, %pump_pool, source, "GRADUATION detected");
            }
            SpySignal::LiquidityEvent { slot, pool, .. } => {
                info!(slot, %pool, "LIQUIDITY EVENT detected");
            }
            SpySignal::NewTransaction { .. } => {
                // high-volume; only logged at trace level
            }
        }
    }

    error!("signal channel closed – shutting down");
    Ok(())
}
