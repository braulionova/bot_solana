// tx_decoder.rs – Deserialise Solana entries from deshredded data blocks.

use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use dashmap::DashMap;
use solana_entry::entry::Entry;
use solana_sdk::{message::v0::LoadedAddresses, pubkey::Pubkey, transaction::VersionedTransaction};
use tracing::{debug, warn};

#[derive(Debug, Clone)]
pub struct AltEntry {
    pub addresses: Vec<Pubkey>,
}

pub type AltCache = Arc<DashMap<Pubkey, AltEntry>>;

pub fn new_alt_cache() -> AltCache {
    Arc::new(DashMap::new())
}

pub fn resolve_alt(
    cache: &AltCache,
    table_key: &Pubkey,
    writable_indexes: &[u8],
    readonly_indexes: &[u8],
) -> Result<LoadedAddresses> {
    let entry = cache
        .get(table_key)
        .ok_or_else(|| anyhow::anyhow!("ALT not found: {}", table_key))?;

    let writable: Result<Vec<Pubkey>> = writable_indexes
        .iter()
        .map(|&idx| {
            entry
                .addresses
                .get(idx as usize)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("ALT index {} out of range", idx))
        })
        .collect();

    let readonly: Result<Vec<Pubkey>> = readonly_indexes
        .iter()
        .map(|&idx| {
            entry
                .addresses
                .get(idx as usize)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("ALT index {} out of range", idx))
        })
        .collect();

    Ok(LoadedAddresses {
        writable: writable?,
        readonly: readonly?,
    })
}

pub fn decode_entries(raw: &[u8]) -> Result<Vec<Entry>> {
    Ok(bincode::deserialize::<Vec<Entry>>(raw)?)
}

pub struct TxDecoder {
    pub alt_cache: AltCache,
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_ledger::shred::{ProcessShredsStats, ReedSolomonCache, Shred, Shredder};
    use solana_sdk::{
        hash::Hash,
        signature::{Keypair, Signer},
        system_instruction,
        transaction::Transaction,
    };

    fn make_entry_block(
        slot: u64,
        parent_slot: u64,
        next_shred_index: u32,
        is_last_in_slot: bool,
        lamports: u64,
    ) -> Vec<Shred> {
        let payer = Keypair::new();
        let recipient = Keypair::new();
        let tx = Transaction::new_signed_with_payer(
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient.pubkey(),
                lamports,
            )],
            Some(&payer.pubkey()),
            &[&payer],
            Hash::new_unique(),
        );
        let entry = Entry::new(&Hash::new_unique(), 1, vec![tx]);
        let shredder = Shredder::new(slot, parent_slot, 0, 1).unwrap();
        let mut stats = ProcessShredsStats::default();
        let (data, _coding) = shredder.entries_to_shreds(
            &Keypair::new(),
            &[entry],
            is_last_in_slot,
            None,
            next_shred_index,
            next_shred_index + 64,
            false,
            &ReedSolomonCache::default(),
            &mut stats,
        );
        data
    }

    #[test]
    fn decode_entries_matches_solana_ledger_bincode_format() {
        let shreds = make_entry_block(10, 9, 0, false, 1);
        let payload = Shredder::deshred(&shreds).unwrap();
        let entries = decode_entries(&payload).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].transactions.len(), 1);
    }
}

impl TxDecoder {
    pub fn new(alt_cache: AltCache) -> Self {
        Self { alt_cache }
    }

    pub fn decode_batch(&self, slot: u64, payloads: &[Bytes]) -> Vec<VersionedTransaction> {
        let (txs, _) = self.decode_batch_with_entries(slot, payloads);
        txs
    }

    /// Decode transactions AND return raw entries (for blockhash derivation).
    /// Returns (transactions, entries) where entries preserve the PoH hashes.
    pub fn decode_batch_with_entries(
        &self,
        slot: u64,
        payloads: &[Bytes],
    ) -> (Vec<VersionedTransaction>, Vec<Entry>) {
        let mut txs = Vec::new();
        let mut all_entries = Vec::new();

        for payload in payloads {
            match decode_entries(payload) {
                Ok(entries) => {
                    for entry in entries {
                        for tx in &entry.transactions {
                            self.try_resolve_alt(tx);
                        }
                        let decoded_txs = entry.transactions.clone();
                        all_entries.push(entry);
                        txs.extend(decoded_txs);
                    }
                }
                Err(e) => {
                    warn!(slot, error = %e, "entry decode failed");
                }
            }
        }

        debug!(slot, txs = txs.len(), entries = all_entries.len(), "decoded transactions+entries");
        (txs, all_entries)
    }

    fn try_resolve_alt(&self, tx: &VersionedTransaction) -> bool {
        use solana_sdk::message::VersionedMessage;
        match &tx.message {
            VersionedMessage::V0(msg) => {
                for lookup in &msg.address_table_lookups {
                    if let Err(_) = resolve_alt(
                        &self.alt_cache,
                        &lookup.account_key,
                        &lookup.writable_indexes,
                        &lookup.readonly_indexes,
                    ) {
                        return false;
                    }
                }
                true
            }
            VersionedMessage::Legacy(_) => true,
        }
    }
}
