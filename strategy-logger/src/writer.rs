//! Background PG writer — drains LogEvent channel, batches inserts.

use crate::types::*;
use crossbeam_channel::Receiver;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_postgres::{Client, NoTls};
use tracing::{error, info, warn};

/// Main writer loop. Runs on a dedicated tokio current_thread runtime.
pub async fn run_writer(rx: Receiver<LogEvent>, db_url: String, dropped: Arc<AtomicU64>) {
    let client = connect_with_retry(&db_url).await;
    info!("pg-logger: connected to PostgreSQL");

    // Prepare statements for maximum insert speed
    let stmt_opp = client
        .prepare(
            "INSERT INTO opportunities (opp_id, slot, detected_at, signal_type, source_strategy, \
             token_mints, borrow_amount, flash_provider, gross_profit, net_profit, score, \
             risk_factor, n_hops, fast_arb) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) \
             RETURNING id",
        )
        .await
        .expect("prepare stmt_opp");

    let stmt_hop = client
        .prepare(
            "INSERT INTO opportunity_hops (opp_db_id, hop_index, pool, dex_type, token_in, \
             token_out, amount_in, amount_out, price_impact, pool_reserve_a, pool_reserve_b, \
             reserve_age_ms) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
        )
        .await
        .expect("prepare stmt_hop");

    let stmt_exec = client
        .prepare(
            "INSERT INTO executions (opp_db_id, tx_signature, send_channels, fast_arb, \
             detect_to_build_us, build_to_send_us, total_us, jito_tip, tpu_leaders) \
             VALUES ((SELECT id FROM opportunities WHERE opp_id=$1 ORDER BY id DESC LIMIT 1),\
             $2,$3,$4,$5,$6,$7,$8,$9)",
        )
        .await
        .expect("prepare stmt_exec");

    let stmt_exec_update = client
        .prepare(
            "UPDATE executions SET landed=$2, landed_slot=$3, actual_profit=$4, error_message=$5 \
             WHERE tx_signature=$1",
        )
        .await
        .expect("prepare stmt_exec_update");

    let stmt_sniper = client
        .prepare(
            "INSERT INTO sniper_events (slot, token_mint, pool, source, safety_passed, \
             safety_reason, pool_sol_reserve, snipe_lamports, tx_signature, build_ms, send_ms) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
        )
        .await
        .expect("prepare stmt_sniper");

    let stmt_sniper_sell = client
        .prepare(
            "UPDATE sniper_events SET sell_triggered=$2, sell_reason=$3, sell_tx_sig=$4, \
             sell_sol_received=$5, hold_duration_ms=$6, pnl_lamports=$7 \
             WHERE id = (SELECT id FROM sniper_events WHERE token_mint=$1 ORDER BY created_at DESC LIMIT 1)",
        )
        .await
        .expect("prepare stmt_sniper_sell");

    let stmt_snap = client
        .prepare(
            "INSERT INTO pool_snapshots (pool_address, dex_type, token_a, token_b, \
             reserve_a, reserve_b, fee_bps) VALUES ($1,$2,$3,$4,$5,$6,$7)",
        )
        .await
        .expect("prepare stmt_snap");

    let stmt_pool_perf = client
        .prepare(
            "INSERT INTO pool_performance (pool_address, dex_type, total_opps, total_sent, \
             total_landed, total_profit, avg_score, last_success, last_failure) \
             VALUES ($1,$2,1,1,CASE WHEN $3 THEN 1 ELSE 0 END,$4::bigint,$5, \
             CASE WHEN $3 THEN now() ELSE NULL END, \
             CASE WHEN NOT $3 THEN now() ELSE NULL END) \
             ON CONFLICT (pool_address) DO UPDATE SET \
             total_opps = pool_performance.total_opps + 1, \
             total_sent = pool_performance.total_sent + 1, \
             total_landed = pool_performance.total_landed + CASE WHEN $3 THEN 1 ELSE 0 END, \
             total_profit = pool_performance.total_profit + $4::bigint, \
             avg_score = (pool_performance.avg_score * pool_performance.total_opps + $5) \
                         / (pool_performance.total_opps + 1), \
             last_success = CASE WHEN $3 THEN now() ELSE pool_performance.last_success END, \
             last_failure = CASE WHEN NOT $3 THEN now() ELSE pool_performance.last_failure END, \
             updated_at = now()",
        )
        .await
        .expect("prepare stmt_pool_perf");

    let stmt_route_candidate = client
        .prepare(
            "INSERT INTO route_candidates (slot, detected_at, strategy, borrow_amount, n_hops, \
             gross_profit, net_profit, score, ml_boost, risk_factor, emitted, reject_reason, \
             pools, token_mints, reserves_a, reserves_b) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)",
        )
        .await
        .expect("prepare stmt_route_candidate");

    let stmt_pool_discovery = client
        .prepare(
            "INSERT INTO pool_discoveries (pool_address, dex_type, token_a, token_b, \
             initial_reserve_a, initial_reserve_b, discovery_source, discovery_slot) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
        )
        .await
        .expect("prepare stmt_pool_discovery");

    let stmt_wallet_swap = client
        .prepare(
            "INSERT INTO wallet_swaps (wallet_address, tx_signature, slot, dex_type, pool, \
             token_in, token_out, amount_in, amount_out, is_buy, price_impact_bps, \
             pool_reserve_a, pool_reserve_b) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
        )
        .await
        .expect("prepare stmt_wallet_swap");

    let stmt_wallet_update = client
        .prepare(
            "INSERT INTO tracked_wallets (wallet_address, label, category, total_swaps, \
             total_profit_lamports, win_rate, last_swap_slot, last_swap_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,now()) \
             ON CONFLICT (wallet_address) DO UPDATE SET \
             total_swaps = $4, total_profit_lamports = $5, win_rate = $6, \
             last_swap_slot = $7, last_swap_at = now(), updated_at = now()",
        )
        .await
        .expect("prepare stmt_wallet_update");

    let stmt_pattern = client
        .prepare(
            "INSERT INTO wallet_patterns (pattern_type, wallet_address, token_mint, pool, \
             dex_type, occurrences, success_count, avg_profit, avg_delay_ms, confidence, features) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) \
             ON CONFLICT ON CONSTRAINT wallet_patterns_pkey DO NOTHING",
        )
        .await
        .expect("prepare stmt_pattern");

    // Pattern upsert uses a separate statement since patterns are identified by type+wallet+token
    let stmt_pattern_upsert = client
        .prepare(
            "INSERT INTO wallet_patterns (pattern_type, wallet_address, token_mint, pool, \
             dex_type, occurrences, success_count, avg_profit, avg_delay_ms, confidence, features) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) \
             ON CONFLICT (id) DO UPDATE SET \
             occurrences = $6, success_count = $7, avg_profit = $8, avg_delay_ms = $9, \
             confidence = $10, features = $11, updated_at = now()",
        )
        .await
        .expect("prepare stmt_pattern_upsert");

    let mut batch: Vec<LogEvent> = Vec::with_capacity(128);
    let mut total_written: u64 = 0;

    loop {
        // Block on first event, then drain more
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => batch.push(event),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                info!("pg-logger: channel closed, shutting down");
                break;
            }
        }

        // Drain up to 127 more events
        while batch.len() < 128 {
            match rx.try_recv() {
                Ok(event) => batch.push(event),
                Err(_) => break,
            }
        }

        // Process batch
        let mut pg_errors = 0u32;
        for event in batch.drain(..) {
            let (label, result) = match event {
                LogEvent::Opportunity(opp) => {
                    ("opportunity", insert_opportunity(&client, &stmt_opp, &stmt_hop, &opp).await)
                }
                LogEvent::Execution(exec) => {
                    ("execution", insert_execution(&client, &stmt_exec, &exec).await)
                }
                LogEvent::ExecutionUpdate(upd) => {
                    ("exec_update", update_execution(&client, &stmt_exec_update, &upd).await)
                }
                LogEvent::Sniper(ev) => {
                    ("sniper", insert_sniper(&client, &stmt_sniper, &ev).await)
                }
                LogEvent::SniperSellUpdate(upd) => {
                    ("sniper_sell", update_sniper_sell(&client, &stmt_sniper_sell, &upd).await)
                }
                LogEvent::PoolSnapshots(snaps) => {
                    ("pool_snap", insert_pool_snapshots(&client, &stmt_snap, &snaps).await)
                }
                LogEvent::PoolPerfUpdate(upd) => {
                    ("pool_perf", update_pool_perf(&client, &stmt_pool_perf, &upd).await)
                }
                LogEvent::RouteCandidate(rc) => {
                    ("route_cand", insert_route_candidate(&client, &stmt_route_candidate, &rc).await)
                }
                LogEvent::PoolDiscovery(pd) => {
                    ("pool_disc", insert_pool_discovery(&client, &stmt_pool_discovery, &pd).await)
                }
                LogEvent::WalletSwap(ws) => {
                    ("wallet_swap", insert_wallet_swap(&client, &stmt_wallet_swap, &ws).await)
                }
                LogEvent::WalletUpdate(wu) => {
                    ("wallet_upd", upsert_wallet(&client, &stmt_wallet_update, &wu).await)
                }
                LogEvent::PatternUpdate(pu) => {
                    ("pattern", insert_pattern(&client, &stmt_pattern, &pu).await)
                }
            };

            match result {
                Ok(_) => total_written += 1,
                Err(e) => {
                    pg_errors += 1;
                    // Log first error per batch, then suppress to avoid log spam.
                    if pg_errors <= 3 {
                        error!("pg-logger write error [{label}]: {e}");
                    }
                }
            }
        }
        if pg_errors > 3 {
            error!("pg-logger: {pg_errors} total errors in batch (suppressed {} repeats)", pg_errors - 3);
        }

        // Periodic stats
        if total_written % 500 == 0 && total_written > 0 {
            let d = dropped.load(Ordering::Relaxed);
            info!("pg-logger: written={total_written} dropped={d}");
        }
    }
}

async fn connect_with_retry(url: &str) -> Client {
    let mut delay = Duration::from_secs(1);
    loop {
        match tokio_postgres::connect(url, NoTls).await {
            Ok((client, connection)) => {
                tokio::spawn(async move {
                    if let Err(e) = connection.await {
                        error!("pg connection error: {e}");
                    }
                });
                return client;
            }
            Err(e) => {
                warn!("pg-logger: connect failed ({e}), retrying in {delay:?}");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
}

async fn insert_opportunity(
    client: &Client,
    stmt_opp: &tokio_postgres::Statement,
    stmt_hop: &tokio_postgres::Statement,
    opp: &LogOpportunity,
) -> Result<(), tokio_postgres::Error> {
    let row = client
        .query_one(
            stmt_opp,
            &[
                &(opp.opp_id as i64),
                &(opp.slot as i64),
                &opp.detected_at,
                &opp.signal_type,
                &opp.source_strategy,
                &opp.token_mints,
                &(opp.borrow_amount as i64),
                &opp.flash_provider,
                &(opp.gross_profit as i64),
                &opp.net_profit,
                &opp.score,
                &opp.risk_factor,
                &opp.n_hops,
                &opp.fast_arb,
            ],
        )
        .await?;

    let db_id: i64 = row.get(0);

    for hop in &opp.hops {
        client
            .execute(
                stmt_hop,
                &[
                    &db_id,
                    &hop.hop_index,
                    &hop.pool,
                    &hop.dex_type,
                    &hop.token_in,
                    &hop.token_out,
                    &hop.amount_in.map(|v| v as i64),
                    &(hop.amount_out as i64),
                    &hop.price_impact,
                    &hop.pool_reserve_a.map(|v| v as i64),
                    &hop.pool_reserve_b.map(|v| v as i64),
                    &hop.reserve_age_ms.map(|v| v as i64),
                ],
            )
            .await?;
    }

    Ok(())
}

async fn insert_execution(
    client: &Client,
    stmt: &tokio_postgres::Statement,
    exec: &LogExecution,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            stmt,
            &[
                &(exec.opp_id as i64),
                &exec.tx_signature,
                &exec.send_channels,
                &exec.fast_arb,
                &exec.detect_to_build_us.map(|v| v as i64),
                &exec.build_to_send_us.map(|v| v as i64),
                &exec.total_us.map(|v| v as i64),
                &exec.jito_tip.map(|v| v as i64),
                &exec.tpu_leaders,
            ],
        )
        .await?;
    Ok(())
}

async fn update_execution(
    client: &Client,
    stmt: &tokio_postgres::Statement,
    upd: &LogExecutionUpdate,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            stmt,
            &[
                &upd.tx_signature,
                &upd.landed,
                &upd.landed_slot.map(|v| v as i64),
                &upd.actual_profit,
                &upd.error_message,
            ],
        )
        .await?;
    Ok(())
}

async fn insert_sniper(
    client: &Client,
    stmt: &tokio_postgres::Statement,
    ev: &LogSniperEvent,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            stmt,
            &[
                &(ev.slot as i64),
                &ev.token_mint,
                &ev.pool,
                &ev.source,
                &ev.safety_passed,
                &ev.safety_reason,
                &ev.pool_sol_reserve.map(|v| v as i64),
                &(ev.snipe_lamports as i64),
                &ev.tx_signature,
                &ev.build_ms.map(|v| v as i64),
                &ev.send_ms.map(|v| v as i64),
            ],
        )
        .await?;
    Ok(())
}

async fn update_sniper_sell(
    client: &Client,
    stmt: &tokio_postgres::Statement,
    upd: &LogSniperSellUpdate,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            stmt,
            &[
                &upd.token_mint,
                &upd.sell_triggered,
                &upd.sell_reason,
                &upd.sell_tx_sig,
                &upd.sell_sol_received.map(|v| v as i64),
                &upd.hold_duration_ms.map(|v| v as i64),
                &upd.pnl_lamports,
            ],
        )
        .await?;
    Ok(())
}

async fn insert_pool_snapshots(
    client: &Client,
    stmt: &tokio_postgres::Statement,
    snaps: &[LogPoolSnapshot],
) -> Result<(), tokio_postgres::Error> {
    for snap in snaps {
        client
            .execute(
                stmt,
                &[
                    &snap.pool_address,
                    &snap.dex_type,
                    &snap.token_a,
                    &snap.token_b,
                    &(snap.reserve_a as i64),
                    &(snap.reserve_b as i64),
                    &snap.fee_bps,
                ],
            )
            .await?;
    }
    Ok(())
}

async fn update_pool_perf(
    client: &Client,
    stmt: &tokio_postgres::Statement,
    upd: &LogPoolPerfUpdate,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            stmt,
            &[
                &upd.pool_address,
                &upd.dex_type,
                &upd.landed,
                &(upd.profit),
                &upd.score,
            ],
        )
        .await?;
    Ok(())
}

async fn insert_route_candidate(
    client: &Client,
    stmt: &tokio_postgres::Statement,
    rc: &LogRouteCandidate,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            stmt,
            &[
                &(rc.slot as i64),
                &rc.detected_at,
                &rc.strategy,
                &(rc.borrow_amount as i64),
                &rc.n_hops,
                &(rc.gross_profit as i64),
                &rc.net_profit,
                &rc.score,
                &rc.ml_boost,
                &rc.risk_factor,
                &rc.emitted,
                &rc.reject_reason,
                &rc.pools,
                &rc.token_mints,
                &rc.reserves_a,
                &rc.reserves_b,
            ],
        )
        .await?;
    Ok(())
}

async fn insert_pool_discovery(
    client: &Client,
    stmt: &tokio_postgres::Statement,
    pd: &LogPoolDiscovery,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            stmt,
            &[
                &pd.pool_address,
                &pd.dex_type,
                &pd.token_a,
                &pd.token_b,
                &pd.initial_reserve_a.map(|v| v as i64),
                &pd.initial_reserve_b.map(|v| v as i64),
                &pd.discovery_source,
                &pd.discovery_slot.map(|v| v as i64),
            ],
        )
        .await?;
    Ok(())
}

async fn insert_wallet_swap(
    client: &Client,
    stmt: &tokio_postgres::Statement,
    ws: &LogWalletSwap,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            stmt,
            &[
                &ws.wallet_address,
                &ws.tx_signature,
                &(ws.slot as i64),
                &ws.dex_type,
                &ws.pool,
                &ws.token_in,
                &ws.token_out,
                &(ws.amount_in as i64),
                &(ws.amount_out as i64),
                &ws.is_buy,
                &ws.price_impact_bps,
                &ws.pool_reserve_a.map(|v| v as i64),
                &ws.pool_reserve_b.map(|v| v as i64),
            ],
        )
        .await?;
    Ok(())
}

async fn upsert_wallet(
    client: &Client,
    stmt: &tokio_postgres::Statement,
    wu: &LogWalletUpdate,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            stmt,
            &[
                &wu.wallet_address,
                &wu.label,
                &wu.category,
                &(wu.total_swaps as i64),
                &wu.total_profit_lamports,
                &wu.win_rate,
                &wu.last_swap_slot.map(|v| v as i64),
            ],
        )
        .await?;
    Ok(())
}

async fn insert_pattern(
    client: &Client,
    stmt: &tokio_postgres::Statement,
    pu: &LogPatternUpdate,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            stmt,
            &[
                &pu.pattern_type,
                &pu.wallet_address,
                &pu.token_mint,
                &pu.pool,
                &pu.dex_type,
                &(pu.occurrences as i64),
                &(pu.success_count as i64),
                &pu.avg_profit,
                &pu.avg_delay_ms.map(|v| v as i64),
                &pu.confidence,
                &pu.features,
            ],
        )
        .await?;
    Ok(())
}
