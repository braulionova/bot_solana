// deduper.rs – LRU-based shred deduplication.
//
// Key: hash(slot ++ index ++ shred_type_byte)
// Cache size: configurable, default 10 000 entries.

use lru::LruCache;
use parking_lot::Mutex;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;

use crate::shred_parser::{ParsedShred, ShredType};

// ---------------------------------------------------------------------------
// Dedup key
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DedupKey {
    slot: u64,
    index: u32,
    fec_set: u32,
    shred_type: u8,
}

impl DedupKey {
    fn from_shred(shred: &ParsedShred) -> Self {
        Self {
            slot: shred.slot,
            index: shred.index,
            fec_set: shred.fec_set_index,
            shred_type: match shred.shred_type {
                ShredType::Data => 0,
                ShredType::Code => 1,
            },
        }
    }

    fn fingerprint(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }
}

// ---------------------------------------------------------------------------
// Deduper
// ---------------------------------------------------------------------------

pub struct Deduper {
    cache: Mutex<LruCache<u64, ()>>,
}

impl Deduper {
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).unwrap();
        Self {
            cache: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Returns `true` if this shred is a duplicate (already seen).
    pub fn is_duplicate(&self, shred: &ParsedShred) -> bool {
        let key = DedupKey::from_shred(shred).fingerprint();
        let mut cache = self.cache.lock();
        if cache.contains(&key) {
            true
        } else {
            cache.put(key, ());
            false
        }
    }

    /// Convenience: returns `true` when the shred should be forwarded (not seen before).
    pub fn accept(&self, shred: &ParsedShred) -> bool {
        !self.is_duplicate(shred)
    }
}
