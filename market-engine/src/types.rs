// types.rs – Shared types for the market-engine pipeline.

use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::time::Instant;

// ---------------------------------------------------------------------------
// DEX program IDs
// ---------------------------------------------------------------------------

/// Well-known DEX program IDs on Solana mainnet.
pub mod dex_programs {
    use solana_sdk::pubkey::Pubkey;

    macro_rules! pk {
        ($s:expr) => {
            $s.parse::<Pubkey>().expect("valid pubkey literal")
        };
    }

    pub fn raydium_amm_v4() -> Pubkey {
        pk!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8")
    }

    pub fn raydium_clmm() -> Pubkey {
        pk!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK")
    }

    pub fn orca_whirlpool() -> Pubkey {
        pk!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc")
    }

    pub fn meteora_dlmm() -> Pubkey {
        pk!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo")
    }

    pub fn pumpswap() -> Pubkey {
        pk!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA")
    }

    pub fn fluxbeam() -> Pubkey {
        pk!("FLUXubRmkEi2q6K3Y9kBPg9248ggaZVsoSFhtJHSR1X4")
    }

    pub fn saber() -> Pubkey {
        pk!("SSwpkEEcbUqx4vtoEByFjSkhKdCT862DNVb52nZg1UZ")
    }

    pub fn raydium_cpmm() -> Pubkey {
        pk!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C")
    }

    pub fn pumpfun_bonding() -> Pubkey {
        pk!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P")
    }

    pub fn jupiter_v6() -> Pubkey {
        pk!("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4")
    }

    pub fn meteora_dynamic_amm() -> Pubkey {
        pk!("Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB")
    }

    pub fn raydium_launchlab() -> Pubkey {
        pk!("LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj")
    }

    pub fn moonshot() -> Pubkey {
        pk!("MoonCVVNZFSYkqNXP6bxHLPL6QQJiMagDL3qcqUQTrG")
    }

    pub fn all() -> Vec<Pubkey> {
        vec![
            raydium_amm_v4(),
            raydium_clmm(),
            orca_whirlpool(),
            meteora_dlmm(),
            meteora_dynamic_amm(),
            pumpswap(),
            pumpfun_bonding(),
            jupiter_v6(),
            fluxbeam(),
            saber(),
            raydium_cpmm(),
            raydium_launchlab(),
            moonshot(),
        ]
    }
}

// ---------------------------------------------------------------------------
// Flash Loan Providers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FlashProvider {
    MarginFi,
    PortFinance,
    Solend,
    Kamino,
}

impl FlashProvider {
    pub fn fee_bps(&self) -> u64 {
        match self {
            FlashProvider::MarginFi => 0,
            FlashProvider::PortFinance => 9, // 0.09 %
            FlashProvider::Solend => 30,     // 0.30 %
            FlashProvider::Kamino => 1,      // 0.01 %
        }
    }

    pub fn program_id(&self) -> solana_sdk::pubkey::Pubkey {
        use solana_sdk::pubkey::Pubkey;
        match self {
            FlashProvider::MarginFi => "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA"
                .parse()
                .unwrap(),
            FlashProvider::PortFinance => "Port7uDYB3wk6GJAw4KT1WpTeMtSu9bTcChBHkX2LfR"
                .parse()
                .unwrap(),
            FlashProvider::Solend => "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo"
                .parse()
                .unwrap(),
            FlashProvider::Kamino => "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD"
                .parse()
                .unwrap(),
        }
    }

    /// Lower = higher priority.
    pub fn priority(&self) -> u8 {
        match self {
            FlashProvider::MarginFi => 0,
            FlashProvider::PortFinance => 1,
            FlashProvider::Solend => 2,
            FlashProvider::Kamino => 3,
        }
    }

    pub fn fallback_order(preferred: FlashProvider) -> Vec<FlashProvider> {
        let mut providers = vec![
            preferred,
            FlashProvider::MarginFi,
            FlashProvider::PortFinance,
            FlashProvider::Solend,
            FlashProvider::Kamino,
        ];
        providers.dedup();
        providers
    }
}

// ---------------------------------------------------------------------------
// Swap hop
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hop {
    /// DEX pool / pair account.
    pub pool: Pubkey,
    /// DEX program handling this hop.
    pub dex_program: Pubkey,
    /// Token being sold.
    pub token_in: Pubkey,
    /// Token being received.
    pub token_out: Pubkey,
    /// Estimated output amount (in smallest units).
    pub amount_out: u64,
    /// Estimated price impact (%).
    pub price_impact: f64,
}

// ---------------------------------------------------------------------------
// Arbitrage route
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteParams {
    /// Ordered hops forming the arbitrage path.
    pub hops: Vec<Hop>,
    /// Input amount to borrow (in lamports or token units).
    pub borrow_amount: u64,
    /// Expected gross profit before fees (in lamports).
    pub gross_profit: u64,
    /// Net profit after flash loan fee + tx fee.
    pub net_profit: i64,
    /// Composite risk factor (0.0 = no risk, 1.0 = maximum).
    pub risk_factor: f64,
    /// Strategy label for telemetry ("bellman-ford", "cross-dex", etc.).
    pub strategy: &'static str,
    /// Route tier based on robustness to ~400ms slot delay.
    /// 1=best (Raydium-only), 2=Raydium+Meteora, 3=PumpSwap, 4=Orca w/ liquidity, 5=other, 99=fragile.
    #[serde(default)]
    pub tier: u8,
}

// ---------------------------------------------------------------------------
// Opportunity bundle (ready for executor)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct OpportunityBundle {
    pub id: u64,
    pub slot: u64,
    pub detected_at: Instant,
    pub route: RouteParams,
    pub flash_provider: FlashProvider,
    /// Composite score (higher = better).
    pub score: f64,
}

// ---------------------------------------------------------------------------
// Dex type enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DexType {
    RaydiumAmmV4,
    RaydiumClmm,
    OrcaWhirlpool,
    MeteoraDlmm,
    PumpSwap,
    Fluxbeam,
    Saber,
    RaydiumCpmm,
    Unknown,
}

impl DexType {
    pub fn from_program_id(pk: &Pubkey) -> Self {
        let pk_str = pk.to_string();
        match pk_str.as_str() {
            "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8" => DexType::RaydiumAmmV4,
            "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK" => DexType::RaydiumClmm,
            "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc" => DexType::OrcaWhirlpool,
            "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo" => DexType::MeteoraDlmm,
            "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA" => DexType::PumpSwap,
            "FLUXubRmkEi2q6K3Y9kBPg9248ggaZVsoSFhtJHSR1X4" => DexType::Fluxbeam,
            "SSwpkEEcbUqx4vtoEByFjSkhKdCT862DNVb52nZg1UZ" => DexType::Saber,
            "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C" => DexType::RaydiumCpmm,
            _ => DexType::Unknown,
        }
    }
}
