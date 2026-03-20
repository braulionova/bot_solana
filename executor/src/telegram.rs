//! telegram.rs – Telegram Bot API alerts for trade events and system status.
//!
//! Uses the Bot API with long polling (no webhook needed).
//! Set TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID env vars.

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::json;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// TelegramBot
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct TelegramBot {
    client: Client,
    token: String,
    chat_id: String,
    enabled: bool,
}

impl TelegramBot {
    /// Create from environment variables TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID.
    /// If either is missing, returns a disabled instance (all sends are no-ops).
    pub fn from_env() -> Self {
        let token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
        let chat_id = std::env::var("TELEGRAM_CHAT_ID").unwrap_or_default();
        let enabled = !token.is_empty() && !chat_id.is_empty();
        if !enabled {
            warn!("[telegram] TELEGRAM_BOT_TOKEN or TELEGRAM_CHAT_ID not set – alerts disabled");
        }
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            token,
            chat_id,
            enabled,
        }
    }

    /// Send a text message (Markdown format).
    pub async fn send(&self, text: &str) -> Result<()> {
        if !self.enabled {
            warn!("[telegram] DISABLED — token/chat_id not set");
            return Ok(());
        }
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let body = json!({
            "chat_id":    self.chat_id,
            "text":       text,
            "parse_mode": "Markdown",
            "disable_web_page_preview": true,
        });
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("telegram send request")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            warn!(status = %status, body = %body, "[telegram] send failed");
        } else {
            info!("[telegram] message sent OK");
        }
        Ok(())
    }

    // ── Pre-formatted alert helpers ──────────────────────────────────────────

    /// Alert: arbitrage opportunity detected and queued for execution.
    pub async fn alert_opportunity_detected(
        &self,
        bundle_id: u64,
        net_profit_lamports: i64,
        hops: usize,
        score: f64,
        strategy: &str,
        dry_run: bool,
    ) {
        info!(bundle_id, net_profit_lamports, hops, strategy, "[telegram] alert_opportunity_detected called");
        let mode = if dry_run { "DRY RUN" } else { "LIVE" };
        let sol_profit = net_profit_lamports as f64 / 1e9;
        let text = format!(
            "👀 *Arbitrage Opportunity* \\[{}\\]\n\
             Strategy: `{}`\n\
             Bundle:   `{}`\n\
             Profit:   `{:.6} SOL` (`{} lamports`)\n\
             Hops:     `{}`\n\
             Score:    `{:.3}`",
            mode, strategy, bundle_id, sol_profit, net_profit_lamports, hops, score
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_opportunity_detected failed");
        }
    }

    /// Alert: arbitrage trade executed.
    pub async fn alert_trade_executed(
        &self,
        profit_sol: f64,
        net_profit_lamports: i64,
        hops: usize,
        strategy: &str,
        bundle_id: &str,
    ) {
        let emoji = if profit_sol >= 0.05 {
            "🚀"
        } else if profit_sol >= 0.01 {
            "✅"
        } else {
            "💰"
        };
        let text = format!(
            "{} *Helios Trade Executed*\n\
             Strategy: `{}`\n\
             Profit:   `{:.6} SOL` (`{} lamports`)\n\
             Hops:     `{}`\n\
             Bundle:   `{}`",
            emoji, strategy, profit_sol, net_profit_lamports, hops, bundle_id
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_trade_executed failed");
        }
    }

    /// Alert: simulation passed — ruta ejecutable encontrada.
    pub async fn alert_simulation_succeeded(
        &self,
        provider: &str,
        hops: usize,
        net_profit_lamports: i64,
        units: u64,
        dry_run: bool,
    ) {
        let mode = if dry_run { "DRY RUN" } else { "LIVE" };
        let sol_profit = net_profit_lamports as f64 / 1e9;
        let text = format!(
            "✅ *Simulation PASSED* \\[{}\\]\n\
             Provider: `{}`\n\
             Hops:     `{}`\n\
             Profit:   `{:.6} SOL` (`{} lamports`)\n\
             CU:       `{}`",
            mode, provider, hops, sol_profit, net_profit_lamports, units
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_simulation_succeeded failed");
        }
    }

    /// Alert: bot switching from DRY_RUN to LIVE mode.
    pub async fn alert_going_live(&self, provider: &str, net_profit_lamports: i64) {
        let sol_profit = net_profit_lamports as f64 / 1e9;
        let text = format!(
            "🚀 *Pasando a modo LIVE*\n\
             Primera simulación exitosa con `{}`\n\
             Profit estimado: `{:.6} SOL`\n\
             DRY\\_RUN → false. Reiniciando helios\\-bot...",
            provider, sol_profit
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_going_live failed");
        }
    }

    /// Alert: simulation failed (high signal of bad arb).
    pub async fn alert_simulation_failed(&self, reason: &str) {
        let text = format!("⚠️ *Simulation Failed*\nReason: `{}`", reason);
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_simulation_failed failed");
        }
    }

    // ── New Strategy Alerts ──────────────────────────────────────────────

    /// Alert: new strategy arb opportunity detected (more detail than generic).
    pub async fn alert_new_strategy_opportunity(
        &self,
        strategy: &str,
        net_profit_lamports: i64,
        hops: usize,
        dex_venues: &str,
        borrow_sol: f64,
    ) {
        let sol_profit = net_profit_lamports as f64 / 1e9;
        let margin_bps = if borrow_sol > 0.0 {
            (sol_profit / borrow_sol) * 10_000.0
        } else {
            0.0
        };
        let emoji = match strategy {
            "pump_graduation_arb" => "🎓",
            "cyclic_3leg" => "🔄",
            "dlmm_whirlpool_arb" => "📊",
            _ => "🔍",
        };
        let text = format!(
            "{} *{} Detected* \\[LIVE\\]\n\
             Venues:   `{}`\n\
             Profit:   `{:.6} SOL` (`{:.1} bps`)\n\
             Borrow:   `{:.4} SOL`\n\
             Hops:     `{}`",
            emoji, strategy, dex_venues, sol_profit, margin_bps, borrow_sol, hops,
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_new_strategy_opportunity failed");
        }
    }

    /// Alert: new strategy arb trade landed on-chain.
    pub async fn alert_new_strategy_landed(
        &self,
        strategy: &str,
        net_profit_lamports: i64,
        hops: usize,
        dex_venues: &str,
        tx_sig: &str,
    ) {
        let sol_profit = net_profit_lamports as f64 / 1e9;
        let emoji = match strategy {
            "pump_graduation_arb" => "🎓✅",
            "cyclic_3leg" => "🔄✅",
            "dlmm_whirlpool_arb" => "📊✅",
            _ => "✅",
        };
        let sig_short = if tx_sig.len() > 16 { &tx_sig[..16] } else { tx_sig };
        let text = format!(
            "{} *{} LANDED*\n\
             Venues: `{}`\n\
             Profit: `{:.6} SOL` (`{} lamports`)\n\
             Hops:   `{}`\n\
             TX:     `{}...`",
            emoji, strategy, dex_venues, sol_profit, net_profit_lamports, hops, sig_short,
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_new_strategy_landed failed");
        }
    }

    /// Alert: daily profit summary.
    pub async fn alert_daily_summary(&self, trades: u64, profit_usd: f64, win_rate_pct: f64) {
        let emoji = if profit_usd > 0.0 { "📈" } else { "📉" };
        let text = format!(
            "{} *Daily Summary*\n\
             Trades:   `{}`\n\
             Profit:   `${:.2}`\n\
             Win Rate: `{:.1}%`",
            emoji, trades, profit_usd, win_rate_pct
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_daily_summary failed");
        }
    }

    /// Alert: drawdown exceeded threshold.
    pub async fn alert_drawdown(&self, drawdown_pct: f64) {
        let text = format!(
            "🔴 *DRAWDOWN ALERT*\nDrawdown: `{:.2}%`\nConsider pausing bot.",
            drawdown_pct
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_drawdown failed");
        }
    }

    /// Fetch SOL + USDC + USDT balances and send them to Telegram.
    /// Skips tokens with zero balance.
    pub async fn alert_wallet_balance(&self, rpc_url: &str, wallet: &str, net_profit_lamports: i64) {
        if !self.enabled {
            return;
        }
        let sol_profit = net_profit_lamports as f64 / 1e9;
        let mut lines: Vec<String> = vec![
            format!("💰 *Trade ejecutado* `+{:.6} SOL`", sol_profit),
            String::new(),
            format!("🏦 *Wallet:* `{}`", &wallet[..8]),
        ];

        // SOL balance via getBalance
        let sol_balance = self.rpc_get_balance(rpc_url, wallet).await;
        if let Some(lamports) = sol_balance {
            let sol = lamports as f64 / 1e9;
            if sol > 0.0 {
                lines.push(format!("◎ SOL:  `{:.6}`", sol));
            }
        }

        // USDC + USDT via getTokenAccountsByOwner
        const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        const USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
        for (symbol, mint) in [("USDC", USDC), ("USDT", USDT)] {
            if let Some(amount) = self.rpc_get_token_balance(rpc_url, wallet, mint).await {
                if amount > 0.0 {
                    lines.push(format!("$ {}:  `{:.2}`", symbol, amount));
                }
            }
        }

        let text = lines.join("\n");
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_wallet_balance failed");
        }
    }

    async fn rpc_get_balance(&self, rpc_url: &str, wallet: &str) -> Option<u64> {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "getBalance",
            "params": [wallet, {"commitment": "confirmed"}]
        });
        let resp: serde_json::Value = self.client
            .post(rpc_url).json(&body).send().await.ok()?
            .json().await.ok()?;
        resp["result"]["value"].as_u64()
    }

    async fn rpc_get_token_balance(&self, rpc_url: &str, wallet: &str, mint: &str) -> Option<f64> {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "getTokenAccountsByOwner",
            "params": [
                wallet,
                {"mint": mint},
                {"encoding": "jsonParsed", "commitment": "confirmed"}
            ]
        });
        let resp: serde_json::Value = self.client
            .post(rpc_url).json(&body).send().await.ok()?
            .json().await.ok()?;
        let accounts = resp["result"]["value"].as_array()?;
        // Sum across all token accounts for this mint (usually one)
        let total: f64 = accounts.iter()
            .filter_map(|a| {
                a["account"]["data"]["parsed"]["info"]["tokenAmount"]["uiAmount"].as_f64()
            })
            .sum();
        Some(total)
    }

    /// Alert: bot starting up.
    pub async fn alert_startup(&self, version: &str) {
        let text = format!("🟢 *Helios Bot Started*\nVersion: `{}`", version);
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_startup failed");
        }
    }

    /// Alert: graduation/migration detected from a launchpad.
    pub async fn alert_graduation_detected(
        &self,
        token_mint: &str,
        pool: &str,
        slot: u64,
        source: &str,
    ) {
        let token_short = if token_mint.len() > 8 { &token_mint[..8] } else { token_mint };
        let pool_short = if pool.len() > 8 { &pool[..8] } else { pool };
        let text = format!(
            "🎓 *Migración Detectada*\n\
             Fuente:  `{}`\n\
             Token:   `{}...`\n\
             Pool:    `{}...`\n\
             Slot:    `{}`\n\
             [Solscan](https://solscan.io/token/{})",
            source, token_short, pool_short, slot, token_mint
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_graduation_detected failed");
        }
    }

    /// Alert: successful arbitrage from a graduation snipe.
    pub async fn alert_graduation_arb_success(
        &self,
        token_mint: &str,
        pool: &str,
        sol_amount: f64,
        bundle_id: &str,
        tpu_sent: usize,
        build_ms: u128,
        send_ms: u128,
    ) {
        let token_short = if token_mint.len() > 8 { &token_mint[..8] } else { token_mint };
        let text = format!(
            "🚀 *Snipe Enviado \\- Graduación*\n\
             Token:     `{}...`\n\
             Monto:     `{:.4} SOL`\n\
             Bundle:    `{}`\n\
             TPU sent:  `{}`\n\
             Build:     `{}ms`\n\
             Total:     `{}ms`\n\
             [Solscan](https://solscan.io/token/{})",
            token_short, sol_amount, bundle_id, tpu_sent, build_ms, send_ms, token_mint
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_graduation_arb_success failed");
        }
    }

    /// Alert: whale backrun opportunity detected.
    pub async fn alert_whale_backrun(
        &self,
        pair: &str,
        net_profit_lamports: i64,
        dex_a: &str,
        dex_b: &str,
    ) {
        let sol_profit = net_profit_lamports as f64 / 1e9;
        let text = format!(
            "🐋 *Whale Backrun Detectado*\n\
             Par:       `{}`\n\
             Profit:    `{:.6} SOL`\n\
             Ruta:      `{} → {}`",
            pair, sol_profit, dex_a, dex_b
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_whale_backrun failed");
        }
    }

    /// Alert: LST arbitrage opportunity detected.
    pub async fn alert_lst_arb(
        &self,
        lst_symbol: &str,
        net_profit_lamports: i64,
        margin_bps: f64,
    ) {
        let sol_profit = net_profit_lamports as f64 / 1e9;
        let text = format!(
            "🔄 *LST Arb Detectado*\n\
             Token:     `{}`\n\
             Profit:    `{:.6} SOL`\n\
             Margen:    `{:.1} bps`",
            lst_symbol, sol_profit, margin_bps
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_lst_arb failed");
        }
    }

    /// Alert: liquidation opportunity detected (Phase A).
    pub async fn alert_liquidation_detected(
        &self,
        protocol: &str,
        obligation: &str,
        health_factor: f64,
        debt_sol: f64,
        collateral_mint: &str,
        debt_mint: &str,
    ) {
        let obligation_short = if obligation.len() > 8 { &obligation[..8] } else { obligation };
        let col_short = if collateral_mint.len() > 8 { &collateral_mint[..8] } else { collateral_mint };
        let debt_short = if debt_mint.len() > 8 { &debt_mint[..8] } else { debt_mint };
        let text = format!(
            "💀 *Liquidación Detectada*\n\
             Protocolo:   `{}`\n\
             Obligación:  `{}...`\n\
             Health:      `{:.4}`\n\
             Deuda:       `{:.4} SOL`\n\
             Colateral:   `{}...`\n\
             Deuda mint:  `{}...`",
            protocol, obligation_short, health_factor, debt_sol, col_short, debt_short
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_liquidation_detected failed");
        }
    }

    /// Alert: sniper skipped a token due to safety check.
    pub async fn alert_sniper_safety_skip(
        &self,
        token_mint: &str,
        reason: &str,
        pool_sol: f64,
    ) {
        let token_short = if token_mint.len() > 8 { &token_mint[..8] } else { token_mint };
        let text = format!(
            "🚫 *Sniper SKIP*\n\
             Token:   `{}...`\n\
             Razón:   `{}`\n\
             Pool:    `{:.1} SOL`\n\
             [Solscan](https://solscan.io/token/{})",
            token_short, reason, pool_sol, token_mint
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_sniper_safety_skip failed");
        }
    }

    /// Alert: auto-sell result with P&L.
    pub async fn alert_auto_sell_result(
        &self,
        token_mint: &str,
        sell_reason: &str,
        cost_sol: f64,
        received_sol: f64,
        hold_secs: u64,
    ) {
        let pnl = received_sol - cost_sol;
        let pnl_pct = if cost_sol > 0.0 { pnl / cost_sol * 100.0 } else { 0.0 };
        let emoji = if pnl >= 0.0 { "✅" } else { "🔴" };
        let token_short = if token_mint.len() > 8 { &token_mint[..8] } else { token_mint };
        let text = format!(
            "{} *Auto\\-Sell {}*\n\
             Token:   `{}...`\n\
             Costo:   `{:.6} SOL`\n\
             Venta:   `{:.6} SOL`\n\
             P&L:     `{:+.6} SOL ({:+.1}%)`\n\
             Hold:    `{}s`\n\
             Razón:   `{}`",
            emoji,
            if pnl >= 0.0 { "PROFIT" } else { "LOSS" },
            token_short, cost_sol, received_sol, pnl, pnl_pct, hold_secs, sell_reason
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_auto_sell_result failed");
        }
    }

    /// Alert: generic warning.
    pub async fn alert_warning(&self, msg: &str) {
        let text = format!("⚠️ *Warning*\n{}", msg);
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_warning failed");
        }
    }

    // -----------------------------------------------------------------------
    // Wallet Tracker alerts
    // -----------------------------------------------------------------------

    /// Alert when a smart money wallet is detected.
    pub async fn alert_smart_money_detected(
        &self,
        wallet: &str,
        category: &str,
        total_swaps: u64,
        win_rate: f64,
    ) {
        let text = format!(
            "*SMART MONEY DETECTED*\n\
             Wallet: `{}`\n\
             Category: {}\n\
             Swaps: {} | Win rate: {:.1}%",
            &wallet[..8],
            category,
            total_swaps,
            win_rate * 100.0,
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_smart_money_detected failed");
        }
    }

    /// Alert when a significant wallet pattern is detected.
    pub async fn alert_wallet_pattern(
        &self,
        pattern_type: &str,
        wallet: &str,
        token: Option<&str>,
        confidence: f64,
        features: &serde_json::Value,
    ) {
        let token_str = token.map(|t| format!("`{}`", &t[..8.min(t.len())])).unwrap_or_else(|| "N/A".into());
        let text = format!(
            "*WALLET PATTERN: {}*\n\
             Wallet: `{}`\n\
             Token: {}\n\
             Confidence: {:.0}%\n\
             Details: {}",
            pattern_type.to_uppercase(),
            &wallet[..8.min(wallet.len())],
            token_str,
            confidence * 100.0,
            serde_json::to_string(features).unwrap_or_default(),
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_wallet_pattern failed");
        }
    }

    /// Alert when a new pool is discovered.
    pub async fn alert_pool_discovered(
        &self,
        pool: &str,
        dex: &str,
        token_a: &str,
        token_b: &str,
        source: &str,
    ) {
        let text = format!(
            "*NEW POOL DISCOVERED*\n\
             Pool: `{}`\n\
             DEX: {} | Source: {}\n\
             Tokens: `{}`/`{}`",
            &pool[..8.min(pool.len())],
            dex,
            source,
            &token_a[..8.min(token_a.len())],
            &token_b[..8.min(token_b.len())],
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_pool_discovered failed");
        }
    }

    /// Daily report with full PG stats + wallet tracker summary.
    pub async fn alert_daily_report(
        &self,
        total_opps: u64,
        total_executions: u64,
        total_landed: u64,
        total_profit_lamports: i64,
        wallets_tracked: usize,
        smart_money_count: usize,
        patterns_detected: u64,
        pools_discovered: u64,
        routes_logged: u64,
    ) {
        let sol_profit = total_profit_lamports as f64 / 1_000_000_000.0;
        let text = format!(
            "*DAILY REPORT - HELIOS GOLD V2*\n\
             ━━━━━━━━━━━━━━━━━━━━\n\
             *Trading*\n\
             Opportunities: {}\n\
             Executions: {} | Landed: {}\n\
             P&L: {:.4} SOL\n\
             ━━━━━━━━━━━━━━━━━━━━\n\
             *Wallet Tracker*\n\
             Wallets tracked: {}\n\
             Smart money: {}\n\
             Patterns detected: {}\n\
             ━━━━━━━━━━━━━━━━━━━━\n\
             *ML Data*\n\
             Pools discovered: {}\n\
             Routes logged: {}",
            total_opps,
            total_executions,
            total_landed,
            sol_profit,
            wallets_tracked,
            smart_money_count,
            patterns_detected,
            pools_discovered,
            routes_logged,
        );
        if let Err(e) = self.send(&text).await {
            warn!(error = %e, "[telegram] alert_daily_report failed");
        }
    }

    // -----------------------------------------------------------------------
    // Email daily report
    // -----------------------------------------------------------------------

    /// Send daily report via email using SMTP (Gmail/external SMTP).
    /// Requires SMTP_HOST, SMTP_USER, SMTP_PASS, REPORT_EMAIL env vars.
    pub async fn send_daily_email(
        &self,
        subject: &str,
        body: &str,
    ) {
        let email_to = std::env::var("REPORT_EMAIL").unwrap_or_default();
        let smtp_host = std::env::var("SMTP_HOST").unwrap_or_else(|_| "smtp.gmail.com".into());
        let smtp_user = std::env::var("SMTP_USER").unwrap_or_default();
        let smtp_pass = std::env::var("SMTP_PASS").unwrap_or_default();

        if email_to.is_empty() || smtp_user.is_empty() || smtp_pass.is_empty() {
            debug!("[email] SMTP credentials not configured, skipping email report");
            return;
        }

        // Use reqwest to POST to a simple SMTP relay or use raw SMTP via TCP.
        // For simplicity, we use Python's smtplib via subprocess (always available).
        let python_script = format!(
            r#"
import smtplib
from email.mime.text import MIMEText
msg = MIMEText('''{body}''', 'plain')
msg['Subject'] = '{subject}'
msg['From'] = '{smtp_user}'
msg['To'] = '{email_to}'
try:
    s = smtplib.SMTP('{smtp_host}', 587)
    s.starttls()
    s.login('{smtp_user}', '{smtp_pass}')
    s.sendmail('{smtp_user}', '{email_to}', msg.as_string())
    s.quit()
    print('OK')
except Exception as e:
    print(f'FAIL: {{e}}')
"#,
            body = body.replace('\'', "\\'").replace('\n', "\\n"),
            subject = subject.replace('\'', "\\'"),
            smtp_user = smtp_user,
            smtp_pass = smtp_pass,
            smtp_host = smtp_host,
            email_to = email_to,
        );

        match tokio::process::Command::new("python3")
            .arg("-c")
            .arg(&python_script)
            .output()
            .await
        {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if stdout.contains("OK") {
                    debug!("[email] daily report sent to {}", email_to);
                } else {
                    warn!("[email] send failed: {}", stdout.trim());
                }
            }
            Err(e) => {
                warn!(error = %e, "[email] failed to run python3 for email");
            }
        }
    }

    /// Send a plain text message (no Markdown parsing — safe for emojis and special chars).
    pub async fn send_raw(&self, text: &str) {
        if !self.enabled {
            return;
        }
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let body = json!({
            "chat_id": self.chat_id,
            "text":    text,
            "disable_web_page_preview": true,
        });
        let _ = self.client.post(&url).json(&body).send().await;
    }
}

// ---------------------------------------------------------------------------
// Daily stats tracker
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct TradeStats {
    pub trades_total: AtomicU64,
    pub trades_won: AtomicU64,
    /// Accumulated profit in micro-USD (multiply by 1e-6 to get USD).
    pub profit_usd_micro: AtomicU64,
    /// Accumulated loss in micro-USD.
    pub loss_usd_micro: AtomicU64,
}

impl TradeStats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            trades_total: AtomicU64::new(0),
            trades_won: AtomicU64::new(0),
            profit_usd_micro: AtomicU64::new(0),
            loss_usd_micro: AtomicU64::new(0),
        })
    }

    pub fn record_win(&self, profit_usd: f64) {
        self.trades_total.fetch_add(1, Ordering::Relaxed);
        self.trades_won.fetch_add(1, Ordering::Relaxed);
        self.profit_usd_micro
            .fetch_add((profit_usd * 1_000_000.0) as u64, Ordering::Relaxed);
    }

    pub fn record_loss(&self, loss_usd: f64) {
        self.trades_total.fetch_add(1, Ordering::Relaxed);
        self.loss_usd_micro
            .fetch_add((loss_usd * 1_000_000.0) as u64, Ordering::Relaxed);
    }

    pub fn win_rate(&self) -> f64 {
        let total = self.trades_total.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        self.trades_won.load(Ordering::Relaxed) as f64 / total as f64 * 100.0
    }

    pub fn net_profit_usd(&self) -> f64 {
        let profit = self.profit_usd_micro.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        let loss = self.loss_usd_micro.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        profit - loss
    }

    pub fn trades(&self) -> u64 {
        self.trades_total.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Background summary loop
// ---------------------------------------------------------------------------

/// Spawn a thread that sends a summary via Telegram every `interval`.
pub fn spawn_summary_loop(
    bot: TelegramBot,
    stats: Arc<TradeStats>,
    interval: std::time::Duration,
    rt: std::sync::Arc<tokio::runtime::Runtime>,
) {
    std::thread::Builder::new()
        .name("telegram-summary".into())
        .spawn(move || loop {
            std::thread::sleep(interval);
            let trades = stats.trades();
            let profit_usd = stats.net_profit_usd();
            let win_rate = stats.win_rate();
            let bot_c = bot.clone();
            rt.block_on(async move {
                bot_c
                    .alert_daily_summary(trades, profit_usd, win_rate)
                    .await;
            });
        })
        .expect("spawn telegram summary thread");
}
