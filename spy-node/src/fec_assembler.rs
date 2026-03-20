// fec_assembler.rs – Recover missing shreds and reassemble contiguous data blocks.

use bytes::Bytes;
use reed_solomon_erasure::galois_8::ReedSolomon;
use solana_ledger::shred::{
    Error as ShredError, ReedSolomonCache, Shred, Shredder, DATA_SHREDS_PER_FEC_BLOCK,
};
use solana_sdk::hash::{hashv, Hash};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::PathBuf;
use tracing::{debug, trace, warn};

use crate::shred_parser::{CodingMeta, ParsedShred, ShredType};

const SIGNATURE_SIZE: usize = 64;
const SHRED_VARIANT_OFFSET: usize = 64;
const SLOT_OFFSET: usize = 65;
const INDEX_OFFSET: usize = 73;
const VERSION_OFFSET: usize = 77;
const FEC_SET_INDEX_OFFSET: usize = 79;
const CODING_NUM_DATA_OFFSET: usize = 83;
const CODING_NUM_CODING_OFFSET: usize = 85;
const CODING_POSITION_OFFSET: usize = 87;

const LEGACY_PAYLOAD_SIZE: usize = 1228;
const LEGACY_DATA_HEADERS_SIZE: usize = 88;
const LEGACY_CODING_HEADERS_SIZE: usize = 89;
const LEGACY_ERASURE_SHARD_SIZE: usize = 1139;
const MERKLE_DATA_PAYLOAD_SIZE: usize = 1203;
const MERKLE_PROOF_ENTRY_SIZE: usize = 20;
const MERKLE_ROOT_SIZE: usize = 32;
const MERKLE_HASH_PREFIX_LEAF: &[u8] = b"\x00SOLANA_MERKLE_SHREDS_LEAF";
const MERKLE_HASH_PREFIX_NODE: &[u8] = b"\x01SOLANA_MERKLE_SHREDS_NODE";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryVariant {
    LegacyData,
    LegacyCode,
    MerkleData {
        proof_size: u8,
        chained: bool,
        resigned: bool,
    },
    MerkleCode {
        proof_size: u8,
        chained: bool,
        resigned: bool,
    },
}

impl RecoveryVariant {
    fn from_payload(payload: &[u8]) -> Option<Self> {
        let byte = *payload.get(SHRED_VARIANT_OFFSET)?;
        match byte {
            0x5a => Some(Self::LegacyCode),
            0xa5 => Some(Self::LegacyData),
            _ => {
                let proof_size = byte & 0x0f;
                let upper = byte >> 4;
                match upper {
                    0b0100 => Some(Self::MerkleCode {
                        proof_size,
                        chained: false,
                        resigned: false,
                    }),
                    0b0110 => Some(Self::MerkleCode {
                        proof_size,
                        chained: true,
                        resigned: false,
                    }),
                    0b0111 => Some(Self::MerkleCode {
                        proof_size,
                        chained: true,
                        resigned: true,
                    }),
                    0b1000 => Some(Self::MerkleData {
                        proof_size,
                        chained: false,
                        resigned: false,
                    }),
                    0b1001 => Some(Self::MerkleData {
                        proof_size,
                        chained: true,
                        resigned: false,
                    }),
                    0b1011 => Some(Self::MerkleData {
                        proof_size,
                        chained: true,
                        resigned: true,
                    }),
                    _ => None,
                }
            }
        }
    }

    fn is_merkle(self) -> bool {
        matches!(self, Self::MerkleData { .. } | Self::MerkleCode { .. })
    }

    fn is_data(self) -> bool {
        matches!(self, Self::LegacyData | Self::MerkleData { .. })
    }

    fn proof_size(self) -> u8 {
        match self {
            Self::MerkleData { proof_size, .. } | Self::MerkleCode { proof_size, .. } => {
                proof_size
            }
            Self::LegacyData | Self::LegacyCode => 0,
        }
    }

    fn chained(self) -> bool {
        match self {
            Self::MerkleData { chained, .. } | Self::MerkleCode { chained, .. } => chained,
            Self::LegacyData | Self::LegacyCode => false,
        }
    }

    fn resigned(self) -> bool {
        match self {
            Self::MerkleData { resigned, .. } | Self::MerkleCode { resigned, .. } => resigned,
            Self::LegacyData | Self::LegacyCode => false,
        }
    }

    fn variant_byte(self) -> u8 {
        match self {
            Self::LegacyData => 0xa5,
            Self::LegacyCode => 0x5a,
            Self::MerkleCode {
                proof_size,
                chained: false,
                resigned: false,
            } => 0b0100_0000 | proof_size,
            Self::MerkleCode {
                proof_size,
                chained: true,
                resigned: false,
            } => 0b0110_0000 | proof_size,
            Self::MerkleCode {
                proof_size,
                chained: true,
                resigned: true,
            } => 0b0111_0000 | proof_size,
            Self::MerkleData {
                proof_size,
                chained: false,
                resigned: false,
            } => 0b1000_0000 | proof_size,
            Self::MerkleData {
                proof_size,
                chained: true,
                resigned: false,
            } => 0b1001_0000 | proof_size,
            Self::MerkleData {
                proof_size,
                chained: true,
                resigned: true,
            } => 0b1011_0000 | proof_size,
            Self::MerkleData {
                chained: false,
                resigned: true,
                ..
            }
            | Self::MerkleCode {
                chained: false,
                resigned: true,
                ..
            } => unreachable!("resigned merkle shreds are always chained"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FecKey {
    slot: u64,
    fec_set_index: u32,
}

struct FecSet {
    data_shreds: HashMap<u32, Shred>,
    coding_shreds: HashMap<u32, Shred>,
    coding_meta: Option<ObservedCodingMeta>,
}

impl FecSet {
    fn new() -> Self {
        Self {
            data_shreds: HashMap::new(),
            coding_shreds: HashMap::new(),
            coding_meta: None,
        }
    }

    fn add_shred(&mut self, shred: Shred) {
        if shred.is_data() {
            self.data_shreds.insert(shred.index(), shred);
        } else {
            self.coding_shreds.insert(shred.index(), shred);
        }
    }

    fn all_shreds(&self) -> Vec<Shred> {
        self.data_shreds
            .values()
            .chain(self.coding_shreds.values())
            .cloned()
            .collect()
    }

    fn observe_coding_meta(
        &mut self,
        index: u32,
        meta: CodingMeta,
    ) -> Result<(), ObservedCodingMeta> {
        let observed = ObservedCodingMeta {
            num_data_shreds: meta.num_data_shreds,
            num_coding_shreds: meta.num_coding_shreds,
            first_coding_index: meta.first_coding_index(index).ok_or(ObservedCodingMeta {
                num_data_shreds: meta.num_data_shreds,
                num_coding_shreds: meta.num_coding_shreds,
                first_coding_index: 0,
            })?,
        };
        match self.coding_meta {
            None => {
                self.coding_meta = Some(observed);
                Ok(())
            }
            Some(current) if current == observed => Ok(()),
            Some(_) => Err(observed),
        }
    }

    fn summary(&self) -> FecSetSummary {
        FecSetSummary {
            data_shreds: self.data_shreds.len(),
            coding_shreds: self.coding_shreds.len(),
            coding_meta: self.coding_meta,
        }
    }

    fn can_attempt_recovery(&self) -> bool {
        let Some(meta) = self.coding_meta else {
            return false;
        };
        if self.coding_shreds.is_empty() {
            return false;
        }
        let num_data_shreds = usize::from(meta.num_data_shreds);
        let present = self.data_shreds.len() + self.coding_shreds.len();
        self.data_shreds.len() < num_data_shreds && present >= num_data_shreds
    }

    fn maybe_recover(&mut self, rs_cache: &ReedSolomonCache) -> Result<Vec<Shred>, ShredError> {
        if self.coding_shreds.is_empty() {
            return Ok(Vec::new());
        }

        let recovered = match self.recovery_variant() {
            Some(variant) if variant.is_merkle() => self.maybe_recover_merkle().map_err(|err| {
                warn!(error = %err, "merkle FEC recovery failed (inner)");
                ShredError::InvalidRecoveredShred
            })?,
            _ => match Shredder::try_recovery(self.all_shreds(), rs_cache) {
                Ok(recovered) => recovered,
                Err(err) => {
                    trace!(error = ?err, "FEC recovery skipped");
                    return Err(err);
                }
            },
        };

        let mut new_data = Vec::new();
        for shred in recovered {
            if shred.is_data() && !self.data_shreds.contains_key(&shred.index()) {
                self.data_shreds.insert(shred.index(), shred.clone());
                new_data.push(shred);
            }
        }
        Ok(new_data)
    }

    fn recovery_variant(&self) -> Option<RecoveryVariant> {
        self.data_shreds
            .values()
            .chain(self.coding_shreds.values())
            .find_map(|shred| RecoveryVariant::from_payload(shred.payload()))
    }

    fn maybe_recover_merkle(&self) -> Result<Vec<Shred>, String> {
        let variant = self
            .recovery_variant()
            .ok_or_else(|| "missing merkle variant".to_string())?;
        let coding_meta = self
            .coding_meta
            .ok_or_else(|| "missing coding meta for merkle recovery".to_string())?;
        let first_code = self
            .coding_shreds
            .values()
            .next()
            .ok_or_else(|| "missing coding shred for merkle recovery".to_string())?;
        let first_code_payload = first_code.payload();
        let signature = first_code_payload
            .get(..SIGNATURE_SIZE)
            .ok_or_else(|| "missing signature".to_string())?;
        let slot = parse_u64(first_code_payload, SLOT_OFFSET)?;
        let version = parse_u16(first_code_payload, VERSION_OFFSET)?;
        let fec_set_index = parse_u32(first_code_payload, FEC_SET_INDEX_OFFSET)?;
        let chained_merkle_root = extract_chained_merkle_root(first_code_payload, variant)
            .or_else(|| {
                self.data_shreds
                    .values()
                    .find_map(|shred| extract_chained_merkle_root(shred.payload(), variant))
            });

        let num_data = usize::from(coding_meta.num_data_shreds);
        let num_coding = usize::from(coding_meta.num_coding_shreds);
        let total_shards = num_data + num_coding;
        let mut present_payloads = vec![None; total_shards];
        let mut shards = vec![None; total_shards];

        for shred in self.data_shreds.values().chain(self.coding_shreds.values()) {
            let payload = shred.payload().clone();
            let index = erasure_shard_index(&payload)
                .ok_or_else(|| format!("invalid erasure index for slot={slot} fec={fec_set_index}"))?;
            if index >= total_shards {
                return Err(format!(
                    "erasure index out of range slot={slot} fec={fec_set_index} index={index} total={total_shards}"
                ));
            }
            present_payloads[index] = Some(payload.clone());
            shards[index] = Some(erasure_shard_bytes(&payload)?);
        }

        ReedSolomon::new(num_data, num_coding)
            .map_err(|err| format!("reed-solomon init failed: {err}"))?
            .reconstruct(&mut shards)
            .map_err(|err| format!("merkle reconstruct failed: {err}"))?;

        let mut full_payloads = vec![Vec::new(); total_shards];
        let mut nodes = vec![Hash::default(); total_shards];
        for index in 0..total_shards {
            let payload = if let Some(payload) = present_payloads[index].clone() {
                payload
            } else if index < num_data {
                build_recovered_merkle_data_payload(
                    signature,
                    shards[index]
                        .clone()
                        .ok_or_else(|| "missing reconstructed data shard".to_string())?,
                    variant,
                    chained_merkle_root,
                )?
            } else {
                build_recovered_merkle_code_payload(
                    signature,
                    slot,
                    coding_meta.first_coding_index + (index - num_data) as u32,
                    version,
                    fec_set_index,
                    coding_meta.num_data_shreds,
                    coding_meta.num_coding_shreds,
                    (index - num_data) as u16,
                    shards[index]
                        .clone()
                        .ok_or_else(|| "missing reconstructed coding shard".to_string())?,
                    variant,
                    chained_merkle_root,
                )?
            };
            nodes[index] = merkle_node(&payload, variant)?;
            full_payloads[index] = payload;
        }

        let tree = make_merkle_tree(nodes);
        let mut recovered = Vec::new();
        for index in 0..num_data {
            if present_payloads[index].is_some() {
                continue;
            }
            let mut payload = full_payloads[index].clone();
            let proof = make_merkle_proof(index, total_shards, &tree)
                .ok_or_else(|| "failed to build merkle proof".to_string())?;
            set_merkle_proof(&mut payload, variant, &proof)?;
            let shred = Shred::new_from_serialized_shred(payload)
                .map_err(|err| format!("failed to deserialize recovered merkle data shred: {err:?}"))?;
            recovered.push(shred);
        }
        Ok(recovered)
    }
}

#[derive(Default)]
struct SlotBuffer {
    data_shreds: BTreeMap<u32, Shred>,
    completed_data_indexes: BTreeSet<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ObservedCodingMeta {
    num_data_shreds: u16,
    num_coding_shreds: u16,
    first_coding_index: u32,
}

#[derive(Debug, Clone, Copy)]
struct FecSetSummary {
    data_shreds: usize,
    coding_shreds: usize,
    coding_meta: Option<ObservedCodingMeta>,
}

#[derive(Debug, Clone)]
struct RecoveryLayout {
    data_expected: usize,
    coding_expected: usize,
    total_expected: usize,
    data_in_range: usize,
    data_out_of_range: usize,
    coding_in_range: usize,
    coding_out_of_range: usize,
    occupied_erasure_slots: usize,
    missing_data_slots: usize,
    min_data_index: Option<u32>,
    max_data_index: Option<u32>,
    min_code_index: Option<u32>,
    max_code_index: Option<u32>,
}

pub struct FecAssembler {
    sets: HashMap<FecKey, FecSet>,
    slots: HashMap<u64, SlotBuffer>,
    complete: Vec<(u64, u32, Vec<Bytes>)>,
    rs_cache: ReedSolomonCache,
    recovery_attempts: u64,
    merkle_recovery_attempts: u64,
    legacy_recovery_attempts: u64,
    recovered_data_shreds: u64,
    recovery_errors: u64,
    recovery_invalid_index: u64,
    recovery_invalid_shard_size: u64,
    recovery_too_few_shards: u64,
    recovery_other_errors: u64,
    recovery_empty_results: u64,
    deshred_attempts: u64,
    deshred_failures: u64,
    code_header_inconsistencies: u64,
    code_header_invalid: u64,
    sampled_recovery_logs: u64,
    sampled_header_logs: u64,
    sampled_empty_recovery_logs: u64,
    sampled_empty_recovery_dumps: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FecStats {
    pub sets: usize,
    pub slot_buffers: usize,
    pub buffered_data_shreds: usize,
    pub completed_boundaries: usize,
    pub sets_with_coding: usize,
    pub sets_at_data_threshold: usize,
    pub sets_with_coding_meta: usize,
    pub sets_missing_coding_meta: usize,
    pub avg_shreds_per_set_x10: usize,
    pub max_data_in_set: usize,
    pub sets_all_data_present: usize,
    pub recovery_attempts: u64,
    pub merkle_recovery_attempts: u64,
    pub legacy_recovery_attempts: u64,
    pub recovered_data_shreds: u64,
    pub recovery_errors: u64,
    pub recovery_invalid_index: u64,
    pub recovery_invalid_shard_size: u64,
    pub recovery_too_few_shards: u64,
    pub recovery_other_errors: u64,
    pub recovery_empty_results: u64,
    pub deshred_attempts: u64,
    pub deshred_failures: u64,
    pub code_header_inconsistencies: u64,
    pub code_header_invalid: u64,
}

impl FecAssembler {
    pub fn new() -> Self {
        Self {
            sets: HashMap::new(),
            slots: HashMap::new(),
            complete: Vec::new(),
            rs_cache: ReedSolomonCache::default(),
            recovery_attempts: 0,
            merkle_recovery_attempts: 0,
            legacy_recovery_attempts: 0,
            recovered_data_shreds: 0,
            recovery_errors: 0,
            recovery_invalid_index: 0,
            recovery_invalid_shard_size: 0,
            recovery_too_few_shards: 0,
            recovery_other_errors: 0,
            recovery_empty_results: 0,
            deshred_attempts: 0,
            deshred_failures: 0,
            code_header_inconsistencies: 0,
            code_header_invalid: 0,
            sampled_recovery_logs: 0,
            sampled_header_logs: 0,
            sampled_empty_recovery_logs: 0,
            sampled_empty_recovery_dumps: 0,
        }
    }

    pub fn ingest(&mut self, shred: &ParsedShred) -> bool {
        let sol_shred = shred.shred.clone();

        let key = FecKey {
            slot: shred.slot,
            fec_set_index: shred.fec_set_index,
        };

        let mut inconsistent_header = None;
        let mut invalid_header = None;
        let (recovered, set_summary, attempted_recovery, recovery_variant) = {
            let set = self.sets.entry(key).or_insert_with(FecSet::new);
            if let Some(meta) = shred.coding_meta {
                match set.observe_coding_meta(shred.index, meta) {
                    Ok(()) => {}
                    Err(observed) if meta.first_coding_index(shred.index).is_none() => {
                        invalid_header = Some(meta);
                    }
                    Err(observed) => {
                        inconsistent_header = Some((set.coding_meta, observed));
                    }
                }
            }
            set.add_shred(sol_shred.clone());
            let attempted_recovery = set.can_attempt_recovery();
            let recovery_variant = set.recovery_variant();
            let recovered = if attempted_recovery {
                self.recovery_attempts = self.recovery_attempts.saturating_add(1);
                if matches!(recovery_variant, Some(variant) if variant.is_merkle()) {
                    self.merkle_recovery_attempts =
                        self.merkle_recovery_attempts.saturating_add(1);
                } else {
                    self.legacy_recovery_attempts =
                        self.legacy_recovery_attempts.saturating_add(1);
                }
                set.maybe_recover(&self.rs_cache)
            } else {
                Ok(Vec::new())
            };
            (recovered, set.summary(), attempted_recovery, recovery_variant)
        };
        if let Some(meta) = invalid_header {
            self.code_header_invalid = self.code_header_invalid.saturating_add(1);
            if self.sampled_header_logs < 10 {
                warn!(
                    slot = shred.slot,
                    fec_set_index = shred.fec_set_index,
                    index = shred.index,
                    num_data_shreds = meta.num_data_shreds,
                    num_coding_shreds = meta.num_coding_shreds,
                    position = meta.position,
                    "invalid coding header: position exceeds shred index"
                );
                self.sampled_header_logs += 1;
            }
        }
        if let Some((expected, observed)) = inconsistent_header {
            self.code_header_inconsistencies = self.code_header_inconsistencies.saturating_add(1);
            if self.sampled_header_logs < 10 {
                warn!(
                    slot = shred.slot,
                    fec_set_index = shred.fec_set_index,
                    index = shred.index,
                    data_shreds = set_summary.data_shreds,
                    coding_shreds = set_summary.coding_shreds,
                    expected = ?expected,
                    observed = ?observed,
                    "mixed coding headers inside fec set"
                );
                self.sampled_header_logs += 1;
            }
        }

        let recovered = match recovered {
            Ok(recovered) => {
                if attempted_recovery && recovered.is_empty() && set_summary.coding_meta.is_some() {
                    self.record_empty_recovery(key, set_summary, recovery_variant);
                }
                self.recovered_data_shreds = self
                    .recovered_data_shreds
                    .saturating_add(recovered.len() as u64);
                recovered
            }
            Err(err) => {
                self.record_recovery_error(key, set_summary, &err);
                Vec::new()
            }
        };

        if matches!(shred.shred_type, ShredType::Data) {
            self.insert_data_shred(shred.slot, sol_shred);
        }

        for recovered in recovered {
            self.insert_data_shred(shred.slot, recovered);
        }

        let emitted = self.emit_ready_blocks(shred.slot);

        emitted
    }

    fn insert_data_shred(&mut self, slot: u64, shred: Shred) {
        let index = shred.index();
        let slot_buf = self.slots.entry(slot).or_default();
        if shred.data_complete() || shred.last_in_slot() {
            slot_buf.completed_data_indexes.insert(index);
        }
        slot_buf.data_shreds.entry(index).or_insert(shred);
    }

    fn emit_ready_blocks(&mut self, slot: u64) -> bool {
        let Some(slot_buf) = self.slots.get_mut(&slot) else {
            return false;
        };

        let mut emitted = false;

        loop {
            let Some((indices, shreds)) =
                find_complete_data_block(&slot_buf.data_shreds, &slot_buf.completed_data_indexes)
            else {
                break;
            };
            let start_index = *indices.first().unwrap();
            let end_index = *indices.last().unwrap();
            self.deshred_attempts = self.deshred_attempts.saturating_add(1);

            match Shredder::deshred(&shreds) {
                Ok(payload) => {
                    debug!(
                        slot,
                        start = start_index,
                        end = end_index,
                        bytes = payload.len(),
                        "completed data block"
                    );
                    self.complete
                        .push((slot, start_index, vec![Bytes::from(payload)]));
                    for index in &indices {
                        slot_buf.data_shreds.remove(&index);
                    }
                    emitted = true;
                }
                Err(err) => {
                    self.deshred_failures = self.deshred_failures.saturating_add(1);
                    warn!(
                        slot,
                        start = start_index,
                        end = end_index,
                        error = ?err,
                        "deshred failed"
                    );
                    for index in indices {
                        slot_buf.data_shreds.remove(&index);
                    }
                }
            }
        }

        emitted
    }

    pub fn drain_complete(&mut self) -> Vec<(u64, u32, Vec<Bytes>)> {
        std::mem::take(&mut self.complete)
    }

    pub fn prune_old_slots(&mut self, min_slot: u64) {
        self.sets.retain(|k, _| k.slot >= min_slot);
        self.slots.retain(|slot, _| *slot >= min_slot);
    }

    pub fn stats_snapshot(&self) -> FecStats {
        let mut stats = FecStats {
            sets: self.sets.len(),
            slot_buffers: self.slots.len(),
            recovery_attempts: self.recovery_attempts,
            merkle_recovery_attempts: self.merkle_recovery_attempts,
            legacy_recovery_attempts: self.legacy_recovery_attempts,
            recovered_data_shreds: self.recovered_data_shreds,
            recovery_errors: self.recovery_errors,
            recovery_invalid_index: self.recovery_invalid_index,
            recovery_invalid_shard_size: self.recovery_invalid_shard_size,
            recovery_too_few_shards: self.recovery_too_few_shards,
            recovery_other_errors: self.recovery_other_errors,
            recovery_empty_results: self.recovery_empty_results,
            deshred_attempts: self.deshred_attempts,
            deshred_failures: self.deshred_failures,
            code_header_inconsistencies: self.code_header_inconsistencies,
            code_header_invalid: self.code_header_invalid,
            ..FecStats::default()
        };

        let mut total_shreds_in_sets: usize = 0;
        for set in self.sets.values() {
            let present = set.data_shreds.len() + set.coding_shreds.len();
            total_shreds_in_sets += present;
            if set.data_shreds.len() > stats.max_data_in_set {
                stats.max_data_in_set = set.data_shreds.len();
            }
            if set.coding_meta.is_some() {
                stats.sets_with_coding_meta += 1;
                let num_data = usize::from(set.coding_meta.unwrap().num_data_shreds);
                if set.data_shreds.len() >= num_data {
                    stats.sets_all_data_present += 1;
                }
            } else {
                stats.sets_missing_coding_meta += 1;
            }
            if !set.coding_shreds.is_empty() {
                stats.sets_with_coding += 1;
                let num_data_shreds = set
                    .coding_meta
                    .map(|meta| usize::from(meta.num_data_shreds))
                    .unwrap_or(DATA_SHREDS_PER_FEC_BLOCK);
                if set.data_shreds.len() < num_data_shreds
                    && present >= num_data_shreds
                {
                    stats.sets_at_data_threshold += 1;
                }
            }
        }
        if !self.sets.is_empty() {
            stats.avg_shreds_per_set_x10 = total_shreds_in_sets * 10 / self.sets.len();
        }

        for slot in self.slots.values() {
            stats.buffered_data_shreds += slot.data_shreds.len();
            stats.completed_boundaries += slot.completed_data_indexes.len();
        }

        stats
    }

    fn record_recovery_error(&mut self, key: FecKey, summary: FecSetSummary, err: &ShredError) {
        self.recovery_errors = self.recovery_errors.saturating_add(1);

        let err_text = err.to_string();
        if err_text.contains("InvalidIndex") || err_text.contains("invalid index") {
            self.recovery_invalid_index = self.recovery_invalid_index.saturating_add(1);
        } else if err_text.contains("Too few shards")
            || err_text.contains("too few shards")
            || err_text.contains("TooFewShards")
        {
            self.recovery_too_few_shards = self.recovery_too_few_shards.saturating_add(1);
        } else if err_text.contains("Invalid shard size") {
            self.recovery_invalid_shard_size = self.recovery_invalid_shard_size.saturating_add(1);
        } else {
            self.recovery_other_errors = self.recovery_other_errors.saturating_add(1);
        }

        if self.sampled_recovery_logs < 10 {
            warn!(
                slot = key.slot,
                fec_set_index = key.fec_set_index,
                data_shreds = summary.data_shreds,
                coding_shreds = summary.coding_shreds,
                coding_meta = ?summary.coding_meta,
                error = %err,
                "fec recovery failed"
            );
            self.sampled_recovery_logs += 1;
        }
    }

    fn record_empty_recovery(
        &mut self,
        key: FecKey,
        summary: FecSetSummary,
        variant: Option<RecoveryVariant>,
    ) {
        self.recovery_empty_results = self.recovery_empty_results.saturating_add(1);
        let Some(set) = self.sets.get(&key) else {
            return;
        };
        if matches!(variant, Some(variant) if variant.is_merkle())
            && self.sampled_empty_recovery_dumps < 3
        {
            if let Err(err) = dump_empty_recovery_set(key, set, summary, variant) {
                warn!(
                    slot = key.slot,
                    fec_set_index = key.fec_set_index,
                    error = %err,
                    "failed to dump empty merkle recovery set"
                );
            } else {
                self.sampled_empty_recovery_dumps += 1;
            }
        }
        if self.sampled_empty_recovery_logs >= 10 {
            return;
        }
        let Some(layout) = set.recovery_layout(key.fec_set_index) else {
            return;
        };
        warn!(
            slot = key.slot,
            fec_set_index = key.fec_set_index,
            data_shreds = summary.data_shreds,
            coding_shreds = summary.coding_shreds,
            coding_meta = ?summary.coding_meta,
            recovery_variant = ?variant,
            layout = ?layout,
            "fec recovery returned no data shreds"
        );
        self.sampled_empty_recovery_logs += 1;
    }
}

impl FecSet {
    fn recovery_layout(&self, fec_set_index: u32) -> Option<RecoveryLayout> {
        let meta = self.coding_meta?;
        let data_expected = usize::from(meta.num_data_shreds);
        let coding_expected = usize::from(meta.num_coding_shreds);
        let total_expected = data_expected.checked_add(coding_expected)?;

        let data_start = fec_set_index;
        let data_end = data_start.checked_add(meta.num_data_shreds.saturating_sub(1) as u32)?;
        let code_start = meta.first_coding_index;
        let code_end = code_start.checked_add(meta.num_coding_shreds.saturating_sub(1) as u32)?;

        let min_data_index = self.data_shreds.keys().min().copied();
        let max_data_index = self.data_shreds.keys().max().copied();
        let min_code_index = self.coding_shreds.keys().min().copied();
        let max_code_index = self.coding_shreds.keys().max().copied();

        let data_in_range = self
            .data_shreds
            .keys()
            .filter(|&&index| (data_start..=data_end).contains(&index))
            .count();
        let data_out_of_range = self.data_shreds.len().saturating_sub(data_in_range);
        let coding_in_range = self
            .coding_shreds
            .keys()
            .filter(|&&index| (code_start..=code_end).contains(&index))
            .count();
        let coding_out_of_range = self.coding_shreds.len().saturating_sub(coding_in_range);

        let mut occupied = BTreeSet::new();
        for &index in self.data_shreds.keys() {
            if let Some(relative) = index.checked_sub(fec_set_index) {
                let relative = relative as usize;
                if relative < total_expected {
                    occupied.insert(relative);
                }
            }
        }
        for &index in self.coding_shreds.keys() {
            if let Some(relative) = index.checked_sub(code_start) {
                let relative = data_expected + relative as usize;
                if relative < total_expected {
                    occupied.insert(relative);
                }
            }
        }

        Some(RecoveryLayout {
            data_expected,
            coding_expected,
            total_expected,
            data_in_range,
            data_out_of_range,
            coding_in_range,
            coding_out_of_range,
            occupied_erasure_slots: occupied.len(),
            missing_data_slots: data_expected.saturating_sub(data_in_range),
            min_data_index,
            max_data_index,
            min_code_index,
            max_code_index,
        })
    }
}

fn find_complete_data_block(
    data_shreds: &BTreeMap<u32, Shred>,
    completed_data_indexes: &BTreeSet<u32>,
) -> Option<(Vec<u32>, Vec<Shred>)> {
    // On Turbine we often join a slot mid-flight and never receive the first
    // FEC sets.  Starting start_index at 0 means we can never satisfy
    // `count == expected` for any boundary above our first available shred
    // because the range always includes the missing leading shreds.
    //
    // Fix: for each DATA_COMPLETE boundary, if start_index falls before our
    // first available shred, skip that boundary (we can't reconstruct the
    // beginning of that entry batch) and advance past it.  Once start_index
    // is at or beyond first_available, every subsequent boundary is a valid
    // contiguous batch that we CAN assemble.
    let first_available = *data_shreds.keys().next()?;
    let mut start_index = 0u32;

    for &end_index in completed_data_indexes {
        // If the block starts before our first shred, we're missing the
        // beginning of this entry batch — skip it.
        if start_index < first_available {
            start_index = end_index.saturating_add(1);
            continue;
        }

        let count = data_shreds.range(start_index..=end_index).count() as u32;
        let expected = end_index.saturating_sub(start_index).saturating_add(1);
        if count == expected {
            let indices: Vec<u32> = data_shreds
                .range(start_index..=end_index)
                .map(|(&index, _)| index)
                .collect();
            let shreds: Vec<Shred> = data_shreds
                .range(start_index..=end_index)
                .map(|(_, shred)| shred.clone())
                .collect();
            return Some((indices, shreds));
        }
        start_index = end_index.saturating_add(1);
    }

    None
}

fn dump_empty_recovery_set(
    key: FecKey,
    set: &FecSet,
    summary: FecSetSummary,
    variant: Option<RecoveryVariant>,
) -> Result<(), String> {
    let dir = PathBuf::from(format!(
        "/tmp/spy-node-fec-dumps/slot-{}-fec-{}",
        key.slot, key.fec_set_index
    ));
    fs::create_dir_all(&dir).map_err(|err| format!("create dump dir failed: {err}"))?;

    let mut meta = String::new();
    meta.push_str(&format!("slot={}\n", key.slot));
    meta.push_str(&format!("fec_set_index={}\n", key.fec_set_index));
    meta.push_str(&format!("variant={variant:?}\n"));
    meta.push_str(&format!("data_shreds={}\n", summary.data_shreds));
    meta.push_str(&format!("coding_shreds={}\n", summary.coding_shreds));
    meta.push_str(&format!("coding_meta={:?}\n", summary.coding_meta));
    fs::write(dir.join("meta.txt"), meta)
        .map_err(|err| format!("write dump metadata failed: {err}"))?;

    for shred in set.data_shreds.values() {
        let path = dir.join(format!("data-{}.bin", shred.index()));
        fs::write(path, shred.payload()).map_err(|err| format!("write data shred failed: {err}"))?;
    }
    for shred in set.coding_shreds.values() {
        let path = dir.join(format!("code-{}.bin", shred.index()));
        fs::write(path, shred.payload()).map_err(|err| format!("write code shred failed: {err}"))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use solana_entry::entry::Entry;
    use solana_ledger::shred::{ProcessShredsStats, Shredder};
    use solana_sdk::{
        hash::Hash,
        signature::{Keypair, Signer},
        system_instruction,
        transaction::Transaction,
    };

    use crate::{shred_parser::parse_shred, tx_decoder::TxDecoder};

    fn make_tx(lamports: u64) -> Transaction {
        let payer = Keypair::new();
        let recipient = Keypair::new();
        Transaction::new_signed_with_payer(
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient.pubkey(),
                lamports,
            )],
            Some(&payer.pubkey()),
            &[&payer],
            Hash::new_unique(),
        )
    }

    fn make_data_block(
        shredder: &Shredder,
        next_shred_index: u32,
        is_last_in_slot: bool,
        lamports: u64,
        tx_count: usize,
        merkle_variant: bool,
    ) -> Vec<Shred> {
        let txs = (0..tx_count)
            .map(|offset| make_tx(lamports + offset as u64))
            .collect();
        let entry = Entry::new(&Hash::new_unique(), 1, txs);
        let mut stats = ProcessShredsStats::default();
        let (data, _coding) = shredder.entries_to_shreds(
            &Keypair::new(),
            &[entry],
            is_last_in_slot,
            None,
            next_shred_index,
            next_shred_index + 64,
            merkle_variant,
            &ReedSolomonCache::default(),
            &mut stats,
        );
        if merkle_variant {
            data.into_iter().chain(_coding).collect()
        } else {
            data
        }
    }

    #[test]
    fn ingest_can_emit_complete_block_after_resyncing_to_previous_boundary() {
        let shredder = Shredder::new(100, 99, 0, 1).unwrap();
        let first_block = make_data_block(&shredder, 0, false, 1, 32, false);
        assert!(first_block.len() > 1);
        let second_block =
            make_data_block(&shredder, first_block.len() as u32, false, 2, 1, false);

        let mut assembler = FecAssembler::new();
        let first_block_tail = first_block.last().unwrap().clone();
        let parsed_tail = parse_shred(Bytes::from(first_block_tail.into_payload())).unwrap();
        assembler.ingest(&parsed_tail);

        for shred in second_block {
            let parsed = parse_shred(Bytes::from(shred.into_payload())).unwrap();
            assembler.ingest(&parsed);
        }

        let complete = assembler.drain_complete();
        assert_eq!(complete.len(), 1);
        assert_eq!(complete[0].1, first_block.len() as u32);

        let txs = TxDecoder::new(crate::tx_decoder::new_alt_cache())
            .decode_batch(complete[0].0, &complete[0].2);
        assert_eq!(txs.len(), 1);
    }

    #[test]
    fn ingest_can_recover_missing_merkle_data_shred() {
        let shredder = Shredder::new(100, 99, 0, 1).unwrap();
        let shreds = make_data_block(&shredder, 0, false, 10, 256, true);
        let mut data = Vec::new();
        let mut coding = Vec::new();
        for shred in shreds {
            if shred.is_data() {
                data.push(shred);
            } else {
                coding.push(shred);
            }
        }
        assert!(!coding.is_empty());
        let missing_index = data
            .iter()
            .find(|shred| shred.data_complete())
            .map(Shred::index)
            .unwrap_or_else(|| data.last().unwrap().index());
        let mut assembler = FecAssembler::new();

        for shred in coding {
            let parsed = parse_shred(Bytes::from(shred.into_payload())).unwrap();
            assembler.ingest(&parsed);
        }
        for shred in data.into_iter().filter(|shred| shred.index() != missing_index) {
            let parsed = parse_shred(Bytes::from(shred.into_payload())).unwrap();
            assembler.ingest(&parsed);
        }

        let complete = assembler.drain_complete();
        assert_eq!(complete.len(), 1);
        let txs = TxDecoder::new(crate::tx_decoder::new_alt_cache())
            .decode_batch(complete[0].0, &complete[0].2);
        assert!(!txs.is_empty());
    }
}

impl Default for FecAssembler {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_u16(payload: &[u8], offset: usize) -> Result<u16, String> {
    let bytes: [u8; 2] = payload
        .get(offset..offset + 2)
        .ok_or_else(|| format!("missing u16 at offset {offset}"))?
        .try_into()
        .map_err(|_| format!("invalid u16 at offset {offset}"))?;
    Ok(u16::from_le_bytes(bytes))
}

fn parse_u32(payload: &[u8], offset: usize) -> Result<u32, String> {
    let bytes: [u8; 4] = payload
        .get(offset..offset + 4)
        .ok_or_else(|| format!("missing u32 at offset {offset}"))?
        .try_into()
        .map_err(|_| format!("invalid u32 at offset {offset}"))?;
    Ok(u32::from_le_bytes(bytes))
}

fn parse_u64(payload: &[u8], offset: usize) -> Result<u64, String> {
    let bytes: [u8; 8] = payload
        .get(offset..offset + 8)
        .ok_or_else(|| format!("missing u64 at offset {offset}"))?
        .try_into()
        .map_err(|_| format!("invalid u64 at offset {offset}"))?;
    Ok(u64::from_le_bytes(bytes))
}

fn erasure_shard_index(payload: &[u8]) -> Option<usize> {
    let variant = RecoveryVariant::from_payload(payload)?;
    match variant {
        RecoveryVariant::LegacyData | RecoveryVariant::MerkleData { .. } => {
            let index = parse_u32(payload, INDEX_OFFSET).ok()?;
            let fec_set_index = parse_u32(payload, FEC_SET_INDEX_OFFSET).ok()?;
            usize::try_from(index.checked_sub(fec_set_index)?).ok()
        }
        RecoveryVariant::LegacyCode | RecoveryVariant::MerkleCode { .. } => {
            let num_data = usize::from(parse_u16(payload, CODING_NUM_DATA_OFFSET).ok()?);
            let position = usize::from(parse_u16(payload, CODING_POSITION_OFFSET).ok()?);
            num_data.checked_add(position)
        }
    }
}

fn erasure_shard_bytes(payload: &[u8]) -> Result<Vec<u8>, String> {
    let variant = RecoveryVariant::from_payload(payload)
        .ok_or_else(|| "unknown shred variant while extracting erasure shard".to_string())?;
    match variant {
        RecoveryVariant::LegacyData => {
            if payload.len() != LEGACY_PAYLOAD_SIZE {
                return Err(format!("invalid legacy data payload size {}", payload.len()));
            }
            Ok(payload[..LEGACY_ERASURE_SHARD_SIZE].to_vec())
        }
        RecoveryVariant::LegacyCode => {
            if payload.len() != LEGACY_PAYLOAD_SIZE {
                return Err(format!("invalid legacy code payload size {}", payload.len()));
            }
            Ok(payload[LEGACY_CODING_HEADERS_SIZE..].to_vec())
        }
        RecoveryVariant::MerkleData { .. } => {
            let offset = merkle_proof_offset(variant)?;
            Ok(payload[SIGNATURE_SIZE..offset].to_vec())
        }
        RecoveryVariant::MerkleCode { .. } => {
            let offset = merkle_proof_offset(variant)?;
            Ok(payload[LEGACY_CODING_HEADERS_SIZE..offset].to_vec())
        }
    }
}

fn merkle_code_capacity(variant: RecoveryVariant) -> Result<usize, String> {
    let proof = usize::from(variant.proof_size()) * MERKLE_PROOF_ENTRY_SIZE;
    LEGACY_PAYLOAD_SIZE
        .checked_sub(
            LEGACY_CODING_HEADERS_SIZE
                + if variant.chained() { MERKLE_ROOT_SIZE } else { 0 }
                + proof
                + if variant.resigned() { SIGNATURE_SIZE } else { 0 },
        )
        .ok_or_else(|| "invalid merkle code capacity".to_string())
}

fn merkle_data_capacity(variant: RecoveryVariant) -> Result<usize, String> {
    let proof = usize::from(variant.proof_size()) * MERKLE_PROOF_ENTRY_SIZE;
    MERKLE_DATA_PAYLOAD_SIZE
        .checked_sub(
            LEGACY_DATA_HEADERS_SIZE
                + if variant.chained() { MERKLE_ROOT_SIZE } else { 0 }
                + proof
                + if variant.resigned() { SIGNATURE_SIZE } else { 0 },
        )
        .ok_or_else(|| "invalid merkle data capacity".to_string())
}

fn merkle_proof_offset(variant: RecoveryVariant) -> Result<usize, String> {
    let base = if variant.is_data() {
        LEGACY_DATA_HEADERS_SIZE + merkle_data_capacity(variant)?
    } else {
        LEGACY_CODING_HEADERS_SIZE + merkle_code_capacity(variant)?
    };
    Ok(base + if variant.chained() { MERKLE_ROOT_SIZE } else { 0 })
}

fn extract_chained_merkle_root(payload: &[u8], variant: RecoveryVariant) -> Option<[u8; 32]> {
    if !variant.chained() {
        return None;
    }
    let offset = if variant.is_data() {
        LEGACY_DATA_HEADERS_SIZE + merkle_data_capacity(variant).ok()?
    } else {
        LEGACY_CODING_HEADERS_SIZE + merkle_code_capacity(variant).ok()?
    };
    payload
        .get(offset..offset + MERKLE_ROOT_SIZE)?
        .try_into()
        .ok()
}

fn build_recovered_merkle_data_payload(
    signature: &[u8],
    mut shard: Vec<u8>,
    variant: RecoveryVariant,
    chained_merkle_root: Option<[u8; 32]>,
) -> Result<Vec<u8>, String> {
    let shard_size = shard.len();
    // Shard bytes = payload[SIGNATURE_SIZE..proof_offset] for both data and code shreds.
    // For chained Merkle shreds the chained root is part of the shard, so the expected
    // size is LEGACY_PAYLOAD_SIZE - LEGACY_CODING_HEADERS_SIZE minus proof/resigned bytes
    // (same formula for both data and code variants so RS shards are consistent).
    let proof_bytes = usize::from(variant.proof_size()) * MERKLE_PROOF_ENTRY_SIZE;
    let resigned_bytes = if variant.resigned() { SIGNATURE_SIZE } else { 0 };
    let expected = LEGACY_PAYLOAD_SIZE
        .saturating_sub(LEGACY_CODING_HEADERS_SIZE + proof_bytes + resigned_bytes);
    if shard_size != expected {
        return Err(format!(
            "unexpected recovered merkle data shard size {shard_size}, expected {expected} \
             (proof_size={}, chained={}, resigned={})",
            variant.proof_size(),
            variant.chained(),
            variant.resigned(),
        ));
    }
    if shard_size + SIGNATURE_SIZE > MERKLE_DATA_PAYLOAD_SIZE {
        return Err(format!("merkle data shard too large: {shard_size}"));
    }
    shard.resize(MERKLE_DATA_PAYLOAD_SIZE, 0);
    shard.copy_within(0..shard_size, SIGNATURE_SIZE);
    shard[..SIGNATURE_SIZE].copy_from_slice(signature);
    if let Some(root) = chained_merkle_root {
        let offset = LEGACY_DATA_HEADERS_SIZE + merkle_data_capacity(variant)?;
        shard[offset..offset + MERKLE_ROOT_SIZE].copy_from_slice(&root);
    }
    Ok(shard)
}

#[allow(clippy::too_many_arguments)]
fn build_recovered_merkle_code_payload(
    signature: &[u8],
    slot: u64,
    index: u32,
    version: u16,
    fec_set_index: u32,
    num_data_shreds: u16,
    num_coding_shreds: u16,
    position: u16,
    mut shard: Vec<u8>,
    variant: RecoveryVariant,
    chained_merkle_root: Option<[u8; 32]>,
) -> Result<Vec<u8>, String> {
    let shard_size = shard.len();
    // Same formula as erasure_shard_bytes for code: payload[LEGACY_CODING_HEADERS_SIZE..proof_offset]
    // which includes the chained root bytes for chained shreds (not subtracted here).
    let proof_bytes = usize::from(variant.proof_size()) * MERKLE_PROOF_ENTRY_SIZE;
    let resigned_bytes = if variant.resigned() { SIGNATURE_SIZE } else { 0 };
    let expected = LEGACY_PAYLOAD_SIZE
        .saturating_sub(LEGACY_CODING_HEADERS_SIZE + proof_bytes + resigned_bytes);
    if shard_size != expected {
        return Err(format!(
            "unexpected recovered merkle code shard size {shard_size}, expected {expected} \
             (proof_size={}, chained={}, resigned={})",
            variant.proof_size(),
            variant.chained(),
            variant.resigned(),
        ));
    }
    if shard_size + LEGACY_CODING_HEADERS_SIZE > LEGACY_PAYLOAD_SIZE {
        return Err(format!("merkle code shard too large: {shard_size}"));
    }
    shard.resize(LEGACY_PAYLOAD_SIZE, 0);
    shard.copy_within(0..shard_size, LEGACY_CODING_HEADERS_SIZE);
    shard[..SIGNATURE_SIZE].copy_from_slice(signature);
    shard[SHRED_VARIANT_OFFSET] = variant.variant_byte();
    shard[SLOT_OFFSET..SLOT_OFFSET + 8].copy_from_slice(&slot.to_le_bytes());
    shard[INDEX_OFFSET..INDEX_OFFSET + 4].copy_from_slice(&index.to_le_bytes());
    shard[VERSION_OFFSET..VERSION_OFFSET + 2].copy_from_slice(&version.to_le_bytes());
    shard[FEC_SET_INDEX_OFFSET..FEC_SET_INDEX_OFFSET + 4].copy_from_slice(&fec_set_index.to_le_bytes());
    shard[CODING_NUM_DATA_OFFSET..CODING_NUM_DATA_OFFSET + 2]
        .copy_from_slice(&num_data_shreds.to_le_bytes());
    shard[CODING_NUM_CODING_OFFSET..CODING_NUM_CODING_OFFSET + 2]
        .copy_from_slice(&num_coding_shreds.to_le_bytes());
    shard[CODING_POSITION_OFFSET..CODING_POSITION_OFFSET + 2]
        .copy_from_slice(&position.to_le_bytes());
    if let Some(root) = chained_merkle_root {
        let offset = LEGACY_CODING_HEADERS_SIZE + merkle_code_capacity(variant)?;
        shard[offset..offset + MERKLE_ROOT_SIZE].copy_from_slice(&root);
    }
    Ok(shard)
}

fn merkle_node(payload: &[u8], variant: RecoveryVariant) -> Result<Hash, String> {
    let proof_offset = merkle_proof_offset(variant)?;
    let node = payload
        .get(SIGNATURE_SIZE..proof_offset)
        .ok_or_else(|| "invalid merkle node slice".to_string())?;
    Ok(hashv(&[MERKLE_HASH_PREFIX_LEAF, node]))
}

fn join_nodes(node: &Hash, other: &Hash) -> Hash {
    hashv(&[
        MERKLE_HASH_PREFIX_NODE,
        &node.as_ref()[..MERKLE_PROOF_ENTRY_SIZE],
        &other.as_ref()[..MERKLE_PROOF_ENTRY_SIZE],
    ])
}

fn make_merkle_tree(mut nodes: Vec<Hash>) -> Vec<Hash> {
    let mut size = nodes.len();
    while size > 1 {
        let offset = nodes.len() - size;
        for index in (offset..offset + size).step_by(2) {
            let node = nodes[index];
            let other = nodes[(index + 1).min(offset + size - 1)];
            nodes.push(join_nodes(&node, &other));
        }
        size = nodes.len() - offset - size;
    }
    nodes
}

fn make_merkle_proof(index: usize, size: usize, tree: &[Hash]) -> Option<Vec<[u8; 20]>> {
    if index >= size {
        return None;
    }
    let mut index = index;
    let mut size = size;
    let mut offset = 0usize;
    let mut proof = Vec::new();
    while size > 1 {
        let node = tree.get(offset + (index ^ 1).min(size - 1))?;
        proof.push(node.as_ref()[..MERKLE_PROOF_ENTRY_SIZE].try_into().ok()?);
        offset += size;
        size = (size + 1) >> 1;
        index >>= 1;
    }
    (offset + 1 == tree.len()).then_some(proof)
}

fn set_merkle_proof(
    payload: &mut [u8],
    variant: RecoveryVariant,
    proof: &[[u8; 20]],
) -> Result<(), String> {
    let proof_offset = merkle_proof_offset(variant)?;
    let expected_len = usize::from(variant.proof_size());
    if proof.len() != expected_len {
        return Err(format!(
            "invalid proof length {}, expected {}",
            proof.len(),
            expected_len
        ));
    }
    let proof_bytes = payload
        .get_mut(proof_offset..proof_offset + proof.len() * MERKLE_PROOF_ENTRY_SIZE)
        .ok_or_else(|| "invalid proof write range".to_string())?;
    for (chunk, entry) in proof_bytes.chunks_mut(MERKLE_PROOF_ENTRY_SIZE).zip(proof.iter()) {
        chunk.copy_from_slice(entry);
    }
    Ok(())
}
