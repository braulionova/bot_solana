//! sniper.rs – Direct Graduation Sniper (fast path, no flash loan).
//!
//! Flow: SpySignal::Graduation → build PumpSwap buy TX → preflight → TPU + Jito
//!
//! Bypasses the route engine and flash loan entirely.
//! Latency target: <50ms from signal to TX on wire.
//! Coverage: ~1.6s window via TPU fanout (4 leaders) + Jito Frankfurt bundle.
//!
//! PumpSwap buy accounts layout (23 accounts, post-creator-fee upgrade 2026-03):
//!   0: pool              (writable)
//!   1: user              payer (writable, SIGNER)
//!   2: global_config     PDA([b"global_config"], PUMPSWAP_PROGRAM)
//!   3: base_mint
//!   4: quote_mint
//!   5: user_base_ata     ATA(user, base_mint)   (writable)
//!   6: user_quote_ata    ATA(user, quote_mint)  (writable)
//!   7: pool_base_vault   ATA(pool, base_mint)   (writable)
//!   8: pool_quote_vault  ATA(pool, quote_mint)  (writable)
//!   9: protocol_fee_recipient
//!  10: protocol_fee_recipient_token_account  ATA(fee_recipient, quote_mint) (writable)
//!  11: base_token_program
//!  12: quote_token_program
//!  13: system_program
//!  14: associated_token_program
//!  15: event_authority   PDA([b"__event_authority"], PUMPSWAP_PROGRAM)
//!  16: program           PUMPSWAP_PROGRAM
//!  17: coin_creator_vault_ata  ATA(creator_vault_auth, quote_mint) (writable)
//!  18: coin_creator_vault_authority  PDA([b"creator_vault", creator], PUMPSWAP_PROGRAM)
//!  19: global_volume_accumulator  (readonly, singleton)
//!  20: user_volume_accumulator  PDA([b"user_volume_accumulator", user], PUMPSWAP_PROGRAM) (writable)
//!  21: fee_config  PDA([b"fee_config", PUMPSWAP_PROGRAM], FEE_PROGRAM)
//!  22: fee_program

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bincode::serialize;
use dashmap::DashMap;
use solana_sdk::{
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::v0::Message as MessageV0,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use tracing::{debug, info, warn};

use crate::jito_sender::JitoSender;
use crate::telegram::TelegramBot;
use crate::tpu_sender::TpuSender;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const PUMPSWAP_PROGRAM: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const ASSOC_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
/// Known PumpSwap protocol fee recipient (mainnet).
const PROTOCOL_FEE_RECIPIENT: &str = "62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV";
/// PumpSwap fee program (mainnet).
const FEE_PROGRAM: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";

/// PumpSwap buy discriminator (Anchor sha256 prefix of "global:buy").
const BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
/// PumpSwap sell discriminator (Anchor sha256 prefix of "global:sell").
const SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

/// Default SOL amount to spend per graduation snipe (0.02 SOL).
/// Reduced from 0.05 SOL to limit per-trade risk while tuning strategy.
const DEFAULT_SNIPE_LAMPORTS: u64 = 20_000_000;
/// Slippage tolerance for graduation snipes: 50%.
/// High tolerance because we're racing to buy — sandwich risk is low on first-slot buys.
const SLIPPAGE_BPS: u64 = 5000;
/// Max TX size in bytes.
const MAX_TX_SIZE: usize = 1232;
/// Cooldown before re-sniping the same pool (seconds).
const POOL_COOLDOWN_SECS: u64 = 300;
/// Minimum WSOL liquidity in pool to snipe (lamports). Below this, pool is too thin.
/// Data: PumpFun graduation analysis shows typical pools have ~82 SOL.
/// Winners had 614-797 SOL (7-10× average). Default: 200 SOL filters out 95%+ of rugs.
/// Override with MIN_POOL_SOL_LAMPORTS env var.
/// PumpFun graduation deposits ~85 SOL. Set to 50 SOL to catch fresh pools
/// that have partial liquidity. Brand-new pools (vault=0) skip this check entirely.
const DEFAULT_MIN_POOL_SOL: u64 = 50_000_000_000;
/// Delay before auto-sell (ms). Gives time for buy TX to land.
/// Reduced from 4s to 2s — sell ASAP before token dumps.
const AUTO_SELL_DELAY_MS: u64 = 2000;
/// Slippage for auto-sell: 30%. We want to exit fast, not maximize price.
const SELL_SLIPPAGE_BPS: u64 = 3000;

// ---------------------------------------------------------------------------
// GraduationSniper
// ---------------------------------------------------------------------------

pub struct GraduationSniper {
    payer: Arc<Keypair>,
    sender: Arc<JitoSender>,
    tpu_sender: Option<Arc<TpuSender>>,
    tg: TelegramBot,
    snipe_lamports: u64,
    min_pool_sol: u64,
    /// Pools already sniped — prevents duplicate snipes on the same graduation.
    sniped_pools: DashMap<Pubkey, Instant>,
    /// PostgreSQL logger for ML training (optional).
    strategy_logger: Option<Arc<strategy_logger::StrategyLogger>>,
}

impl GraduationSniper {
    pub fn new(
        payer: Arc<Keypair>,
        sender: Arc<JitoSender>,
        tpu_sender: Option<Arc<TpuSender>>,
        tg: TelegramBot,
        strategy_logger: Option<Arc<strategy_logger::StrategyLogger>>,
    ) -> Self {
        let snipe_lamports = std::env::var("SNIPE_SOL_AMOUNT")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .map(|sol| (sol * 1_000_000_000.0) as u64)
            .unwrap_or(DEFAULT_SNIPE_LAMPORTS);

        let min_pool_sol = std::env::var("MIN_POOL_SOL_LAMPORTS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MIN_POOL_SOL);

        info!(
            snipe_sol = snipe_lamports as f64 / 1e9,
            min_pool_sol = min_pool_sol as f64 / 1e9,
            "GraduationSniper initialized"
        );

        Self {
            payer,
            sender,
            tpu_sender,
            tg,
            snipe_lamports,
            min_pool_sol,
            sniped_pools: DashMap::new(),
            strategy_logger,
        }
    }

    /// Send a Telegram notification that a graduation/migration was detected.
    pub async fn notify_graduation_detected(
        &self,
        token_mint: &Pubkey,
        pump_pool: &Pubkey,
        slot: u64,
        source: &str,
    ) {
        self.tg
            .alert_graduation_detected(
                &token_mint.to_string(),
                &pump_pool.to_string(),
                slot,
                source,
            )
            .await;
    }

    /// Execute a graduation snipe.
    ///
    /// `token_mint` — the newly graduated token.
    /// `pump_pool`  — the PumpSwap pool pubkey (from SpySignal::Graduation).
    /// `blockhash`  — cached recent blockhash (from sim-server, 0 RPC latency).
    /// `slot`       — current slot for TPU leader routing.
    /// `source`     — launchpad source ("PumpSwap", "LaunchLab (BONK.fun)", "Moonshot").
    pub async fn snipe(
        &self,
        token_mint: Pubkey,
        pump_pool: Pubkey,
        blockhash: Hash,
        slot: u64,
        dry_run: bool,
        creator: Option<Pubkey>,
        pool_base_mint: Option<Pubkey>,
        source: &str,
    ) -> Result<()> {
        let t0 = Instant::now();

        // Dedup: skip if we already sniped this pool recently.
        if let Some(last) = self.sniped_pools.get(&pump_pool) {
            if last.elapsed().as_secs() < POOL_COOLDOWN_SECS {
                debug!(pool = %pump_pool, "SNIPER: pool already sniped, skipping");
                return Ok(());
            }
        }
        self.sniped_pools.insert(pump_pool, Instant::now());

        // GC old entries every 100 snipes.
        if self.sniped_pools.len() > 100 {
            self.sniped_pools.retain(|_, v| v.elapsed().as_secs() < POOL_COOLDOWN_SECS);
        }

        // Dispatch by source launchpad.
        match source {
            "PumpSwap" => {
                // PumpSwap graduation — existing flow.
            }
            "LaunchLab (BONK.fun)" => {
                // LaunchLab migrates to Raydium AMM V4 or CPSwap.
                info!(token = %token_mint, pool = %pump_pool, "SNIPER: LaunchLab graduation → Raydium AMM V4 buy");
                return self.snipe_raydium_v4(token_mint, pump_pool, blockhash, slot, dry_run).await;
            }
            "Moonshot" => {
                // Moonshot: pool destination varies, skip for now (needs post-migration discovery).
                info!(token = %token_mint, pool = %pump_pool, "SNIPER: Moonshot graduation detected — pool discovery needed, skipping");
                return Ok(());
            }
            _ => {
                warn!(source, "SNIPER: unknown graduation source, skipping");
                return Ok(());
            }
        }

        // ---- PumpSwap flow ----

        // Anti-scam: check mint for freeze authority, mint authority, and honeypot indicators.
        if let Some(reason) = check_token_safety(&token_mint).await {
            info!(token = %token_mint, reason, "SNIPER: token failed safety check, skipping");
            self.tg.alert_sniper_safety_skip(
                &token_mint.to_string(), &reason, 0.0,
            ).await;
            // Log safety rejection to PG
            if let Some(ref logger) = self.strategy_logger {
                logger.log_sniper(strategy_logger::types::LogSniperEvent {
                    slot,
                    token_mint: token_mint.to_string(),
                    pool: pump_pool.to_string(),
                    source: source.to_string(),
                    safety_passed: false,
                    safety_reason: Some(reason),
                    pool_sol_reserve: None,
                    snipe_lamports: self.snipe_lamports,
                    tx_signature: None,
                    build_ms: None,
                    send_ms: None,
                });
            }
            return Ok(());
        }

        // Detect token program (must succeed — mint exists before pool).
        let token_tp = detect_token_program(&token_mint).await;

        // Try to read pool state for accurate vault addresses.
        // If pool isn't created yet (we're ahead of block confirmation), derive vaults.
        let pool_state = match read_pool_state(&pump_pool).await {
            Some(ps) => ps,
            None => {
                // Derive vault addresses from graduation event data.
                let wsol_mint: Pubkey = WSOL_MINT.parse().unwrap();
                let spl_token_program: Pubkey = TOKEN_PROGRAM.parse().unwrap();
                let assoc_program: Pubkey = ASSOC_TOKEN_PROGRAM.parse().unwrap();
                let wsol_is_base = pool_base_mint.map_or(false, |bm| bm == wsol_mint);
                let (base_mint, quote_mint, base_tp, quote_tp) = if wsol_is_base {
                    (wsol_mint, token_mint, spl_token_program, token_tp)
                } else {
                    (token_mint, wsol_mint, token_tp, spl_token_program)
                };
                info!(pool = %pump_pool, "SNIPER: pool not yet on-chain, deriving vaults");
                PoolState {
                    base_mint,
                    quote_mint,
                    pool_base_vault: ata(&pump_pool, &base_mint, &base_tp, &assoc_program),
                    pool_quote_vault: ata(&pump_pool, &quote_mint, &quote_tp, &assoc_program),
                    coin_creator: Pubkey::default(), // PumpFun graduation pools
                }
            }
        };

        // Compute expected token output from pool reserves.
        let wsol_mint: Pubkey = WSOL_MINT.parse().unwrap();
        let wsol_is_base = pool_state.base_mint == wsol_mint;

        // Liquidity check: PumpFun graduation deposits ~85 SOL into the pool.
        // When pool_wsol_reserve == 0, the graduation TX hasn't landed yet — this is
        // expected for fresh detections via shreds. Send blind (the graduation TX will
        // land before or in the same slot as our snipe, providing ~85 SOL liquidity).
        // Only skip pools with known low liquidity (>0 but <50 SOL).
        // Poll pool vault for liquidity — don't send blind on vault=0.
        // Wait up to 3 attempts (500ms each) for graduation TX to land and fund the pool.
        let wsol_vault_key = if wsol_is_base { &pool_state.pool_base_vault } else { &pool_state.pool_quote_vault };
        let mut pool_wsol_reserve = 0u64;
        for poll in 0..3 {
            pool_wsol_reserve = read_vault_balance(wsol_vault_key).await;
            if pool_wsol_reserve > 0 {
                info!(pool = %pump_pool, token = %token_mint, pool_sol = pool_wsol_reserve as f64 / 1e9,
                    poll, "SNIPER: pool has liquidity");
                break;
            }
            if poll < 2 {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        if pool_wsol_reserve == 0 {
            info!(pool = %pump_pool, token = %token_mint,
                "SNIPER: pool still empty after 3 polls (1.5s), skipping — too late to compete");
            return Ok(());
        }
        if pool_wsol_reserve < self.min_pool_sol {
            let pool_sol = pool_wsol_reserve as f64 / 1e9;
            info!(
                pool = %pump_pool,
                token = %token_mint,
                pool_wsol = pool_sol,
                min_required = self.min_pool_sol as f64 / 1e9,
                "SNIPER: pool liquidity too low, skipping"
            );
            self.tg.alert_sniper_safety_skip(
                &token_mint.to_string(),
                &format!("liquidity_too_low={:.1}SOL (min={})", pool_sol, self.min_pool_sol / 1_000_000_000),
                pool_sol,
            ).await;
            if let Some(ref logger) = self.strategy_logger {
                logger.log_sniper(strategy_logger::types::LogSniperEvent {
                    slot,
                    token_mint: token_mint.to_string(),
                    pool: pump_pool.to_string(),
                    source: source.to_string(),
                    safety_passed: false,
                    safety_reason: Some(format!("liquidity_too_low={:.1}SOL", pool_sol)),
                    pool_sol_reserve: Some(pool_wsol_reserve),
                    snipe_lamports: self.snipe_lamports,
                    tx_signature: None,
                    build_ms: None,
                    send_ms: None,
                });
            }
            return Ok(());
        }

        let expected_token_out = compute_token_output(&pool_state, self.snipe_lamports, wsol_is_base).await;

        let tx = self.build_buy_tx(token_mint, pump_pool, blockhash, &token_tp, pool_base_mint, &pool_state, expected_token_out)?;

        // Preflight: size check only (no RPC, ~0µs).
        let tx_bytes = serialize(&tx).context("serialize snipe tx")?;
        if tx_bytes.len() > MAX_TX_SIZE {
            warn!(
                size = tx_bytes.len(),
                pool = %pump_pool,
                "snipe TX too large, dropping"
            );
            return Ok(());
        }

        let build_ms = t0.elapsed().as_millis();

        if dry_run {
            info!(
                pool = %pump_pool,
                token = %token_mint,
                sol = self.snipe_lamports as f64 / 1e9,
                build_ms,
                tx_size = tx_bytes.len(),
                "SNIPER DRY RUN – would send graduation snipe"
            );
            return Ok(());
        }

        // Simulate buy TX before sending — verify it will succeed on-chain.
        // Adds ~80-150ms latency but prevents wasting SOL on failed TXs.
        let sim_rpc = std::env::var("SIM_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into());
        match simulate_tx(&sim_rpc, &tx).await {
            SimResult::Success => {
                info!(pool = %pump_pool, token = %token_mint, "SNIPER: simulation PASSED");
            }
            SimResult::Failed(err) => {
                info!(pool = %pump_pool, token = %token_mint, error = %err,
                    "SNIPER: simulation FAILED — NOT sending (saved {:.3} SOL)",
                    self.snipe_lamports as f64 / 1e9);
                return Ok(());
            }
            SimResult::RpcError(err) => {
                // RPC error — proceed with caution (send anyway, atomic TX will revert if bad).
                warn!(pool = %pump_pool, token = %token_mint, error = %err,
                    "SNIPER: simulation RPC error — sending anyway (atomic)");
            }
        }

        // Parallel send: TPU fanout + Jito multi-region bundle + Jito sendTransaction + Agave local.
        let tx_for_tpu = tx.clone();
        let tx_for_send_tx = tx.clone();
        let tx_for_agave = tx.clone();
        let tpu = self.tpu_sender.clone();
        let sender_for_tx = Arc::clone(&self.sender);

        let tpu_handle = std::thread::spawn(move || -> usize {
            if let Some(tpu_sender) = tpu {
                tpu_sender.send(&tx_for_tpu, slot)
            } else {
                0
            }
        });

        // Fire all methods concurrently: Jito bundle + Jito sendTx + Agave local RPC.
        let bundle_txs = [tx];
        let (bundle_result, _send_tx_result, agave_result) = tokio::join!(
            self.sender.send_bundle_multi_region(&bundle_txs),
            sender_for_tx.send_transaction_multi_region(&tx_for_send_tx),
            agave_send_tx(&tx_for_agave),
        );
        match &agave_result {
            Ok(sig) => info!(sig = %sig, "SNIPER Agave local sendTransaction OK"),
            Err(e) => warn!(error = %e, "SNIPER Agave local send failed"),
        }
        let tpu_sent = tpu_handle.join().unwrap_or(0);

        let send_ms = t0.elapsed().as_millis();

        match &bundle_result {
            Ok(bundle_id) => {
                info!(
                    pool = %pump_pool,
                    token = %token_mint,
                    bundle_id = %bundle_id,
                    tpu_sent,
                    sol = self.snipe_lamports as f64 / 1e9,
                    build_ms,
                    send_ms,
                    "SNIPER bundle submitted (Jito + TPU fanout)"
                );
                // Telegram: snipe sent successfully
                self.tg
                    .alert_graduation_arb_success(
                        &token_mint.to_string(),
                        &pump_pool.to_string(),
                        self.snipe_lamports as f64 / 1e9,
                        &bundle_id,
                        tpu_sent,
                        build_ms,
                        send_ms,
                    )
                    .await;
            }
            Err(ref e) => {
                let e_str = e.to_string();
                if e_str.contains("429") || e_str.to_lowercase().contains("too many requests") {
                    debug!(
                        pool = %pump_pool,
                        error = %e,
                        tpu_sent,
                        "SNIPER Jito 429 rate limited (TPU may have succeeded)"
                    );
                } else {
                    warn!(
                        pool = %pump_pool,
                        error = %e,
                        tpu_sent,
                        "SNIPER Jito bundle failed (TPU may have succeeded)"
                    );
                }
            }
        }

        // Log sniper event to PG
        if let Some(ref logger) = self.strategy_logger {
            let tx_sig = bundle_result.as_ref().ok().map(|s| s.clone());
            logger.log_sniper(strategy_logger::types::LogSniperEvent {
                slot,
                token_mint: token_mint.to_string(),
                pool: pump_pool.to_string(),
                source: source.to_string(),
                safety_passed: true,
                safety_reason: None,
                pool_sol_reserve: Some(pool_wsol_reserve),
                snipe_lamports: self.snipe_lamports,
                tx_signature: tx_sig,
                build_ms: Some(build_ms as u64),
                send_ms: Some(send_ms as u64),
            });
        }

        // Auto-sell: wait for buy to land, then sell tokens back to SOL.
        if !dry_run {
            let payer_arc = Arc::clone(&self.payer);
            let sender_arc = Arc::clone(&self.sender);
            let tpu_arc = self.tpu_sender.clone();
            let tg_clone = self.tg.clone();
            let token_tp_clone = token_tp;
            let pool_state_base = pool_state.base_mint;
            let pool_state_quote = pool_state.quote_mint;
            let pool_base_vault = pool_state.pool_base_vault;
            let pool_quote_vault = pool_state.pool_quote_vault;
            let pool_creator = pool_state.coin_creator;
            let snipe_cost = self.snipe_lamports;
            let sell_logger = self.strategy_logger.clone();
            tokio::spawn(async move {
                if let Err(e) = auto_sell(
                    payer_arc, sender_arc, tpu_arc, tg_clone,
                    token_mint, pump_pool, token_tp_clone,
                    wsol_is_base, pool_state_base, pool_state_quote,
                    pool_base_vault, pool_quote_vault, pool_creator,
                    slot, snipe_cost, sell_logger,
                ).await {
                    warn!(error = %e, token = %token_mint, "SNIPER auto-sell failed");
                }
            });
        }

        Ok(())
    }

    fn build_buy_tx(
        &self,
        token_mint: Pubkey,
        pump_pool: Pubkey,
        blockhash: Hash,
        token_tp: &Pubkey,
        pool_base_mint: Option<Pubkey>,
        pool_state: &PoolState,
        expected_token_out: u64,
    ) -> Result<VersionedTransaction> {
        let pumpswap_program: Pubkey = PUMPSWAP_PROGRAM.parse().unwrap();
        let wsol_mint: Pubkey = WSOL_MINT.parse().unwrap();
        let spl_token_program: Pubkey = TOKEN_PROGRAM.parse().unwrap();
        let assoc_program: Pubkey = ASSOC_TOKEN_PROGRAM.parse().unwrap();
        let system_program: Pubkey = SYSTEM_PROGRAM.parse().unwrap();
        let fee_recipient: Pubkey = PROTOCOL_FEE_RECIPIENT.parse().unwrap();
        let fee_program: Pubkey = FEE_PROGRAM.parse().unwrap();
        let payer = self.payer.pubkey();

        // Use mints and vaults from on-chain pool state (guaranteed correct).
        let base_mint = pool_state.base_mint;
        let quote_mint = pool_state.quote_mint;
        let pool_base_vault = pool_state.pool_base_vault;
        let pool_quote_vault = pool_state.pool_quote_vault;
        let coin_creator = pool_state.coin_creator;

        let wsol_is_base = base_mint == wsol_mint;
        let (base_tp, quote_tp) = if wsol_is_base {
            info!(token = %token_mint, "SNIPER: pool has WSOL as base, token as quote");
            (spl_token_program, *token_tp)
        } else {
            (*token_tp, spl_token_program)
        };

        info!(
            token = %token_mint,
            %base_mint,
            %quote_mint,
            %pool_base_vault,
            %pool_quote_vault,
            %coin_creator,
            "SNIPER build_buy_tx: layout resolved from pool state"
        );

        // Derive deterministic addresses.
        let global_config = Pubkey::find_program_address(
            &[b"global_config"],
            &pumpswap_program,
        ).0;

        let event_authority = Pubkey::find_program_address(
            &[b"__event_authority"],
            &pumpswap_program,
        ).0;

        let global_volume_acc = Pubkey::find_program_address(
            &[b"global_volume_accumulator"],
            &pumpswap_program,
        ).0;
        let user_volume_acc = Pubkey::find_program_address(
            &[b"user_volume_accumulator", payer.as_ref()],
            &pumpswap_program,
        ).0;

        let fee_config = Pubkey::find_program_address(
            &[b"fee_config", pumpswap_program.as_ref()],
            &fee_program,
        ).0;

        // Creator vault authority PDA from pool's coin_creator field (NOT the TX signer).
        let creator_vault_authority = Pubkey::find_program_address(
            &[b"creator_vault", coin_creator.as_ref()],
            &pumpswap_program,
        ).0;
        // Creator vault ATA uses quote_mint (fees collected in quote token).
        let creator_vault_ata = ata(&creator_vault_authority, &quote_mint, &quote_tp, &assoc_program);

        // User ATAs.
        let user_base = ata(&payer, &base_mint, &base_tp, &assoc_program);
        let user_quote = ata(&payer, &quote_mint, &quote_tp, &assoc_program);
        let fee_recipient_quote_ata = ata(&fee_recipient, &quote_mint, &quote_tp, &assoc_program);

        // expected_token_out: estimated tokens we'll receive for our WSOL.
        // When WSOL is base → SELL: pay WSOL (base), receive token (quote).
        //   data: [sell_disc][base_amount_in][min_quote_amount_out]
        // When WSOL is quote → BUY: receive token (base), pay WSOL (quote).
        //   data: [buy_disc][base_amount_out][max_quote_amount_in]
        let min_token_out = expected_token_out * (10_000 - SLIPPAGE_BPS) / 10_000;
        let data = if wsol_is_base {
            // SELL: spend WSOL (base) → get token (quote)
            info!(
                base_amount_in = self.snipe_lamports,
                expected_token_out,
                min_token_out,
                "SNIPER sell: WSOL→token"
            );
            let mut d = SELL_DISCRIMINATOR.to_vec();
            d.extend_from_slice(&self.snipe_lamports.to_le_bytes());
            d.extend_from_slice(&min_token_out.to_le_bytes());
            d
        } else {
            // BUY: receive token (base), pay WSOL (quote)
            let max_quote = self.snipe_lamports + (self.snipe_lamports * SLIPPAGE_BPS / 10_000);
            info!(
                expected_token_out,
                min_token_out,
                max_quote,
                "SNIPER buy: WSOL→token"
            );
            let mut d = BUY_DISCRIMINATOR.to_vec();
            d.extend_from_slice(&min_token_out.to_le_bytes());
            d.extend_from_slice(&max_quote.to_le_bytes());
            d
        };

        // Accounts 0-18 are shared between buy and sell.
        // Buy has 23 accounts (adds global_volume_acc + user_volume_acc at 19-20).
        // Sell has 21 accounts (fee_config/fee_program at 19-20 directly).
        let mut accounts = vec![
            AccountMeta::new(pump_pool, false),                     // 0
            AccountMeta::new(payer, true),                          // 1 user (signer)
            AccountMeta::new_readonly(global_config, false),        // 2
            AccountMeta::new_readonly(base_mint, false),            // 3
            AccountMeta::new_readonly(quote_mint, false),           // 4
            AccountMeta::new(user_base, false),                     // 5 user_base_ata
            AccountMeta::new(user_quote, false),                    // 6 user_quote_ata
            AccountMeta::new(pool_base_vault, false),               // 7 pool_base_vault
            AccountMeta::new(pool_quote_vault, false),              // 8 pool_quote_vault
            AccountMeta::new_readonly(fee_recipient, false),        // 9
            AccountMeta::new(fee_recipient_quote_ata, false),       // 10
            AccountMeta::new_readonly(base_tp, false),              // 11 base_token_program
            AccountMeta::new_readonly(quote_tp, false),             // 12 quote_token_program
            AccountMeta::new_readonly(system_program, false),       // 13
            AccountMeta::new_readonly(assoc_program, false),        // 14
            AccountMeta::new_readonly(event_authority, false),      // 15
            AccountMeta::new_readonly(pumpswap_program, false),     // 16
            AccountMeta::new(creator_vault_ata, false),             // 17 creator vault quote ATA
            AccountMeta::new_readonly(creator_vault_authority, false), // 18
        ];
        if !wsol_is_base {
            // Buy instruction: add volume accumulators before fee accounts
            accounts.push(AccountMeta::new_readonly(global_volume_acc, false)); // 19
            accounts.push(AccountMeta::new(user_volume_acc, false));            // 20
        }
        // fee_config and fee_program are always last
        accounts.push(AccountMeta::new_readonly(fee_config, false));
        accounts.push(AccountMeta::new_readonly(fee_program, false));

        let swap_ix = Instruction {
            program_id: pumpswap_program,
            accounts,
            data,
        };

        // Build instruction list.
        let mut ixs = Vec::new();

        // Compute budget: priority fee for TPU/validator scheduling.
        let priority_fee = std::env::var("PRIORITY_FEE_MICRO_LAMPORTS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(100_000); // sniper uses higher priority than arb
        ixs.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(250_000));
        ixs.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(priority_fee));

        // 1) Create the user's TOKEN ATA (the non-WSOL side) — idempotent.
        let (token_ata, token_ata_mint, token_ata_program) = if base_mint == wsol_mint {
            (user_quote, quote_mint, quote_tp)
        } else {
            (user_base, base_mint, base_tp)
        };
        ixs.push(Instruction {
            program_id: assoc_program,
            accounts: vec![
                AccountMeta::new(payer, true),
                AccountMeta::new(token_ata, false),
                AccountMeta::new_readonly(payer, false),
                AccountMeta::new_readonly(token_ata_mint, false),
                AccountMeta::new_readonly(system_program, false),
                AccountMeta::new_readonly(token_ata_program, false),
            ],
            data: vec![1u8], // idempotent create
        });

        // 2) Wrap SOL into the user's WSOL ATA so the swap can TransferChecked from it.
        //    Steps: create WSOL ATA (idempotent) → system transfer → syncNative.
        let wsol_ata = if wsol_is_base { user_base } else { user_quote };
        let wrap_amount = if wsol_is_base {
            self.snipe_lamports // sell: user pays base (WSOL)
        } else {
            self.snipe_lamports + (self.snipe_lamports * SLIPPAGE_BPS / 10_000) // buy: max_quote
        };

        // Create WSOL ATA (idempotent)
        ixs.push(Instruction {
            program_id: assoc_program,
            accounts: vec![
                AccountMeta::new(payer, true),
                AccountMeta::new(wsol_ata, false),
                AccountMeta::new_readonly(payer, false),
                AccountMeta::new_readonly(wsol_mint, false),
                AccountMeta::new_readonly(system_program, false),
                AccountMeta::new_readonly(spl_token_program, false), // WSOL always SPL Token
            ],
            data: vec![1u8], // idempotent create
        });

        // Transfer native SOL into the WSOL ATA
        ixs.push(solana_sdk::system_instruction::transfer(
            &payer,
            &wsol_ata,
            wrap_amount,
        ));

        // SyncNative: tells SPL Token to update the WSOL ATA balance from its lamports
        ixs.push(Instruction {
            program_id: spl_token_program,
            accounts: vec![AccountMeta::new(wsol_ata, false)],
            data: vec![17u8], // SyncNative instruction index
        });

        // 3) The swap instruction
        ixs.push(swap_ix);

        // 4) Close the WSOL ATA to reclaim rent + leftover SOL back to payer
        ixs.push(Instruction {
            program_id: spl_token_program,
            accounts: vec![
                AccountMeta::new(wsol_ata, false),    // account to close
                AccountMeta::new(payer, false),        // destination for lamports
                AccountMeta::new_readonly(payer, true), // authority (owner)
            ],
            data: vec![9u8], // CloseAccount instruction index
        });

        // 5) Jito tip instruction (mandatory — without it Jito rejects the bundle).
        let tip_ix = self.sender.tip_instruction(&payer);
        ixs.push(tip_ix);

        let msg = MessageV0::try_compile(
            &payer,
            &ixs,
            &[],
            blockhash,
        )
        .context("compile snipe message")?;

        let tx = VersionedTransaction::try_new(
            solana_sdk::message::VersionedMessage::V0(msg),
            &[self.payer.as_ref()],
        )
        .context("sign snipe tx")?;

        Ok(tx)
    }

    /// Snipe a LaunchLab graduation that migrates to Raydium AMM V4.
    /// Reads pool state from RPC, builds a Raydium V4 SwapBaseIn IX.
    async fn snipe_raydium_v4(
        &self,
        token_mint: Pubkey,
        amm_pool: Pubkey,
        blockhash: Hash,
        slot: u64,
        dry_run: bool,
    ) -> Result<()> {
        let t0 = Instant::now();
        let raydium_v4: Pubkey = RAYDIUM_AMM_V4.parse().unwrap();
        let wsol_mint: Pubkey = WSOL_MINT.parse().unwrap();
        let spl_token_program: Pubkey = TOKEN_PROGRAM.parse().unwrap();
        let assoc_program: Pubkey = ASSOC_TOKEN_PROGRAM.parse().unwrap();
        let system_program: Pubkey = SYSTEM_PROGRAM.parse().unwrap();
        let payer = self.payer.pubkey();

        // Read Raydium AMM pool account to get vaults, market accounts, etc.
        let amm_state = match read_raydium_v4_state(&amm_pool).await {
            Some(s) => s,
            None => {
                info!(pool = %amm_pool, "SNIPER-V4: pool not on-chain yet, skipping");
                return Ok(());
            }
        };

        // Determine swap direction: we always buy the non-WSOL token.
        let wsol_is_coin = amm_state.coin_mint == wsol_mint;
        let token_tp = detect_token_program(&token_mint).await;

        // Liquidity check.
        let wsol_vault = if wsol_is_coin { &amm_state.coin_vault } else { &amm_state.pc_vault };
        let wsol_reserve = read_vault_balance(wsol_vault).await;
        if wsol_reserve < self.min_pool_sol {
            info!(pool = %amm_pool, wsol_reserve, "SNIPER-V4: pool liquidity too low");
            return Ok(());
        }

        // Build SwapBaseIn instruction data: [9, amount_in(u64 LE), min_out(u64 LE)]
        let amount_in = self.snipe_lamports;
        let min_out = 1u64; // Minimal — rely on slippage from speed advantage.
        let mut data = vec![9u8]; // SwapBaseIn discriminator
        data.extend_from_slice(&amount_in.to_le_bytes());
        data.extend_from_slice(&min_out.to_le_bytes());

        // Raydium AMM V4 SwapBaseIn account layout (17 accounts):
        let accounts = vec![
            AccountMeta::new_readonly(spl_token_program, false),   // 0: token_program
            AccountMeta::new(amm_pool, false),                     // 1: amm
            AccountMeta::new_readonly(amm_state.authority, false), // 2: amm_authority
            AccountMeta::new(amm_state.open_orders, false),        // 3: amm_open_orders
            AccountMeta::new(amm_state.target_orders, false),      // 4: amm_target_orders
            AccountMeta::new(amm_state.coin_vault, false),         // 5: pool_coin_token_account
            AccountMeta::new(amm_state.pc_vault, false),           // 6: pool_pc_token_account
            AccountMeta::new_readonly(amm_state.market_program, false), // 7: serum_program
            AccountMeta::new(amm_state.market_id, false),          // 8: serum_market
            AccountMeta::new(amm_state.market_bids, false),        // 9: serum_bids
            AccountMeta::new(amm_state.market_asks, false),        // 10: serum_asks
            AccountMeta::new(amm_state.market_event_queue, false), // 11: serum_event_queue
            AccountMeta::new(amm_state.market_base_vault, false),  // 12: serum_coin_vault
            AccountMeta::new(amm_state.market_quote_vault, false), // 13: serum_pc_vault
            AccountMeta::new_readonly(amm_state.market_vault_signer, false), // 14: serum_vault_signer
            // 15: user_source_token_account — our WSOL ATA
            // 16: user_destination_token_account — our token ATA
            // 17: user_source_owner — payer (signer)
        ];

        // User source/dest depends on direction.
        let user_wsol_ata = ata(&payer, &wsol_mint, &spl_token_program, &assoc_program);
        let user_token_ata = ata(&payer, &token_mint, &token_tp, &assoc_program);
        let (user_source, user_dest) = if wsol_is_coin {
            (user_wsol_ata, user_token_ata) // WSOL is coin → buy token (pc)
        } else {
            (user_wsol_ata, user_token_ata) // WSOL is pc → buy token (coin)
        };

        let mut full_accounts = accounts;
        full_accounts.push(AccountMeta::new(user_source, false));   // 15
        full_accounts.push(AccountMeta::new(user_dest, false));     // 16
        full_accounts.push(AccountMeta::new_readonly(payer, true)); // 17

        let swap_ix = Instruction {
            program_id: raydium_v4,
            accounts: full_accounts,
            data,
        };

        // Build instruction list.
        let mut ixs = Vec::new();

        // Compute budget.
        let priority_fee = std::env::var("PRIORITY_FEE_MICRO_LAMPORTS")
            .ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(100_000);
        ixs.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(300_000));
        ixs.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(priority_fee));

        // Create token ATA (idempotent).
        ixs.push(Instruction {
            program_id: assoc_program,
            accounts: vec![
                AccountMeta::new(payer, true),
                AccountMeta::new(user_token_ata, false),
                AccountMeta::new_readonly(payer, false),
                AccountMeta::new_readonly(token_mint, false),
                AccountMeta::new_readonly(system_program, false),
                AccountMeta::new_readonly(token_tp, false),
            ],
            data: vec![1u8],
        });

        // Wrap SOL: create WSOL ATA + transfer + sync.
        ixs.push(Instruction {
            program_id: assoc_program,
            accounts: vec![
                AccountMeta::new(payer, true),
                AccountMeta::new(user_wsol_ata, false),
                AccountMeta::new_readonly(payer, false),
                AccountMeta::new_readonly(wsol_mint, false),
                AccountMeta::new_readonly(system_program, false),
                AccountMeta::new_readonly(spl_token_program, false),
            ],
            data: vec![1u8],
        });
        ixs.push(solana_sdk::system_instruction::transfer(&payer, &user_wsol_ata, amount_in));
        ixs.push(Instruction {
            program_id: spl_token_program,
            accounts: vec![AccountMeta::new(user_wsol_ata, false)],
            data: vec![17u8], // SyncNative
        });

        // Swap.
        ixs.push(swap_ix);

        // Close WSOL ATA.
        ixs.push(Instruction {
            program_id: spl_token_program,
            accounts: vec![
                AccountMeta::new(user_wsol_ata, false),
                AccountMeta::new(payer, false),
                AccountMeta::new_readonly(payer, true),
            ],
            data: vec![9u8], // CloseAccount
        });

        // Jito tip.
        ixs.push(self.sender.tip_instruction(&payer));

        let msg = MessageV0::try_compile(&payer, &ixs, &[], blockhash)
            .context("compile raydium v4 snipe message")?;
        let tx = VersionedTransaction::try_new(
            solana_sdk::message::VersionedMessage::V0(msg),
            &[self.payer.as_ref()],
        ).context("sign raydium v4 snipe tx")?;

        let tx_bytes = serialize(&tx).context("serialize v4 snipe tx")?;
        if tx_bytes.len() > MAX_TX_SIZE {
            warn!(size = tx_bytes.len(), "SNIPER-V4: TX too large");
            return Ok(());
        }

        let build_ms = t0.elapsed().as_millis();

        if dry_run {
            info!(pool = %amm_pool, token = %token_mint, build_ms, "SNIPER-V4 DRY RUN");
            return Ok(());
        }

        // Send via TPU + Jito (same 2-phase as PumpSwap sniper).
        let tx_for_tpu = tx.clone();
        let tpu = self.tpu_sender.clone();
        let tpu_handle = std::thread::spawn(move || -> usize {
            if let Some(tpu_sender) = tpu {
                tpu_sender.send(&tx_for_tpu, slot)
            } else { 0 }
        });

        let bundle_txs = [tx];
        let bundle_result = self.sender.send_bundle_multi_region(&bundle_txs).await;
        let tpu_sent = tpu_handle.join().unwrap_or(0);
        let send_ms = t0.elapsed().as_millis();

        match &bundle_result {
            Ok(bid) => info!(pool = %amm_pool, token = %token_mint, bundle_id = %bid, tpu_sent, build_ms, send_ms, "SNIPER-V4 bundle submitted"),
            Err(e) => {
                if !e.to_string().contains("429") {
                    warn!(error = %e, tpu_sent, "SNIPER-V4 Jito failed (TPU may have landed)");
                }
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Raydium AMM V4 pool state reader
// ---------------------------------------------------------------------------

const RAYDIUM_AMM_V4: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";

struct RaydiumV4State {
    authority: Pubkey,
    coin_mint: Pubkey,
    pc_mint: Pubkey,
    coin_vault: Pubkey,
    pc_vault: Pubkey,
    open_orders: Pubkey,
    target_orders: Pubkey,
    market_id: Pubkey,
    market_program: Pubkey,
    market_event_queue: Pubkey,
    market_bids: Pubkey,
    market_asks: Pubkey,
    market_base_vault: Pubkey,
    market_quote_vault: Pubkey,
    market_vault_signer: Pubkey,
}

/// Read Raydium AMM V4 pool state from RPC.
/// AMM V4 account layout (Borsh, no Anchor discriminator):
///   offset 0:   status (u64) 8 bytes
///   offset 8:   nonce (u64) 8 bytes
///   offset 16:  max_order (u64) 8 bytes
///   offset 24:  depth (u64) 8 bytes
///   ...many u64/u128 fields...
///   The key accounts are at known offsets from pool_hydrator.rs.
async fn read_raydium_v4_state(pool: &Pubkey) -> Option<RaydiumV4State> {
    let url = std::env::var("SIM_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap_or_default();

    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getAccountInfo",
        "params": [pool.to_string(), {"encoding": "base64"}]
    });

    let resp = client.post(&url).json(&body).send().await.ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;

    if json["result"]["value"].is_null() {
        return None;
    }

    let data_b64 = json["result"]["value"]["data"][0].as_str()?;
    let data = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64).ok()?;

    // Raydium AMM V4 account layout:
    // Offsets from empirical analysis and pool_hydrator:
    //   8:  nonce (u64)
    //  16:  max_order (u64)
    //  24:  depth (u64)
    //  32:  base_decimal (u64)
    //  40:  quote_decimal (u64)
    //  48:  state (u64)
    //  56:  reset_flag (u64)
    //  64:  min_size (u64)
    //  72:  vol_max_cut_ratio (u64)
    //  80:  amount_wave_ratio (u64)
    //  88:  coin_lot_size (u64)
    //  96:  pc_lot_size (u64)
    // 104:  min_price_multiplier (u64)
    // 112:  max_price_multiplier (u64)
    // 120:  sys_decimal_value (u64)
    //  ...fees...
    // 264:  min_separate_numerator (u64)
    // ...more fees up to about offset 360...
    // Key pubkeys start at offset 368:
    //  368: pool_coin_token_account (vault_a) [32]
    //  400: pool_pc_token_account (vault_b) [32]
    //  432: coin_mint [32]
    //  464: pc_mint [32]
    //  496: lp_mint [32]
    //  528: open_orders [32]
    //  560: market_id [32]
    //  592: market_program [32]
    //  624: target_orders [32]
    //  656: withdraw_queue [32]  (deprecated)
    //  688: lp_vault [32]       (deprecated)
    //  720: authority [32]
    // Minimum expected length: 752 bytes
    if data.len() < 752 {
        warn!(pool = %pool, len = data.len(), "SNIPER-V4: pool data too short");
        return None;
    }

    let pk = |off: usize| -> Pubkey {
        Pubkey::from(<[u8; 32]>::try_from(&data[off..off + 32]).unwrap_or([0u8; 32]))
    };

    let coin_vault = pk(368);
    let pc_vault = pk(400);
    let coin_mint = pk(432);
    let pc_mint = pk(464);
    let open_orders = pk(528);
    let market_id = pk(560);
    let market_program = pk(592);
    let target_orders = pk(624);
    let authority = pk(720);

    // For market accounts (bids, asks, event_queue, base_vault, quote_vault, vault_signer),
    // we need to read the Serum/OpenBook market account. We'll derive them from the pool
    // cache if available, or read market state directly.
    let market_state = read_serum_market_state(&market_id, &url).await;

    Some(RaydiumV4State {
        authority,
        coin_mint,
        pc_mint,
        coin_vault,
        pc_vault,
        open_orders,
        target_orders,
        market_id,
        market_program,
        market_event_queue: market_state.as_ref().map_or(Pubkey::default(), |m| m.event_queue),
        market_bids: market_state.as_ref().map_or(Pubkey::default(), |m| m.bids),
        market_asks: market_state.as_ref().map_or(Pubkey::default(), |m| m.asks),
        market_base_vault: market_state.as_ref().map_or(Pubkey::default(), |m| m.base_vault),
        market_quote_vault: market_state.as_ref().map_or(Pubkey::default(), |m| m.quote_vault),
        market_vault_signer: market_state.as_ref().map_or(Pubkey::default(), |m| m.vault_signer),
    })
}

struct SerumMarketState {
    bids: Pubkey,
    asks: Pubkey,
    event_queue: Pubkey,
    base_vault: Pubkey,
    quote_vault: Pubkey,
    vault_signer: Pubkey,
}

/// Read Serum/OpenBook market account for bids, asks, event_queue, vaults.
async fn read_serum_market_state(market: &Pubkey, rpc_url: &str) -> Option<SerumMarketState> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap_or_default();

    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getAccountInfo",
        "params": [market.to_string(), {"encoding": "base64"}]
    });

    let resp = client.post(rpc_url).json(&body).send().await.ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;
    let data_b64 = json["result"]["value"]["data"][0].as_str()?;
    let data = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64).ok()?;

    // Serum DEX v3 market layout:
    //   5:  account_flags [8]
    //  13:  own_address [32]
    //  45:  vault_signer_nonce [8]
    //  53:  base_mint [32]
    //  85:  quote_mint [32]
    // 117:  base_vault [32]
    // 149:  base_deposits [8]
    // 157:  base_fees [8]
    // 165:  quote_vault [32]
    // ...
    // 197:  quote_deposits [8]
    // 205:  quote_fees [8]
    // 213:  req_queue [32]
    // 245:  event_queue [32]
    // 277:  bids [32]
    // 309:  asks [32]
    if data.len() < 341 {
        return None;
    }

    let pk = |off: usize| -> Pubkey {
        Pubkey::from(<[u8; 32]>::try_from(&data[off..off + 32]).unwrap_or([0u8; 32]))
    };

    let vault_signer_nonce = u64::from_le_bytes(
        data[45..53].try_into().unwrap_or([0u8; 8])
    );
    let market_program = json["result"]["value"]["owner"].as_str()
        .and_then(|s| s.parse::<Pubkey>().ok())
        .unwrap_or_default();

    // Derive vault signer from nonce.
    let vault_signer = Pubkey::create_program_address(
        &[market.as_ref(), &vault_signer_nonce.to_le_bytes()],
        &market_program,
    ).unwrap_or_default();

    Some(SerumMarketState {
        base_vault: pk(117),
        quote_vault: pk(165),
        event_queue: pk(245),
        bids: pk(277),
        asks: pk(309),
        vault_signer,
    })
}

// ---------------------------------------------------------------------------
// ATA derivation helper
// ---------------------------------------------------------------------------

fn ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey, assoc_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        assoc_program,
    )
    .0
}

// ---------------------------------------------------------------------------
// Pool state reader
// ---------------------------------------------------------------------------

/// Parsed PumpSwap pool account state.
struct PoolState {
    base_mint: Pubkey,
    quote_mint: Pubkey,
    pool_base_vault: Pubkey,
    pool_quote_vault: Pubkey,
    coin_creator: Pubkey,
}

/// Read and parse a PumpSwap pool account from RPC.
/// Single attempt with no retry (caller falls back to derivation).
/// Returns None if the account doesn't exist, isn't owned by PumpSwap, or can't be parsed.
async fn read_pool_state(pool: &Pubkey) -> Option<PoolState> {
    let url = std::env::var("SIM_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap_or_default();

    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getAccountInfo",
        "params": [pool.to_string(), {"encoding": "base64"}]
    });

    for attempt in 0..1u32 {

        let resp = match client.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(_) => continue,
        };
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(_) => continue,
        };

        // Check if account exists
        if json["result"]["value"].is_null() {
            debug!(pool = %pool, attempt, "SNIPER: pool account not found yet, retrying");
            continue;
        }

        // Verify owner is PumpSwap program
        if let Some(owner) = json["result"]["value"]["owner"].as_str() {
            if owner != PUMPSWAP_PROGRAM {
                warn!(pool = %pool, owner, "SNIPER: pool not owned by PumpSwap, skipping");
                return None;
            }
        }

        let data_b64 = match json["result"]["value"]["data"][0].as_str() {
            Some(d) => d,
            None => continue,
        };
        let data = match base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            data_b64,
        ) {
            Ok(d) => d,
            Err(_) => continue,
        };

        // Pool struct layout (after 8-byte Anchor discriminator):
        // offset 8:  bump (1) + index (2) = 3 bytes
        // offset 11: creator (32)
        // offset 43: base_mint (32)
        // offset 75: quote_mint (32)
        // offset 107: lp_mint (32)
        // offset 139: pool_base_token_account (32)
        // offset 171: pool_quote_token_account (32)
        // offset 203: lp_supply (8)
        // offset 211: coin_creator (32)
        if data.len() < 243 {
            warn!(pool = %pool, len = data.len(), "SNIPER: pool data too short");
            return None;
        }

        let base_mint = Pubkey::from(<[u8; 32]>::try_from(&data[43..75]).ok()?);
        let quote_mint = Pubkey::from(<[u8; 32]>::try_from(&data[75..107]).ok()?);
        let pool_base_vault = Pubkey::from(<[u8; 32]>::try_from(&data[139..171]).ok()?);
        let pool_quote_vault = Pubkey::from(<[u8; 32]>::try_from(&data[171..203]).ok()?);
        let coin_creator = Pubkey::from(<[u8; 32]>::try_from(&data[211..243]).ok()?);

        // Sanity check: neither mint should be default
        let wsol: Pubkey = WSOL_MINT.parse().unwrap();
        if (base_mint == Pubkey::default() || quote_mint == Pubkey::default())
            && base_mint != wsol && quote_mint != wsol
        {
            warn!(pool = %pool, "SNIPER: pool has default mint, data may be corrupt");
            return None;
        }

        info!(
            pool = %pool,
            %base_mint,
            %quote_mint,
            %coin_creator,
            attempt,
            "SNIPER: pool state read OK"
        );

        return Some(PoolState {
            base_mint,
            quote_mint,
            pool_base_vault,
            pool_quote_vault,
            coin_creator,
        });
    }

    None
}

/// Compute expected token output from pool vault balances using XY=K.
///
/// `wsol_is_base`: true if WSOL is base_mint (SELL direction), false if quote_mint (BUY direction).
/// Returns expected token amount out (raw units).
async fn compute_token_output(pool_state: &PoolState, wsol_amount_in: u64, wsol_is_base: bool) -> u64 {
    let url = std::env::var("SIM_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap_or_default();

    // Read both vault balances in one batch.
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getMultipleAccounts",
        "params": [[
            pool_state.pool_base_vault.to_string(),
            pool_state.pool_quote_vault.to_string()
        ], {"encoding": "jsonParsed"}]
    });

    match client.post(&url).json(&body).send().await {
        Ok(resp) => {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                let vals = &json["result"]["value"];
                let base_balance = extract_token_balance(&vals[0]).unwrap_or(0);
                let quote_balance = extract_token_balance(&vals[1]).unwrap_or(0);

                if base_balance > 0 && quote_balance > 0 {
                    // XY=K: token_out = token_reserve * wsol_in / (wsol_reserve + wsol_in)
                    let (wsol_reserve, token_reserve) = if wsol_is_base {
                        (base_balance, quote_balance)
                    } else {
                        (quote_balance, base_balance)
                    };

                    let token_out = (token_reserve as u128)
                        .checked_mul(wsol_amount_in as u128)
                        .unwrap_or(0)
                        / (wsol_reserve as u128 + wsol_amount_in as u128);

                    info!(
                        wsol_reserve,
                        token_reserve,
                        wsol_in = wsol_amount_in,
                        token_out = token_out as u64,
                        wsol_is_base,
                        "SNIPER: computed token output from vault balances"
                    );
                    return token_out as u64;
                }
            }
            fallback_token_output(wsol_amount_in)
        }
        Err(e) => {
            warn!(error = %e, "SNIPER: vault balance fetch failed");
            fallback_token_output(wsol_amount_in)
        }
    }
}

// ---------------------------------------------------------------------------
// Blockhash cache client (queries sim-server HTTP)
// ---------------------------------------------------------------------------

/// Fetch the latest blockhash from sim-server (cached, <1ms latency).
/// Falls back to a zero hash if unavailable (TX will fail but won't block sniper).
pub async fn fetch_cached_blockhash(sim_rpc_url: &str) -> Hash {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(20))
        .build()
        .unwrap_or_default();

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getLatestBlockhash",
        "params": [{"commitment": "confirmed"}]
    });

    match client.post(sim_rpc_url).json(&body).send().await {
        Ok(resp) => {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                if let Some(bh_str) = json["result"]["value"]["blockhash"].as_str() {
                    if let Ok(bh) = bh_str.parse::<Hash>() {
                        return bh;
                    }
                }
            }
            warn!("sniper: could not parse blockhash from sim-server, using zero hash");
            Hash::default()
        }
        Err(e) => {
            warn!(error = %e, "sniper: blockhash fetch failed, using zero hash");
            Hash::default()
        }
    }
}

/// Send a transaction via local Agave RPC (gossip propagation, no rate limits).
async fn agave_send_tx(tx: &solana_sdk::transaction::VersionedTransaction) -> anyhow::Result<String> {
    use anyhow::Context;

    let url = std::env::var("AGAVE_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:9000".into());
    let encoded = bincode::serialize(tx).context("serialize tx for agave")?;
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &encoded);

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendTransaction",
        "params": [b64, {"encoding": "base64", "skipPreflight": true, "maxRetries": 0}]
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap_or_default();

    let resp = client.post(&url).json(&body).send().await.context("agave POST")?;
    let json: serde_json::Value = resp.json().await.context("agave parse")?;

    if let Some(err) = json.get("error") {
        anyhow::bail!("agave RPC error: {}", err);
    }

    Ok(json["result"].as_str().unwrap_or("unknown").to_string())
}

/// Check token for scam indicators: freeze authority, active mint authority, honeypot.
/// Returns Some(reason) if unsafe, None if token passes checks.
async fn check_token_safety(mint: &Pubkey) -> Option<String> {
    let url = std::env::var("SIM_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap_or_default();

    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getAccountInfo",
        "params": [mint.to_string(), {"encoding": "jsonParsed"}]
    });

    match client.post(&url).json(&body).send().await {
        Ok(resp) => {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                let info = &json["result"]["value"]["data"]["parsed"]["info"];

                // Check freeze authority — tokens with freeze authority can freeze your ATA.
                if let Some(freeze) = info["freezeAuthority"].as_str() {
                    if !freeze.is_empty() {
                        return Some(format!("freeze_authority={}", &freeze[..8.min(freeze.len())]));
                    }
                }

                // Check mint authority — active mint authority means infinite supply risk.
                if let Some(mint_auth) = info["mintAuthority"].as_str() {
                    if !mint_auth.is_empty() {
                        return Some(format!("mint_authority={}", &mint_auth[..8.min(mint_auth.len())]));
                    }
                }

                // Check supply concentration: if top holder has >30% of supply, likely rug setup.
                if let Some(total_supply_str) = info["supply"].as_str() {
                    if let Ok(total_supply) = total_supply_str.parse::<u128>() {
                        if total_supply > 0 {
                            if let Some(reason) = check_holder_concentration(mint, total_supply).await {
                                return Some(reason);
                            }
                        }
                    }
                }
            }
            None // Passed safety checks.
        }
        Err(e) => {
            warn!(mint = %mint, error = %e, "SNIPER: token safety check RPC failed, proceeding cautiously");
            None // RPC failure — don't block snipe, but log.
        }
    }
}

/// Check if top holder has >30% of total supply.
/// Uses getTokenLargestAccounts (returns top 20 holders).
async fn check_holder_concentration(mint: &Pubkey, total_supply: u128) -> Option<String> {
    let url = std::env::var("SIM_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(800))
        .build()
        .unwrap_or_default();

    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getTokenLargestAccounts",
        "params": [mint.to_string()]
    });

    let resp = client.post(&url).json(&body).send().await.ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;

    if let Some(accounts) = json["result"]["value"].as_array() {
        if let Some(top) = accounts.first() {
            if let Some(amount_str) = top["amount"].as_str() {
                if let Ok(top_balance) = amount_str.parse::<u128>() {
                    let pct = top_balance * 100 / total_supply;
                    if pct > 30 {
                        return Some(format!(
                            "top_holder={}% ({}..)",
                            pct,
                            top["address"].as_str().unwrap_or("?").get(..8).unwrap_or("?")
                        ));
                    }
                }
            }
        }
    }

    None
}

/// Detect which token program owns a mint (SPL Token vs Token-2022).
/// Uses sim-server RPC. Falls back to SPL Token (majority of PumpFun tokens).
async fn detect_token_program(mint: &Pubkey) -> Pubkey {
    let spl_token: Pubkey = TOKEN_PROGRAM.parse().unwrap();
    let token_2022: Pubkey = TOKEN_2022_PROGRAM.parse().unwrap();

    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getAccountInfo",
        "params": [mint.to_string(), {"encoding": "base64"}]
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap_or_default();

    // Try sim-server first, then fallback to public RPC.
    let urls = [
        std::env::var("SIM_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into()),
        "https://api.mainnet-beta.solana.com".to_string(),
    ];

    for url in &urls {
        match client.post(url).json(&body).send().await {
            Ok(resp) => {
                if let Ok(json) = resp.json::<serde_json::Value>().await {
                    if let Some(owner) = json["result"]["value"]["owner"].as_str() {
                        if owner == TOKEN_2022_PROGRAM {
                            info!(mint = %mint, "SNIPER: mint uses Token-2022");
                            return token_2022;
                        }
                        return spl_token;
                    }
                }
            }
            Err(e) => {
                warn!(mint = %mint, url, error = %e, "SNIPER: token program detection failed, trying next RPC");
            }
        }
    }
    warn!(mint = %mint, "SNIPER: all RPCs failed for token program detection, defaulting to SPL Token");
    spl_token
}


/// Extract token balance from a jsonParsed account response.
fn extract_token_balance(val: &serde_json::Value) -> Option<u64> {
    val["data"]["parsed"]["info"]["tokenAmount"]["amount"]
        .as_str()?
        .parse()
        .ok()
}

/// Fallback estimate when vault balance fetch fails.
/// Uses typical PumpFun graduation reserves: ~85 SOL + ~793M tokens (6 decimals).
fn fallback_token_output(wsol_amount_in: u64) -> u64 {
    let assumed_token_raw: u128 = 793_000_000 * 1_000_000; // 793M tokens × 10^6
    let assumed_sol_raw: u128 = 85_000_000_000;             // 85 SOL in lamports
    let out = assumed_token_raw * (wsol_amount_in as u128) / (assumed_sol_raw + wsol_amount_in as u128);
    info!(fallback_token_out = out as u64, wsol_in = wsol_amount_in, "SNIPER: using fallback token output");
    out as u64
}

/// Read the token balance of a single vault account.
async fn read_vault_balance(vault: &Pubkey) -> u64 {
    let url = std::env::var("SIM_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap_or_default();

    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getAccountInfo",
        "params": [vault.to_string(), {"encoding": "jsonParsed"}]
    });

    match client.post(&url).json(&body).send().await {
        Ok(resp) => {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                return extract_token_balance(&json["result"]["value"]).unwrap_or(0);
            }
            0
        }
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------------------
// Auto-sell: sell tokens immediately after buy to lock in profit
// ---------------------------------------------------------------------------

/// Build and send a sell TX to convert tokens back to SOL.
/// Waits AUTO_SELL_DELAY_MS for the buy to land, then reads token balance and sells all.
async fn auto_sell(
    payer: Arc<Keypair>,
    sender: Arc<JitoSender>,
    tpu_sender: Option<Arc<TpuSender>>,
    tg: TelegramBot,
    token_mint: Pubkey,
    pump_pool: Pubkey,
    token_tp: Pubkey,
    wsol_is_base: bool,
    base_mint: Pubkey,
    quote_mint: Pubkey,
    pool_base_vault: Pubkey,
    pool_quote_vault: Pubkey,
    coin_creator: Pubkey,
    slot: u64,
    snipe_cost: u64,
    strategy_logger: Option<Arc<strategy_logger::StrategyLogger>>,
) -> Result<()> {
    // ANTI-RUG trailing stop auto-sell.
    // Data: 99.4% of PumpFun graduations rug, 96.2% completely drained.
    // Rugs happen in first 2-5 seconds. Strategy must be EXTREMELY aggressive.
    //
    // Strategy:
    // - Poll every 150ms for fastest reaction (rugs are sub-second)
    // - SELL IMMEDIATELY on first profitable poll (≥0.5% gain) — don't wait for more
    // - If price drops 10% from cost → emergency sell instantly (rug starting)
    // - If price drops 5% from peak at any point → sell (don't wait for recovery)
    // - Max hold 8s — most value captured or lost in first 5s
    // - Track consecutive drops: 3 drops in a row = rug pattern → sell
    // - All SOL goes back to wallet (WSOL ATA closed)

    const POLL_MS: u64 = 150;          // ultra-fast poll — rugs are sub-second
    const MAX_HOLD_SECS: u64 = 8;      // max hold — value captured in first 5s

    // Initial delay for buy to land.
    tokio::time::sleep(Duration::from_millis(AUTO_SELL_DELAY_MS)).await;

    let pumpswap_program: Pubkey = PUMPSWAP_PROGRAM.parse().unwrap();
    let wsol_mint: Pubkey = WSOL_MINT.parse().unwrap();
    let spl_token_program: Pubkey = TOKEN_PROGRAM.parse().unwrap();
    let assoc_program: Pubkey = ASSOC_TOKEN_PROGRAM.parse().unwrap();
    let system_program: Pubkey = SYSTEM_PROGRAM.parse().unwrap();
    let fee_recipient: Pubkey = PROTOCOL_FEE_RECIPIENT.parse().unwrap();
    let fee_program: Pubkey = FEE_PROGRAM.parse().unwrap();
    let payer_pk = payer.pubkey();

    let (base_tp, quote_tp) = if wsol_is_base {
        (spl_token_program, token_tp)
    } else {
        (token_tp, spl_token_program)
    };

    // Read our token ATA balance.
    let token_ata = if wsol_is_base {
        ata(&payer_pk, &quote_mint, &quote_tp, &assoc_program)
    } else {
        ata(&payer_pk, &base_mint, &base_tp, &assoc_program)
    };

    // Poll for token balance — buy TX may take a few slots to land.
    // Blind sends (vault=0 at detection) need longer confirmation time.
    let mut token_balance = 0u64;
    for attempt in 0..15 {
        token_balance = read_vault_balance(&token_ata).await;
        if token_balance > 0 {
            info!(token = %token_mint, token_balance, attempt, "AUTO-SELL: buy confirmed, tokens received");
            break;
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    if token_balance == 0 {
        info!(token = %token_mint, "AUTO-SELL: no tokens after 15 polls (12s) — buy didn't land");
        let _ = tg.send(&format!(
            "AUTO-SELL SKIP: buy no landing\nToken: {}\nPool: {}",
            token_mint, pump_pool,
        )).await;
        return Ok(());
    }

    let (wsol_vault, token_vault) = if wsol_is_base {
        (&pool_base_vault, &pool_quote_vault)
    } else {
        (&pool_quote_vault, &pool_base_vault)
    };

    // Anti-rug trailing stop: poll price, track peak, sell FAST.
    let hold_start = Instant::now();
    let mut peak_sol_out: u64 = 0;
    let mut final_sol_out: u64 = 0;
    let mut sell_reason = "max_hold";
    let mut consecutive_drops: u32 = 0;
    let mut prev_sol_out: u64 = 0;
    let mut poll_count: u32 = 0;

    loop {
        let wsol_reserve = read_vault_balance(wsol_vault).await;
        let token_reserve = read_vault_balance(token_vault).await;

        let sol_out = if wsol_reserve > 0 && token_reserve > 0 {
            ((wsol_reserve as u128) * (token_balance as u128)
                / (token_reserve as u128 + token_balance as u128)) as u64
        } else { 0 };

        if sol_out > peak_sol_out {
            peak_sol_out = sol_out;
            consecutive_drops = 0;
        }
        // Track consecutive price drops — rug pattern detector
        if poll_count > 0 && sol_out < prev_sol_out {
            consecutive_drops += 1;
        } else if sol_out > prev_sol_out {
            consecutive_drops = 0;
        }
        prev_sol_out = sol_out;
        poll_count += 1;
        final_sol_out = sol_out;

        let gain_pct = if snipe_cost > 0 {
            (sol_out as f64 - snipe_cost as f64) / snipe_cost as f64 * 100.0
        } else { 0.0 };

        let drop_from_peak_pct = if peak_sol_out > 0 {
            (peak_sol_out as f64 - sol_out as f64) / peak_sol_out as f64 * 100.0
        } else { 0.0 };

        let elapsed_ms = hold_start.elapsed().as_millis() as u64;
        let elapsed = elapsed_ms / 1000;

        debug!(
            token = %token_mint,
            sol_out = sol_out as f64 / 1e9,
            peak = peak_sol_out as f64 / 1e9,
            cost = snipe_cost as f64 / 1e9,
            gain_pct = format!("{:.1}", gain_pct),
            drop_pct = format!("{:.1}", drop_from_peak_pct),
            consecutive_drops,
            elapsed_ms,
            "AUTO-SELL: anti-rug monitor"
        );

        // === ANTI-RUG RULES (ordered by urgency) ===

        // 1. RUG PATTERN: 3+ consecutive price drops = rug in progress → EXIT NOW
        if consecutive_drops >= 3 && poll_count >= 4 {
            sell_reason = "rug_pattern_detected";
            info!(token = %token_mint, consecutive_drops, sol_out, snipe_cost,
                "AUTO-SELL: RUG PATTERN — 3+ consecutive drops, emergency exit");
            break;
        }

        // 2. EMERGENCY: price crashed >10% from cost → rug started → EXIT NOW
        if sol_out > 0 && snipe_cost > 0 && sol_out < snipe_cost * 90 / 100 {
            sell_reason = "emergency_stop_loss";
            info!(token = %token_mint, sol_out, snipe_cost, gain_pct,
                "AUTO-SELL: price dropped >10% from cost — emergency exit");
            break;
        }

        // 3. POOL DRAIN: WSOL reserve dropped below 1 SOL → liquidity being pulled
        if wsol_reserve > 0 && wsol_reserve < 1_000_000_000 && poll_count > 1 {
            sell_reason = "pool_drain_detected";
            info!(token = %token_mint, wsol_reserve, sol_out,
                "AUTO-SELL: pool WSOL < 1 SOL — liquidity being drained");
            break;
        }

        // 4. TAKE PROFIT: any gain ≥0.5% → sell immediately (don't be greedy, 99.4% rug)
        if gain_pct >= 0.5 && poll_count >= 2 {
            sell_reason = "take_profit_immediate";
            info!(token = %token_mint, gain_pct, sol_out, snipe_cost,
                "AUTO-SELL: profitable ≥0.5% — taking profit immediately (99.4% rug rate)");
            break;
        }

        // 5. DROP FROM PEAK: 5% drop from peak at any time → momentum lost
        if peak_sol_out > 0 && drop_from_peak_pct >= 5.0 && poll_count >= 2 {
            sell_reason = "peak_drop_exit";
            info!(token = %token_mint, drop_from_peak_pct, peak_sol_out, sol_out,
                "AUTO-SELL: dropped 5% from peak — selling before rug");
            break;
        }

        // 6. MAX HOLD — sell at whatever price is available.
        if elapsed >= MAX_HOLD_SECS {
            sell_reason = if sol_out >= snipe_cost { "max_hold_profit" } else { "max_hold_cut_loss" };
            info!(token = %token_mint, sol_out, snipe_cost, elapsed,
                "AUTO-SELL: max hold reached, selling at current price");
            break;
        }

        tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
    }

    let sol_out = final_sol_out;
    if sol_out < 5000 {
        info!(token = %token_mint, token_balance, sol_out, "AUTO-SELL: estimated SOL out too low — burning tokens + closing ATA to recover rent");
        // Burn tokens and close ATA to recover ~0.002 SOL rent
        let token_ata_cleanup = if wsol_is_base {
            ata(&payer_pk, &quote_mint, &quote_tp, &assoc_program)
        } else {
            ata(&payer_pk, &base_mint, &base_tp, &assoc_program)
        };
        let burn_tp = if wsol_is_base { quote_tp } else { base_tp };
        let burn_ix = Instruction {
            program_id: burn_tp,
            accounts: vec![
                AccountMeta::new(token_ata_cleanup, false),
                AccountMeta::new(token_mint, false),
                AccountMeta::new_readonly(payer_pk, true),
            ],
            data: {
                let mut d = vec![8u8]; // Burn
                d.extend_from_slice(&token_balance.to_le_bytes());
                d
            },
        };
        let close_ix = Instruction {
            program_id: burn_tp,
            accounts: vec![
                AccountMeta::new(token_ata_cleanup, false),
                AccountMeta::new(payer_pk, false),
                AccountMeta::new_readonly(payer_pk, true),
            ],
            data: vec![9u8], // CloseAccount
        };
        let sim_rpc_url = std::env::var("SIM_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into());
        let blockhash = fetch_cached_blockhash(&sim_rpc_url).await;
        let ixs = vec![burn_ix, close_ix];
        if let Ok(msg) = MessageV0::try_compile(&payer_pk, &ixs, &[], blockhash) {
            if let Ok(tx) = VersionedTransaction::try_new(
                solana_sdk::message::VersionedMessage::V0(msg), &[payer.as_ref()],
            ) {
                let _ = agave_send_tx(&tx).await;
                info!(token = %token_mint, "AUTO-SELL: burn+close TX sent to recover rent");
            }
        }
        return Ok(());
    }

    let min_sol_out = sol_out * (10_000 - SELL_SLIPPAGE_BPS) / 10_000;

    info!(
        token = %token_mint,
        token_balance,
        sol_out,
        min_sol_out,
        sell_reason,
        peak_sol = sol_out as f64 / 1e9,
        cost_sol = snipe_cost as f64 / 1e9,
        "AUTO-SELL: selling tokens back to SOL"
    );

    // Build sell instruction data.
    // When WSOL is base: BUY (buy WSOL base, pay token quote)
    //   data: [buy_disc][base_amount_out (WSOL)][max_quote_amount_in (tokens)]
    // When WSOL is quote: SELL (sell token base, receive WSOL quote)
    //   data: [sell_disc][base_amount_in (tokens)][min_quote_amount_out (WSOL)]
    let (data, need_volume_accs) = if wsol_is_base {
        let mut d = BUY_DISCRIMINATOR.to_vec();
        d.extend_from_slice(&min_sol_out.to_le_bytes());      // base_amount_out (min WSOL)
        d.extend_from_slice(&token_balance.to_le_bytes());     // max_quote_amount_in (all tokens)
        (d, true)
    } else {
        let mut d = SELL_DISCRIMINATOR.to_vec();
        d.extend_from_slice(&token_balance.to_le_bytes());     // base_amount_in (all tokens)
        d.extend_from_slice(&min_sol_out.to_le_bytes());       // min_quote_amount_out (min WSOL)
        (d, false)
    };

    // Derive addresses.
    let global_config = Pubkey::find_program_address(&[b"global_config"], &pumpswap_program).0;
    let event_authority = Pubkey::find_program_address(&[b"__event_authority"], &pumpswap_program).0;
    let global_volume_acc = Pubkey::find_program_address(&[b"global_volume_accumulator"], &pumpswap_program).0;
    let user_volume_acc = Pubkey::find_program_address(&[b"user_volume_accumulator", payer_pk.as_ref()], &pumpswap_program).0;
    let fee_config = Pubkey::find_program_address(&[b"fee_config", pumpswap_program.as_ref()], &fee_program).0;
    let creator_vault_authority = Pubkey::find_program_address(&[b"creator_vault", coin_creator.as_ref()], &pumpswap_program).0;
    let creator_vault_ata = ata(&creator_vault_authority, &quote_mint, &quote_tp, &assoc_program);
    let user_base = ata(&payer_pk, &base_mint, &base_tp, &assoc_program);
    let user_quote = ata(&payer_pk, &quote_mint, &quote_tp, &assoc_program);
    let fee_recipient_quote_ata = ata(&fee_recipient, &quote_mint, &quote_tp, &assoc_program);

    let mut accounts = vec![
        AccountMeta::new(pump_pool, false),
        AccountMeta::new(payer_pk, true),
        AccountMeta::new_readonly(global_config, false),
        AccountMeta::new_readonly(base_mint, false),
        AccountMeta::new_readonly(quote_mint, false),
        AccountMeta::new(user_base, false),
        AccountMeta::new(user_quote, false),
        AccountMeta::new(pool_base_vault, false),
        AccountMeta::new(pool_quote_vault, false),
        AccountMeta::new_readonly(fee_recipient, false),
        AccountMeta::new(fee_recipient_quote_ata, false),
        AccountMeta::new_readonly(base_tp, false),
        AccountMeta::new_readonly(quote_tp, false),
        AccountMeta::new_readonly(system_program, false),
        AccountMeta::new_readonly(assoc_program, false),
        AccountMeta::new_readonly(event_authority, false),
        AccountMeta::new_readonly(pumpswap_program, false),
        AccountMeta::new(creator_vault_ata, false),
        AccountMeta::new_readonly(creator_vault_authority, false),
    ];
    if need_volume_accs {
        accounts.push(AccountMeta::new_readonly(global_volume_acc, false));
        accounts.push(AccountMeta::new(user_volume_acc, false));
    }
    accounts.push(AccountMeta::new_readonly(fee_config, false));
    accounts.push(AccountMeta::new_readonly(fee_program, false));

    let swap_ix = Instruction {
        program_id: pumpswap_program,
        accounts,
        data,
    };

    // Build IX list: create WSOL ATA → swap → close WSOL ATA → tip.
    let wsol_ata = if wsol_is_base { user_base } else { user_quote };

    let create_wsol_ata_ix = Instruction {
        program_id: assoc_program,
        accounts: vec![
            AccountMeta::new(payer_pk, true),
            AccountMeta::new(wsol_ata, false),
            AccountMeta::new_readonly(payer_pk, false),
            AccountMeta::new_readonly(wsol_mint, false),
            AccountMeta::new_readonly(system_program, false),
            AccountMeta::new_readonly(spl_token_program, false),
        ],
        data: vec![1u8],
    };

    let close_wsol_ix = Instruction {
        program_id: spl_token_program,
        accounts: vec![
            AccountMeta::new(wsol_ata, false),
            AccountMeta::new(payer_pk, false),
            AccountMeta::new_readonly(payer_pk, true),
        ],
        data: vec![9u8],
    };

    let tip_ix = sender.tip_instruction(&payer_pk);

    // Compute budget for priority scheduling.
    let priority_fee = std::env::var("PRIORITY_FEE_MICRO_LAMPORTS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(100_000);
    let cu_limit_ix = solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(200_000);
    let cu_price_ix = solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(priority_fee);

    // Close token ATA after swap to recover rent (~0.002 SOL).
    // Only safe when SELL instruction is used (base_amount_in = ALL tokens → balance goes to 0).
    // BUY instruction uses max_quote_amount_in which may leave dust due to AMM rounding.
    // If close fails, the entire TX reverts and we lose the sell — so only close when safe.
    let token_ata = if wsol_is_base { user_quote } else { user_base };
    let mut ixs = vec![cu_limit_ix, cu_price_ix, create_wsol_ata_ix, swap_ix];
    if !wsol_is_base {
        // SELL path: base_amount_in = token_balance → guaranteed zero balance after swap
        let close_token_ata_ix = Instruction {
            program_id: token_tp,
            accounts: vec![
                AccountMeta::new(token_ata, false),
                AccountMeta::new(payer_pk, false),
                AccountMeta::new_readonly(payer_pk, true),
            ],
            data: vec![9u8], // CloseAccount
        };
        ixs.push(close_token_ata_ix);
    }
    ixs.push(close_wsol_ix);
    ixs.push(tip_ix);

    // Get fresh blockhash for the sell TX.
    let sim_rpc_url = std::env::var("SIM_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into());
    let blockhash = fetch_cached_blockhash(&sim_rpc_url).await;

    let msg = MessageV0::try_compile(&payer_pk, &ixs, &[], blockhash)
        .context("compile auto-sell message")?;
    let tx = VersionedTransaction::try_new(
        solana_sdk::message::VersionedMessage::V0(msg),
        &[payer.as_ref()],
    ).context("sign auto-sell tx")?;

    let tx_bytes = serialize(&tx).context("serialize auto-sell tx")?;
    if tx_bytes.len() > MAX_TX_SIZE {
        warn!(size = tx_bytes.len(), "AUTO-SELL: TX too large");
        return Ok(());
    }

    // Send sell TX via all channels.
    let tx_for_tpu = tx.clone();
    let tx_for_send_tx = tx.clone();
    let tx_for_agave = tx.clone();
    let tpu_clone = tpu_sender.clone();
    let sender_clone = Arc::clone(&sender);

    let tpu_handle = std::thread::spawn(move || -> usize {
        if let Some(tpu) = tpu_clone {
            tpu.send(&tx_for_tpu, slot)
        } else {
            0
        }
    });

    let bundle_txs = [tx];
    let (bundle_result, _, agave_result) = tokio::join!(
        sender.send_bundle_multi_region(&bundle_txs),
        sender_clone.send_transaction_multi_region(&tx_for_send_tx),
        agave_send_tx(&tx_for_agave),
    );

    let tpu_sent = tpu_handle.join().unwrap_or(0);

    // Wait a bit for the sell TX to land, then read wallet balance.
    tokio::time::sleep(Duration::from_millis(3000)).await;
    let wallet_balance = read_sol_balance(&payer_pk).await;
    let profit_lamports = sol_out as i64 - snipe_cost as i64;
    let profit_sol = profit_lamports as f64 / 1e9;

    match &bundle_result {
        Ok(bundle_id) => {
            info!(
                token = %token_mint,
                token_balance,
                sol_out,
                profit_lamports,
                wallet_sol = wallet_balance as f64 / 1e9,
                bundle_id = %bundle_id,
                tpu_sent,
                "AUTO-SELL: sell bundle submitted"
            );
            let hold_secs = hold_start.elapsed().as_secs();
            let _ = tg.alert_auto_sell_result(
                &token_mint.to_string(), sell_reason,
                snipe_cost as f64 / 1e9, sol_out as f64 / 1e9, hold_secs,
            ).await;
        }
        Err(ref e) => {
            warn!(error = %e, token = %token_mint, tpu_sent, "AUTO-SELL: Jito failed (TPU may succeed)");
            // Still notify on TPU-only sends.
            if tpu_sent > 0 {
                let hold_secs = hold_start.elapsed().as_secs();
                let _ = tg.alert_auto_sell_result(
                    &token_mint.to_string(), sell_reason,
                    snipe_cost as f64 / 1e9, sol_out as f64 / 1e9, hold_secs,
                ).await;
            }
        }
    }
    match &agave_result {
        Ok(sig) => info!(sig = %sig, "AUTO-SELL: Agave sendTransaction OK"),
        Err(e) => debug!(error = %e, "AUTO-SELL: Agave send failed"),
    }

    // Log sell result to PG
    if let Some(ref logger) = strategy_logger {
        let sell_tx_sig = bundle_result.as_ref().ok().map(|s| s.clone());
        logger.update_sniper_sell(strategy_logger::types::LogSniperSellUpdate {
            token_mint: token_mint.to_string(),
            sell_triggered: true,
            sell_reason: Some(sell_reason.to_string()),
            sell_tx_sig,
            sell_sol_received: Some(sol_out),
            hold_duration_ms: Some(hold_start.elapsed().as_millis() as u64),
            pnl_lamports: Some(profit_lamports),
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Simulation helper
// ---------------------------------------------------------------------------

enum SimResult {
    Success,
    Failed(String),
    RpcError(String),
}

/// Simulate a transaction via RPC before sending.
async fn simulate_tx(rpc_url: &str, tx: &VersionedTransaction) -> SimResult {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(2000))
        .build()
        .unwrap_or_default();

    let tx_bytes = match serialize(tx) {
        Ok(b) => b,
        Err(e) => return SimResult::RpcError(format!("serialize: {}", e)),
    };
    let tx_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &tx_bytes);

    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "simulateTransaction",
        "params": [tx_b64, {
            "encoding": "base64",
            "replaceRecentBlockhash": true,
            "commitment": "processed"
        }]
    });

    let resp = match client.post(rpc_url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => return SimResult::RpcError(format!("http: {}", e)),
    };
    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(e) => return SimResult::RpcError(format!("json: {}", e)),
    };

    if let Some(err) = json.get("error") {
        return SimResult::RpcError(format!("rpc_error: {}", err));
    }

    let result = &json["result"]["value"];
    if let Some(err) = result.get("err") {
        if !err.is_null() {
            return SimResult::Failed(format!("{}", err));
        }
    }

    SimResult::Success
}

/// Read SOL balance (native lamports) of a wallet.
async fn read_sol_balance(pubkey: &Pubkey) -> u64 {
    let url = std::env::var("SIM_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap_or_default();

    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getBalance",
        "params": [pubkey.to_string()]
    });

    match client.post(&url).json(&body).send().await {
        Ok(resp) => {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                return json["result"]["value"].as_u64().unwrap_or(0);
            }
            0
        }
        Err(_) => 0,
    }
}
