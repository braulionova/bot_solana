//! helios-ingest — Multi-source Solana data aggregator.
//!
//! Combines 3 sources into a unified dedup'd event bus:
//!   1. Yellowstone gRPC (Helius/Triton) — parsed TXs + account updates
//!   2. Richat gRPC (self-hosted) — parsed TXs + account updates
//!   3. ShredstreamProxy gRPC (SolanaCDN/Jito) — raw entries from shreds
//!
//! The first source to deliver an event wins. Duplicates are dropped.
//! Metrics track which source is fastest per event type.

pub mod event_bus;
pub mod grpc_source;
pub mod richat_source;
pub mod shredstream_source;
pub mod types;
