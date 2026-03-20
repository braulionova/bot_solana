// shred_parser.rs - Parse raw UDP shred bytes into typed Shred structs.
//
// Reference layout (Solana shred wire format common header as implemented by
// the pinned `solana-ledger` crate in this workspace):
//   [0..64]   signature (Ed25519, 64 bytes)
//   [64]      shred_variant byte
//   [65..73]  slot (u64 LE)
//   [73..77]  index (u32 LE)
//   [77..79]  version (u16 LE)
//   [79..83]  fec_set_index (u32 LE)
//
// NOTE: `slot/index/fec_set_index/version` are sourced from
// `solana_ledger::shred::Shred`; only the data payload slice still relies on
// the wire offsets below.

use anyhow::{anyhow, bail, Result};
use bytes::Bytes;
use solana_ledger::shred::Shred as SolanaShred;
use tracing::trace;

// ---------------------------------------------------------------------------
// Wire-format constants
// ---------------------------------------------------------------------------

pub const SIGNATURE_OFFSET: usize = 0;
pub const SIGNATURE_LEN: usize = 64;
pub const SHRED_VARIANT_OFFSET: usize = 64;
pub const SHRED_VERSION_OFFSET: usize = 77; // u16 LE
                                            // Data-shred-specific fields for `solana-ledger 1.18.x`.
pub const PARENT_OFFSET: usize = 83; // u16 LE (data shreds only)
pub const FLAGS_OFFSET: usize = 85; // u8     (data shreds only)
pub const DATA_SIZE_OFFSET: usize = 86; // u16 LE (data shreds only)
pub const DATA_PAYLOAD_OFFSET: usize = 88; // start of actual entry data
pub const CODING_NUM_DATA_SHREDS_OFFSET: usize = 83; // u16 LE (code shreds only)
pub const CODING_NUM_CODING_SHREDS_OFFSET: usize = 85; // u16 LE (code shreds only)
pub const CODING_POSITION_OFFSET: usize = 87; // u16 LE (code shreds only)
pub const CODING_HEADER_END_OFFSET: usize = 89;

pub const MIN_SHRED_SIZE: usize = DATA_PAYLOAD_OFFSET;

// ---------------------------------------------------------------------------
// ShredType / ShredVariant
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShredType {
    Data,
    Code,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShredVariant {
    /// Original shred format without Merkle proof.
    LegacyData,
    LegacyCode,
    /// New Merkle-proof shred format.
    MerkleData {
        proof_size: u8,
    },
    MerkleCode {
        proof_size: u8,
    },
}

impl ShredVariant {
    /// Decode from the single variant byte at offset 64.
    pub fn from_byte(byte: u8) -> Result<Self> {
        match byte {
            0x5a => return Ok(ShredVariant::LegacyCode),
            0xa5 => return Ok(ShredVariant::LegacyData),
            _ => {}
        }
        let proof_size = byte & 0x0F;
        if byte >> 7 == 1 {
            Ok(ShredVariant::MerkleData { proof_size })
        } else if byte >> 7 == 0 {
            Ok(ShredVariant::MerkleCode { proof_size })
        } else {
            bail!("unknown shred variant byte: 0x{:02x}", byte)
        }
    }

    pub fn shred_type(&self) -> ShredType {
        match self {
            ShredVariant::LegacyData | ShredVariant::MerkleData { .. } => ShredType::Data,
            ShredVariant::LegacyCode | ShredVariant::MerkleCode { .. } => ShredType::Code,
        }
    }
}

// ---------------------------------------------------------------------------
// Parsed shred
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ParsedShred {
    /// Raw packet bytes (Zero-copy via Bytes).
    pub raw: Bytes,
    /// Parsed Solana shred reused downstream to avoid reparsing.
    pub shred: SolanaShred,
    pub slot: u64,
    /// Index within the FEC set.
    pub index: u32,
    pub fec_set_index: u32,
    pub shred_type: ShredType,
    pub variant: ShredVariant,
    pub shred_version: u16,
    pub data_complete: bool,
    pub last_in_slot: bool,
    pub coding_meta: Option<CodingMeta>,
    /// Ed25519 signature bytes (first 64 bytes of the packet).
    pub signature: [u8; 64],
    /// For data shreds: the entry payload bytes (slice of raw).
    pub data: Option<Bytes>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodingMeta {
    pub num_data_shreds: u16,
    pub num_coding_shreds: u16,
    pub position: u16,
}

impl CodingMeta {
    pub fn first_coding_index(self, index: u32) -> Option<u32> {
        index.checked_sub(u32::from(self.position))
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse a raw UDP payload into a `ParsedShred`.
pub fn parse_shred(raw: Bytes) -> Result<ParsedShred> {
    if raw.len() < MIN_SHRED_SIZE {
        bail!("shred too small: {} < {} bytes", raw.len(), MIN_SHRED_SIZE);
    }

    let shred = SolanaShred::new_from_serialized_shred(raw.to_vec()).map_err(|err| {
        anyhow!(
            "failed to deserialize shred ({} bytes): {:?}",
            raw.len(),
            err
        )
    })?;

    let mut signature = [0u8; 64];
    signature.copy_from_slice(&raw[SIGNATURE_OFFSET..SIGNATURE_OFFSET + SIGNATURE_LEN]);

    let variant_byte = raw[SHRED_VARIANT_OFFSET];
    let variant = ShredVariant::from_byte(variant_byte)?;
    let shred_type = if shred.is_data() {
        ShredType::Data
    } else {
        ShredType::Code
    };

    let shred_version = shred.version();
    let fec_set_index = shred.fec_set_index();
    let index = shred.index();
    let slot = shred.slot();
    let data_complete = shred.data_complete();
    let last_in_slot = shred.last_in_slot();
    let coding_meta = if shred_type == ShredType::Code && raw.len() >= CODING_HEADER_END_OFFSET {
        Some(CodingMeta {
            num_data_shreds: u16::from_le_bytes([
                raw[CODING_NUM_DATA_SHREDS_OFFSET],
                raw[CODING_NUM_DATA_SHREDS_OFFSET + 1],
            ]),
            num_coding_shreds: u16::from_le_bytes([
                raw[CODING_NUM_CODING_SHREDS_OFFSET],
                raw[CODING_NUM_CODING_SHREDS_OFFSET + 1],
            ]),
            position: u16::from_le_bytes([
                raw[CODING_POSITION_OFFSET],
                raw[CODING_POSITION_OFFSET + 1],
            ]),
        })
    } else {
        None
    };

    let data = if shred_type == ShredType::Data && raw.len() > DATA_PAYLOAD_OFFSET {
        let payload_end = if raw.len() >= DATA_SIZE_OFFSET + 2 {
            let sz =
                u16::from_le_bytes([raw[DATA_SIZE_OFFSET], raw[DATA_SIZE_OFFSET + 1]]) as usize;
            sz.min(raw.len())
        } else {
            raw.len()
        };
        Some(raw.slice(DATA_PAYLOAD_OFFSET..payload_end))
    } else {
        None
    };

    trace!(slot, index, fec_set_index, shred_type = ?shred_type, "parsed shred");

    Ok(ParsedShred {
        raw,
        shred,
        slot,
        index,
        fec_set_index,
        shred_type,
        variant,
        shred_version,
        data_complete,
        last_in_slot,
        coding_meta,
        signature,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_ledger::shred::{Shred, ShredFlags};

    #[test]
    fn parse_shred_reads_metadata_from_solana_shred() {
        let payload = b"helios";
        let raw = Shred::new_from_data(
            42,
            7,
            1,
            payload,
            ShredFlags::DATA_COMPLETE_SHRED,
            0,
            1234,
            5,
        )
        .into_payload();

        let parsed = parse_shred(Bytes::from(raw)).unwrap();

        assert_eq!(parsed.slot, 42);
        assert_eq!(parsed.index, 7);
        assert_eq!(parsed.fec_set_index, 5);
        assert_eq!(parsed.shred_version, 1234);
        assert_eq!(parsed.shred_type, ShredType::Data);
        assert_eq!(parsed.variant, ShredVariant::LegacyData);
        assert!(parsed.data_complete);
        assert!(!parsed.last_in_slot);
        assert_eq!(parsed.data.unwrap().as_ref(), payload);
        assert!(parsed.shred.is_data());
    }
}
