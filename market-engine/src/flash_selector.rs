// flash_selector.rs – Select the optimal flash loan provider for an opportunity.
//
// We select the lowest-fee healthy provider with sufficient liquidity. Only
// providers with working executor wrappers start as healthy.

use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use tracing::info;

use crate::types::FlashProvider;

// ---------------------------------------------------------------------------
// FlashProviderInfo
// ---------------------------------------------------------------------------

/// Current known capacity of a flash loan provider (lamports available).
/// In production this would be fetched from chain state; here we track it
/// with a lightweight in-memory cache updated from observed transactions.
#[derive(Debug, Clone)]
pub struct FlashProviderInfo {
    pub provider: FlashProvider,
    /// Available liquidity (lamports).
    pub available_lamports: u64,
    /// Whether this provider is currently reachable.
    pub healthy: bool,
}

// ---------------------------------------------------------------------------
// FlashSelector
// ---------------------------------------------------------------------------

pub struct FlashSelector {
    providers: Vec<FlashProviderInfo>,
}

impl FlashSelector {
    /// Create a selector with default liquidity estimates.
    ///
    /// These estimates are intentionally conservative; they will be updated
    /// continuously from on-chain observations.
    pub fn new() -> Self {
        Self {
            providers: vec![
                FlashProviderInfo {
                    provider: FlashProvider::MarginFi,
                    available_lamports: 10_000_000_000_000, // 10 000 SOL
                    healthy: true,
                },
                FlashProviderInfo {
                    provider: FlashProvider::PortFinance,
                    available_lamports: 5_000_000_000_000,
                    healthy: false,
                },
                FlashProviderInfo {
                    provider: FlashProvider::Solend,
                    available_lamports: 8_000_000_000_000,
                    healthy: true,
                },
                FlashProviderInfo {
                    provider: FlashProvider::Kamino,
                    available_lamports: 15_000_000_000_000, // 15 000 SOL
                    healthy: true,
                },
            ],
        }
    }

    /// Select the best (lowest fee) provider for `borrow_amount` lamports.
    pub fn select(&self, borrow_amount: u64) -> Result<FlashProvider> {
        // Sort by fee ascending, then by priority (already in spec priority order).
        let mut candidates: Vec<&FlashProviderInfo> = self
            .providers
            .iter()
            .filter(|p| p.healthy && p.available_lamports >= borrow_amount)
            .collect();

        candidates.sort_by_key(|p| (p.provider.fee_bps(), p.provider.priority()));

        let best = candidates.first().ok_or_else(|| {
            anyhow::anyhow!(
                "no flash loan provider available for {} lamports",
                borrow_amount
            )
        })?;

        info!(
            provider = ?best.provider,
            fee_bps  = best.provider.fee_bps(),
            borrow   = borrow_amount,
            "selected flash loan provider"
        );

        Ok(best.provider)
    }

    /// Mark a provider as unhealthy (e.g., after a failed transaction).
    pub fn mark_unhealthy(&mut self, provider: FlashProvider) {
        if let Some(p) = self.providers.iter_mut().find(|p| p.provider == provider) {
            p.healthy = false;
        }
    }

    /// Update liquidity estimate for a provider.
    pub fn update_liquidity(&mut self, provider: FlashProvider, available: u64) {
        if let Some(p) = self.providers.iter_mut().find(|p| p.provider == provider) {
            p.available_lamports = available;
            p.healthy = true;
        }
    }

    /// Return the program ID for a given provider.
    pub fn program_id(provider: FlashProvider) -> Pubkey {
        provider.program_id()
    }

    /// Compute the flash loan fee for `borrow_amount` with the given provider.
    pub fn fee(provider: FlashProvider, borrow_amount: u64) -> u64 {
        let fee_bps = provider.fee_bps();
        (borrow_amount as u128 * fee_bps as u128 / 10_000) as u64
    }
}

impl Default for FlashSelector {
    fn default() -> Self {
        Self::new()
    }
}
