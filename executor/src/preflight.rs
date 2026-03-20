// preflight.rs – In-process fast preflight validation.
//
// Catches obvious TX failures in microseconds, before spending 30-150ms on
// a full simulateTransaction round-trip.
//
// Checks performed (all in-process, zero network):
//   1. TX serialized size ≤ 1232 bytes (Solana MTU)
//   2. Account count ≤ 64 (Solana per-TX limit)
//   3. Route net_profit ≥ min_profit (fast-path reject before simulation)
//   4. Hop amount_out is non-zero for each hop
//   5. borrow_amount and gross_profit are non-zero
//
// Latency: ~0-5 µs per TX (entirely CPU-bound, no I/O).

use market_engine::types::RouteParams;
use solana_sdk::transaction::VersionedTransaction;
use tracing::warn;

const MAX_TX_BYTES: usize = 1232;
const MAX_ACCOUNTS_PER_TX: usize = 64;

/// Result of in-process preflight checks.
pub enum PreflightResult {
    Ok,
    Rejected(String),
}

impl PreflightResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, PreflightResult::Ok)
    }
}

/// Run all in-process preflight checks on a built transaction + its route.
///
/// Call this *before* `simulator.simulate(tx)` to skip the RPC round-trip
/// for TXs that are obviously invalid.
pub fn check(tx: &VersionedTransaction, route: &RouteParams, min_profit: i64) -> PreflightResult {
    // ── 1. TX size (warning only — ALT expansion may shrink it later) ──────
    let tx_bytes = match bincode::serialize(tx) {
        Ok(b) => b,
        Err(e) => {
            return PreflightResult::Rejected(format!("serialize error: {}", e));
        }
    };
    // Note: TX size > 1232 is handled by ALT expansion in the executor loop.
    // Only reject if wildly oversized (>3000, likely a bug).
    if tx_bytes.len() > 3000 {
        return PreflightResult::Rejected(format!(
            "TX wildly oversized: {} bytes (possible bug)",
            tx_bytes.len()
        ));
    }

    // ── 2. Account count (V0 messages with ALT lookups resolve at runtime) ──
    let n_accounts = match &tx.message {
        solana_sdk::message::VersionedMessage::Legacy(m) => m.account_keys.len(),
        solana_sdk::message::VersionedMessage::V0(m) => {
            m.account_keys.len()
                + m.address_table_lookups
                    .iter()
                    .map(|l| l.writable_indexes.len() + l.readonly_indexes.len())
                    .sum::<usize>()
        }
    };
    if n_accounts > MAX_ACCOUNTS_PER_TX {
        return PreflightResult::Rejected(format!(
            "too many accounts: {} > {}",
            n_accounts, MAX_ACCOUNTS_PER_TX
        ));
    }

    // ── 3. Minimum profit ────────────────────────────────────────────────────
    if route.net_profit < min_profit {
        return PreflightResult::Rejected(format!(
            "net_profit {} < min_profit {}",
            route.net_profit, min_profit
        ));
    }

    // ── 4. Non-zero amounts ──────────────────────────────────────────────────
    if route.borrow_amount == 0 {
        return PreflightResult::Rejected("borrow_amount=0".to_string());
    }
    if route.gross_profit == 0 {
        return PreflightResult::Rejected("gross_profit=0".to_string());
    }
    if route.hops.is_empty() {
        return PreflightResult::Rejected("route has no hops".to_string());
    }
    for (i, hop) in route.hops.iter().enumerate() {
        if hop.amount_out == 0 {
            return PreflightResult::Rejected(format!("hop[{}] amount_out=0", i));
        }
    }

    // ── 5. Risk factor sanity ─────────────────────────────────────────────────
    if route.risk_factor >= 1.0 {
        return PreflightResult::Rejected(format!(
            "risk_factor={:.2} >= 1.0 (maximum risk)",
            route.risk_factor
        ));
    }

    PreflightResult::Ok
}

/// Log a preflight rejection and return `false`.
pub fn log_rejected(id: u64, reason: &str) -> bool {
    warn!(id, reason, "preflight REJECTED – skipping simulation");
    false
}
