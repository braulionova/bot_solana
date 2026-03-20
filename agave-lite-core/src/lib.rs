//! agave-lite-core: Embedded account feed for zero-latency MEV.
//!
//! Provides a shared `AccountCache` (DashMap) populated by:
//! 1. Snapshot data at startup (pool_accounts.bin)
//! 2. RPC polling (configurable interval)
//! 3. Shred-decoded transaction deltas (from Turbine)
//!
//! The helios bot reads reserves DIRECTLY from this DashMap — no RPC,
//! no IPC, no serialization. Sub-microsecond reads.

pub mod account_cache;
pub mod feed;
pub mod grpc_subscriber;
pub mod pool_data_loader;
