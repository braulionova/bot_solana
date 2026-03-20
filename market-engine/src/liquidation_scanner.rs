//! liquidation_scanner.rs – Phase A: MarginFi Liquidation Monitor
//!
//! Polls MarginFi lending market for undercollateralized obligations.
//! When health_factor < 1.0, emits a LiquidationOpportunity signal
//! with the obligation details for the executor.
//!
//! Phase A is detection-only — no execution. The executor Phase B
//! will build flash-borrow → liquidate → swap → repay transactions.

use std::time::{Duration, Instant};

use base64::Engine as _;
use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// MarginFi constants (mainnet)
// ---------------------------------------------------------------------------

const MARGINFI_PROGRAM: &str = "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA";
/// MarginFi main lending market (Group account).
const MARGINFI_GROUP: &str = "4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8";

/// Well-known MarginFi bank accounts (SOL, USDC, USDT) for filtering.
const WSOL_BANK: &str = "CCKtUs6Cgwo4aaQUmBPmyoApH2gUDErxNZCAoD2dbAvk";
const USDC_BANK: &str = "2s37akK2eyBbp8DZgCm7RtsaEz8eJP3Nxd4urLHQv7b5";

/// Minimum debt value in lamports equivalent to consider liquidating.
/// Below this threshold, gas + tip cost exceeds liquidation bonus.
const MIN_DEBT_LAMPORTS: u64 = 500_000_000; // 0.5 SOL equivalent

/// Health factor threshold — below this, the obligation is liquidatable.
const LIQUIDATION_THRESHOLD: f64 = 1.0;

/// Poll interval for obligation scanning.
const POLL_INTERVAL: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// LiquidationOpportunity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LiquidationOpportunity {
    pub obligation: Pubkey,
    pub protocol: &'static str,
    pub health_factor: f64,
    pub collateral_mint: Pubkey,
    pub debt_mint: Pubkey,
    pub max_repay_amount: u64,
    pub detected_at: Instant,
}

// ---------------------------------------------------------------------------
// MarginFi Obligation Scanner
// ---------------------------------------------------------------------------

/// Scan MarginFi accounts for liquidatable positions.
/// This is designed to run in a dedicated thread with periodic polling.
pub struct LiquidationScanner {
    rpc_url: String,
    client: reqwest::blocking::Client,
    /// Track recently detected obligations to avoid spamming.
    recent_detections: std::collections::HashMap<Pubkey, Instant>,
}

impl LiquidationScanner {
    pub fn new(rpc_url: &str) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());

        Self {
            rpc_url: rpc_url.to_string(),
            client,
            recent_detections: std::collections::HashMap::new(),
        }
    }

    /// Run one scan cycle. Returns any liquidatable opportunities found.
    pub fn scan_once(&mut self) -> Vec<LiquidationOpportunity> {
        let mut opportunities = Vec::new();

        // Fetch MarginFi accounts using getProgramAccounts with filters.
        // MarginFi "marginfi_account" (user obligation) accounts are identified by:
        //   - Owner = MarginFi program
        //   - Data size = 2304 bytes (Anchor discriminator + MarginfiAccount struct)
        //   - First 8 bytes = Anchor discriminator for MarginfiAccount
        let marginfi_program: Pubkey = MARGINFI_PROGRAM.parse().unwrap();
        let marginfi_group: Pubkey = MARGINFI_GROUP.parse().unwrap();

        // MarginfiAccount Anchor discriminator: sha256("account:MarginfiAccount")[..8]
        // = [67, 178, 130, 109, 126, 114, 28, 42]
        let discriminator = [67u8, 178, 130, 109, 126, 114, 28, 42];

        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "getProgramAccounts",
            "params": [
                MARGINFI_PROGRAM,
                {
                    "encoding": "base64",
                    "filters": [
                        { "dataSize": 2304 },
                        {
                            "memcmp": {
                                "offset": 0,
                                "bytes": bs58::encode(&discriminator).into_string()
                            }
                        },
                        {
                            "memcmp": {
                                "offset": 8,
                                "bytes": marginfi_group.to_string()
                            }
                        }
                    ]
                }
            ]
        });

        let resp = match self.client.post(&self.rpc_url).json(&body).send() {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "liquidation-scanner: RPC request failed");
                return opportunities;
            }
        };

        let json: serde_json::Value = match resp.json() {
            Ok(j) => j,
            Err(e) => {
                warn!(error = %e, "liquidation-scanner: RPC response parse failed");
                return opportunities;
            }
        };

        let accounts = match json["result"].as_array() {
            Some(a) => a,
            None => {
                debug!("liquidation-scanner: no accounts returned");
                return opportunities;
            }
        };

        let mut scanned = 0u64;
        let mut underwater = 0u64;

        for account in accounts {
            let pubkey_str = match account["pubkey"].as_str() {
                Some(s) => s,
                None => continue,
            };
            let obligation: Pubkey = match pubkey_str.parse() {
                Ok(pk) => pk,
                Err(_) => continue,
            };

            let data_b64 = match account["account"]["data"][0].as_str() {
                Some(d) => d,
                None => continue,
            };
            let data = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
                Ok(d) => d,
                Err(_) => continue,
            };

            scanned += 1;

            // Parse MarginfiAccount to extract lending/borrowing balances.
            if let Some(opp) = parse_marginfi_account(&obligation, &data) {
                if opp.health_factor < LIQUIDATION_THRESHOLD && opp.max_repay_amount >= MIN_DEBT_LAMPORTS {
                    // Dedup: skip if recently detected.
                    if let Some(last) = self.recent_detections.get(&obligation) {
                        if last.elapsed() < Duration::from_secs(60) {
                            continue;
                        }
                    }
                    self.recent_detections.insert(obligation, Instant::now());
                    underwater += 1;

                    info!(
                        obligation = %obligation,
                        health = opp.health_factor,
                        debt_mint = %opp.debt_mint,
                        collateral_mint = %opp.collateral_mint,
                        max_repay = opp.max_repay_amount,
                        "[liquidation] undercollateralized position detected"
                    );

                    opportunities.push(opp);
                }
            }
        }

        // GC old detections.
        self.recent_detections.retain(|_, t| t.elapsed() < Duration::from_secs(120));

        if scanned > 0 {
            debug!(scanned, underwater, "liquidation-scanner: scan complete");
        }

        opportunities
    }

    /// Poll interval for the scanner.
    pub fn poll_interval(&self) -> Duration {
        POLL_INTERVAL
    }
}

// ---------------------------------------------------------------------------
// MarginFi account parser
// ---------------------------------------------------------------------------

/// Parse a MarginfiAccount to compute health factor.
///
/// MarginfiAccount layout (Anchor, after 8-byte discriminator):
///   8:   group (Pubkey, 32)
///  40:   authority (Pubkey, 32)
///  72:   lending_account (LendingAccount struct)
///
/// LendingAccount:
///   72:  balances: [Balance; 16]  — each Balance is 136 bytes
///
/// Balance layout (136 bytes):
///   0:  active (bool, 1 byte)
///   1:  bank_pk (Pubkey, 32)
///  33:  padding (7 bytes)
///  40:  asset_shares (WrappedI80F48, 16 bytes) — fixed-point
///  56:  liability_shares (WrappedI80F48, 16 bytes) — fixed-point
///  72:  emissions_outstanding (WrappedI80F48, 16 bytes)
///  88:  last_update (u64, 8 bytes)
///  96:  padding (40 bytes)
///
/// Total Balance: 136 bytes × 16 = 2176 bytes
/// Total MarginfiAccount: 8 + 32 + 32 + 2176 + padding ≈ 2304
fn parse_marginfi_account(
    obligation: &Pubkey,
    data: &[u8],
) -> Option<LiquidationOpportunity> {
    if data.len() < 2304 {
        return None;
    }

    let wsol: Pubkey = "So11111111111111111111111111111111111111112".parse().unwrap();
    let usdc: Pubkey = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();

    // Parse all 16 balance slots starting at offset 72.
    let balance_start = 72;
    let balance_size = 136;

    let mut total_collateral_weighted = 0.0f64;
    let mut total_liability_weighted = 0.0f64;
    let mut largest_collateral_mint = Pubkey::default();
    let mut largest_debt_mint = Pubkey::default();
    let mut largest_collateral_value = 0.0f64;
    let mut largest_debt_value = 0.0f64;
    let mut max_repay = 0u64;

    for i in 0..16 {
        let off = balance_start + i * balance_size;
        if off + balance_size > data.len() {
            break;
        }

        let active = data[off] != 0;
        if !active {
            continue;
        }

        let bank_pk = Pubkey::from(
            <[u8; 32]>::try_from(&data[off + 1..off + 33]).unwrap_or([0u8; 32])
        );

        // Read asset_shares and liability_shares as i128 (I80F48 fixed-point).
        let asset_shares_raw = i128::from_le_bytes(
            data[off + 40..off + 56].try_into().unwrap_or([0u8; 16])
        );
        let liability_shares_raw = i128::from_le_bytes(
            data[off + 56..off + 72].try_into().unwrap_or([0u8; 16])
        );

        // I80F48 has 48 fractional bits. Convert to f64.
        let asset_shares = asset_shares_raw as f64 / (1u128 << 48) as f64;
        let liability_shares = liability_shares_raw as f64 / (1u128 << 48) as f64;

        if asset_shares < 0.0001 && liability_shares < 0.0001 {
            continue;
        }

        // Approximate value weighting.
        // Without reading bank state (share→amount conversion), we use shares
        // directly as a proxy. In Phase B, bank state will give exact amounts.
        // For health computation, the ratio of collateral/liability shares is
        // a reasonable proxy for triage.
        let bank_str = bank_pk.to_string();
        let weight = if bank_str == WSOL_BANK { 1.0 }
            else if bank_str == USDC_BANK { 1.0 }
            else { 0.8 }; // unknown banks weighted conservatively

        if asset_shares > 0.001 {
            let value = asset_shares * weight;
            total_collateral_weighted += value;
            if value > largest_collateral_value {
                largest_collateral_value = value;
                // Map bank to approximate mint (best-effort).
                largest_collateral_mint = bank_to_mint_approx(&bank_str);
            }
        }

        if liability_shares > 0.001 {
            let value = liability_shares * weight;
            total_liability_weighted += value;
            if value > largest_debt_value {
                largest_debt_value = value;
                largest_debt_mint = bank_to_mint_approx(&bank_str);
                // Approximate repay amount in lamports.
                max_repay = (liability_shares * 1_000_000_000.0) as u64;
            }
        }
    }

    if total_liability_weighted < 0.001 {
        return None; // No debt — nothing to liquidate.
    }

    let health_factor = total_collateral_weighted / total_liability_weighted;

    Some(LiquidationOpportunity {
        obligation: *obligation,
        protocol: "MarginFi",
        health_factor,
        collateral_mint: largest_collateral_mint,
        debt_mint: largest_debt_mint,
        max_repay_amount: max_repay,
        detected_at: Instant::now(),
    })
}

/// Best-effort bank → mint mapping for known banks.
fn bank_to_mint_approx(bank_str: &str) -> Pubkey {
    match bank_str {
        "CCKtUs6Cgwo4aaQUmBPmyoApH2gUDErxNZCAoD2dbAvk" => {
            "So11111111111111111111111111111111111111112".parse().unwrap() // SOL
        }
        "2s37akK2eyBbp8DZgCm7RtsaEz8eJP3Nxd4urLHQv7b5" => {
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap() // USDC
        }
        _ => Pubkey::default(), // Unknown bank
    }
}
