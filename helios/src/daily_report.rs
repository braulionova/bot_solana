//! daily_report.rs – Daily Telegram + email report with PG stats and wallet tracker summary.
//!
//! Spawns a background thread that:
//! 1. Every 24h queries PG for aggregated stats
//! 2. Sends a formatted report to Telegram
//! 3. Optionally sends an email to REPORT_EMAIL via SMTP

use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use executor::telegram::TelegramBot;
use market_engine::wallet_tracker::WalletTracker;

/// Spawn the daily report background thread.
pub fn spawn_daily_report(
    bot: TelegramBot,
    wallet_tracker: Option<Arc<WalletTracker>>,
) {
    std::thread::Builder::new()
        .name("daily-report".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("daily-report tokio rt");

            rt.block_on(async move {
                run_daily_report(bot, wallet_tracker).await;
            });
        })
        .expect("spawn daily-report thread");

    info!("daily-report thread started (24h interval)");
}

async fn run_daily_report(
    bot: TelegramBot,
    wallet_tracker: Option<Arc<WalletTracker>>,
) {
    // Wait 60s after startup before first report check.
    tokio::time::sleep(Duration::from_secs(60)).await;

    loop {
        // Calculate time until next report (00:00 UTC).
        let now = chrono::Utc::now();
        let tomorrow = (now + chrono::Duration::days(1))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let until_midnight = tomorrow
            .signed_duration_since(now.naive_utc())
            .to_std()
            .unwrap_or(Duration::from_secs(86400));

        info!(
            wait_hours = until_midnight.as_secs() / 3600,
            "daily-report: sleeping until midnight UTC"
        );
        tokio::time::sleep(until_midnight).await;

        // Gather stats from PG.
        let stats = gather_pg_stats().await;

        // Gather wallet tracker stats.
        let (wt_tracked, wt_smart) = wallet_tracker
            .as_ref()
            .map(|wt| (wt.tracked_count(), wt.smart_money_count()))
            .unwrap_or((0, 0));

        // Send Telegram report.
        bot.alert_daily_report(
            stats.total_opps,
            stats.total_executions,
            stats.total_landed,
            stats.total_profit,
            wt_tracked,
            wt_smart,
            stats.patterns_detected,
            stats.pools_discovered,
            stats.routes_logged,
        )
        .await;

        // Send email report.
        let email_body = format_email_report(&stats, wt_tracked, wt_smart);
        let date = chrono::Utc::now().format("%Y-%m-%d");
        bot.send_daily_email(
            &format!("Helios Gold V2 - Daily Report {}", date),
            &email_body,
        )
        .await;

        info!("daily-report: sent for {}", date);
    }
}

struct DailyStats {
    total_opps: u64,
    total_executions: u64,
    total_landed: u64,
    total_profit: i64,
    patterns_detected: u64,
    pools_discovered: u64,
    routes_logged: u64,
}

async fn gather_pg_stats() -> DailyStats {
    let db_url = std::env::var("DATABASE_URL").unwrap_or_default();
    if db_url.is_empty() {
        return DailyStats {
            total_opps: 0,
            total_executions: 0,
            total_landed: 0,
            total_profit: 0,
            patterns_detected: 0,
            pools_discovered: 0,
            routes_logged: 0,
        };
    }

    let (client, connection) = match tokio_postgres::connect(&db_url, tokio_postgres::NoTls).await
    {
        Ok(pair) => pair,
        Err(e) => {
            warn!("daily-report: PG connect failed: {e}");
            return DailyStats {
                total_opps: 0,
                total_executions: 0,
                total_landed: 0,
                total_profit: 0,
                patterns_detected: 0,
                pools_discovered: 0,
                routes_logged: 0,
            };
        }
    };

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            warn!("daily-report pg connection error: {e}");
        }
    });

    async fn q(client: &tokio_postgres::Client, sql: &str) -> i64 {
        client
            .query_one(sql, &[])
            .await
            .ok()
            .and_then(|r| r.try_get::<_, i64>(0).ok())
            .unwrap_or(0)
    }

    let total_opps = q(&client,
        "SELECT COUNT(*) FROM opportunities WHERE created_at > NOW() - INTERVAL '24 hours'",
    ).await as u64;
    let total_executions = q(&client,
        "SELECT COUNT(*) FROM executions WHERE created_at > NOW() - INTERVAL '24 hours'",
    ).await as u64;
    let total_landed = q(&client,
        "SELECT COUNT(*) FROM executions WHERE landed = true AND created_at > NOW() - INTERVAL '24 hours'",
    ).await as u64;
    let total_profit = q(&client,
        "SELECT COALESCE(SUM(actual_profit), 0) FROM executions WHERE landed = true AND created_at > NOW() - INTERVAL '24 hours'",
    ).await;
    let patterns_detected = q(&client,
        "SELECT COUNT(*) FROM wallet_patterns WHERE created_at > NOW() - INTERVAL '24 hours'",
    ).await as u64;
    let pools_discovered = q(&client,
        "SELECT COUNT(*) FROM pool_discoveries WHERE created_at > NOW() - INTERVAL '24 hours'",
    ).await as u64;
    let routes_logged = q(&client,
        "SELECT COUNT(*) FROM route_candidates WHERE created_at > NOW() - INTERVAL '24 hours'",
    ).await as u64;

    DailyStats {
        total_opps,
        total_executions,
        total_landed,
        total_profit,
        patterns_detected,
        pools_discovered,
        routes_logged,
    }
}

fn format_email_report(stats: &DailyStats, wt_tracked: usize, wt_smart: usize) -> String {
    let sol_profit = stats.total_profit as f64 / 1_000_000_000.0;
    let date = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC");

    format!(
        "HELIOS GOLD V2 - DAILY REPORT\n\
         Date: {date}\n\
         ========================================\n\n\
         TRADING\n\
         - Opportunities detected: {}\n\
         - Executions sent: {}\n\
         - Transactions landed: {}\n\
         - Net P&L: {:.4} SOL\n\n\
         WALLET TRACKER\n\
         - Wallets tracked: {}\n\
         - Smart money wallets: {}\n\
         - Patterns detected (24h): {}\n\n\
         ML DATA\n\
         - New pools discovered (24h): {}\n\
         - Route candidates logged (24h): {}\n\n\
         ========================================\n\
         This report is auto-generated by Helios Gold V2.\n",
        stats.total_opps,
        stats.total_executions,
        stats.total_landed,
        sol_profit,
        wt_tracked,
        wt_smart,
        stats.patterns_detected,
        stats.pools_discovered,
        stats.routes_logged,
    )
}
