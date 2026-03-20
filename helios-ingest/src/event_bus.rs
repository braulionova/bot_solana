//! Unified Event Bus — deduplicates events from all sources.
//!
//! First source to deliver wins. Metrics track source performance.

use crate::types::*;
use dashmap::DashMap;
use lru::LruCache;
use solana_sdk::signature::Signature;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info};

/// Dedup + routing metrics.
pub struct BusMetrics {
    pub events_total: AtomicU64,
    pub events_deduped: AtomicU64,
    pub events_forwarded: AtomicU64,
    /// Per-source: first-delivery wins.
    pub first_wins: DashMap<Source, AtomicU64>,
    /// Per-source: total received.
    pub source_total: DashMap<Source, AtomicU64>,
    /// Per-source: cumulative latency (ns) for averaging.
    pub source_latency_ns: DashMap<Source, AtomicU64>,
}

impl BusMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            events_total: AtomicU64::new(0),
            events_deduped: AtomicU64::new(0),
            events_forwarded: AtomicU64::new(0),
            first_wins: DashMap::new(),
            source_total: DashMap::new(),
            source_latency_ns: DashMap::new(),
        })
    }

    fn record_first(&self, source: Source) {
        self.first_wins
            .entry(source)
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_source(&self, source: Source) {
        self.source_total
            .entry(source)
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn summary(&self) -> String {
        let total = self.events_total.load(Ordering::Relaxed);
        let deduped = self.events_deduped.load(Ordering::Relaxed);
        let fwd = self.events_forwarded.load(Ordering::Relaxed);

        let mut wins = String::new();
        for entry in self.first_wins.iter() {
            let src = *entry.key();
            let n = entry.value().load(Ordering::Relaxed);
            if !wins.is_empty() { wins.push_str(", "); }
            wins.push_str(&format!("{}={}", src, n));
        }

        format!(
            "total={total} deduped={deduped} forwarded={fwd} first_wins=[{wins}]"
        )
    }
}

/// The unified event bus.
pub struct EventBus {
    /// LRU dedup for TX signatures (seen in last ~100 slots).
    seen_sigs: parking_lot::Mutex<LruCache<Signature, (Source, Instant)>>,
    /// LRU dedup for account updates (pubkey → last write_version).
    seen_accounts: DashMap<[u8; 32], u64>,
    /// Output channel for strategy engine.
    output_tx: crossbeam_channel::Sender<IngestEvent>,
    pub metrics: Arc<BusMetrics>,
}

impl EventBus {
    pub fn new(
        output_tx: crossbeam_channel::Sender<IngestEvent>,
        dedup_capacity: usize,
    ) -> Self {
        Self {
            seen_sigs: parking_lot::Mutex::new(LruCache::new(
                NonZeroUsize::new(dedup_capacity).unwrap(),
            )),
            seen_accounts: DashMap::with_capacity(10_000),
            output_tx,
            metrics: BusMetrics::new(),
        }
    }

    /// Process an incoming event. Returns true if forwarded, false if deduped.
    pub fn process(&self, event: IngestEvent) -> bool {
        self.metrics.events_total.fetch_add(1, Ordering::Relaxed);
        self.metrics.record_source(event.source);

        match &event.data {
            EventData::Transaction(tx_info) => {
                let sig = tx_info.signature;
                let mut cache = self.seen_sigs.lock();
                if cache.contains(&sig) {
                    // Duplicate — record but don't forward
                    self.metrics.events_deduped.fetch_add(1, Ordering::Relaxed);
                    debug!(
                        sig = %sig,
                        source = %event.source,
                        "deduped TX"
                    );
                    return false;
                }
                // First time seeing this TX
                cache.put(sig, (event.source, event.received_at));
                self.metrics.record_first(event.source);
            }
            EventData::AccountUpdate(acct) => {
                let key = acct.pubkey.to_bytes();
                // For account updates: forward if write_version is newer
                if let Some(existing) = self.seen_accounts.get(&key) {
                    if acct.write_version <= *existing {
                        self.metrics.events_deduped.fetch_add(1, Ordering::Relaxed);
                        return false;
                    }
                }
                self.seen_accounts.insert(key, acct.write_version);
                self.metrics.record_first(event.source);
            }
            EventData::Slot(_) => {
                // Slot updates always forwarded (no dedup)
                self.metrics.record_first(event.source);
            }
        }

        // Forward to strategy engine
        let _ = self.output_tx.try_send(event);
        self.metrics.events_forwarded.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Log metrics summary.
    pub fn log_metrics(&self) {
        info!("event-bus: {}", self.metrics.summary());
    }
}
