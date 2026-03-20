// sig_verifier.rs – Ed25519 signature verification for incoming shreds.
//
// Every shred's first 64 bytes are an Ed25519 signature produced by the
// current slot leader.  We verify against the leader pubkey obtained from
// the gossip client / leader schedule.

use anyhow::{bail, Result};
use solana_sdk::pubkey::Pubkey;
use tracing::{trace, warn};

use crate::shred_parser::{ParsedShred, SHRED_VARIANT_OFFSET, SIGNATURE_LEN};

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// Verify the Ed25519 signature of a parsed shred.
///
/// The signed message is everything from byte `SIGNATURE_LEN` to the end of
/// the packet (i.e. the shred body, starting with the variant byte).
pub fn verify_shred(shred: &ParsedShred, leader_pubkey: &Pubkey) -> Result<()> {
    if shred.raw.len() < SIGNATURE_LEN + 1 {
        bail!("shred too short to verify");
    }

    let sig_bytes: &[u8; 64] = &shred.signature;
    let message = &shred.raw[SHRED_VARIANT_OFFSET..];
    let pubkey_bytes = leader_pubkey.to_bytes();

    verify_ed25519(sig_bytes, message, &pubkey_bytes)?;

    trace!(slot = shred.slot, index = shred.index, "shred sig OK");
    Ok(())
}

/// Pure Ed25519 verification using the Solana SDK's bundled `ed25519-dalek`.
///
/// We use the `solana_sdk::signature` helpers to avoid pulling in dalek
/// directly while still being correct.
fn verify_ed25519(sig_bytes: &[u8; 64], message: &[u8], pubkey_bytes: &[u8; 32]) -> Result<()> {
    use solana_sdk::pubkey::Pubkey;
    use solana_sdk::signature::Signature;

    let sig = Signature::from(*sig_bytes);
    let pk = Pubkey::from(*pubkey_bytes);

    // solana_sdk Signature::verify uses ed25519-dalek internally.
    if !sig.verify(pk.as_ref(), message) {
        warn!("shred Ed25519 signature verification FAILED");
        bail!("Ed25519 signature invalid");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Batch verifier (parallel, using rayon-style manual threading)
// ---------------------------------------------------------------------------

use crate::shred_parser::ParsedShred as PS;
use crossbeam_channel::{Receiver, Sender};
use std::sync::Arc;

pub struct SigVerifier {
    leader_pubkey: Arc<parking_lot::RwLock<Pubkey>>,
    enabled: bool,
}

impl SigVerifier {
    pub fn new(initial_leader: Pubkey, enabled: bool) -> Self {
        Self {
            leader_pubkey: Arc::new(parking_lot::RwLock::new(initial_leader)),
            enabled,
        }
    }

    /// Update the current slot leader (called when a new leader schedule epoch begins).
    pub fn update_leader(&self, new_leader: Pubkey) {
        *self.leader_pubkey.write() = new_leader;
    }

    /// Verify a single shred.  Returns `Ok(())` on success or when disabled.
    pub fn verify(&self, shred: &ParsedShred) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let leader = *self.leader_pubkey.read();
        verify_shred(shred, &leader)
    }

    /// Run a batch verifier thread: reads from `input`, verifies, writes
    /// passing shreds to `output`.
    pub fn run_batch(self: Arc<Self>, input: Receiver<ParsedShred>, output: Sender<ParsedShred>) {
        for shred in input {
            match self.verify(&shred) {
                Ok(()) => {
                    if output.send(shred).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    warn!(error = %e, slot = shred.slot, "dropping shred: sig verification failed");
                }
            }
        }
    }
}
