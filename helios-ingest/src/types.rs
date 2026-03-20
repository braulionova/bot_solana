//! Shared types for the ingest pipeline.

use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use std::time::Instant;

/// Data source identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// Yellowstone gRPC (SaaS: Helius, Triton, Shyft)
    YellowstoneGrpc,
    /// Richat gRPC (self-hosted, local)
    Richat,
    /// ShredstreamProxy / SolanaCDN (entry-level shreds)
    Shredstream,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::YellowstoneGrpc => write!(f, "yellowstone"),
            Source::Richat => write!(f, "richat"),
            Source::Shredstream => write!(f, "shredstream"),
        }
    }
}

/// A unified ingest event from any source.
#[derive(Debug, Clone)]
pub struct IngestEvent {
    pub source: Source,
    pub received_at: Instant,
    pub slot: u64,
    pub event_type: EventType,
    pub data: EventData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    Transaction,
    AccountUpdate,
    SlotUpdate,
}

#[derive(Debug, Clone)]
pub enum EventData {
    Transaction(TransactionInfo),
    AccountUpdate(AccountUpdateInfo),
    Slot(SlotInfo),
}

/// A parsed transaction from gRPC sources.
#[derive(Debug, Clone)]
pub struct TransactionInfo {
    pub signature: Signature,
    pub is_vote: bool,
    pub success: bool,
    /// Program IDs involved in the TX.
    pub program_ids: Vec<Pubkey>,
    /// Pre/post token balance changes (mint → delta).
    pub token_deltas: Vec<(Pubkey, i64)>,
    /// Accounts referenced (for pool detection).
    pub accounts: Vec<Pubkey>,
    /// Raw serialized TX (for forwarding to strategy engine).
    pub raw: Vec<u8>,
}

/// An account state update from gRPC sources.
#[derive(Debug, Clone)]
pub struct AccountUpdateInfo {
    pub pubkey: Pubkey,
    pub owner: Pubkey,
    pub lamports: u64,
    pub data: Vec<u8>,
    pub write_version: u64,
    pub is_startup: bool,
}

/// Slot notification.
#[derive(Debug, Clone)]
pub struct SlotInfo {
    pub slot: u64,
    pub parent: u64,
    pub status: SlotStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStatus {
    Processed,
    Confirmed,
    Finalized,
}

/// Known DEX program IDs for filtering.
pub const DEX_PROGRAMS: &[&str] = &[
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", // Raydium AMM V4
    "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK", // Raydium CLMM
    "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",   // Orca Whirlpool
    "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",   // Meteora DLMM
    "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA",   // PumpSwap
    "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",   // PumpFun bonding
    "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4",   // Jupiter V6
];
