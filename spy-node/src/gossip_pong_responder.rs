//! gossip_pong_responder.rs — Responde PINGs de gossip en el puerto 8001.
//!
//! Los validators de Solana envían gossip PING (Protocol::PingMessage, variante 4)
//! a la dirección gossip anunciada en ContactInfo para verificar liveness.
//! Sin una respuesta PONG, los validators eliminan nuestro ContactInfo de su CRDS
//! y dejan de enviarnos shreds Turbine.
//!
//! Formato wire (bincode):
//!   PingMessage = [u32 LE = 4] [Pubkey 32B] [token [u8;32]] [Signature 64B]  → 132 bytes
//!   PongMessage = [u32 LE = 5] [Pubkey 32B] [hash Hash 32B] [Signature 64B]  → 132 bytes
//!
//! pong.hash = sha256(b"SOLANA_PING_PONG" || token)
//! pong.signature = keypair.sign(pong.hash)

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::thread;

use solana_sdk::{
    hash::hashv,
    signature::{Keypair, Signer},
};
use tracing::{debug, info, warn};

const PING_PONG_HASH_PREFIX: &[u8] = b"SOLANA_PING_PONG";

const VARIANT_PING: u32 = 4;
const VARIANT_PONG: u32 = 5;

/// Tamaño exacto de un PingMessage wire (variante 4).
/// [u32=4][Pubkey 32B][token [u8;32]][Signature 64B] = 132 bytes
const PING_WIRE_SIZE: usize = 4 + 32 + 32 + 64;

pub fn spawn_gossip_pong_responder(bind_addr: SocketAddr, identity: Arc<Keypair>) {
    thread::Builder::new()
        .name("gossip-pong".into())
        .spawn(move || run_pong_loop(bind_addr, &identity))
        .expect("spawn gossip-pong thread");
}

fn run_pong_loop(bind_addr: SocketAddr, identity: &Keypair) {
    let socket = match UdpSocket::bind(bind_addr) {
        Ok(s) => s,
        Err(e) => {
            warn!(%bind_addr, error = %e, "gossip-pong: failed to bind");
            return;
        }
    };

    info!(%bind_addr, pubkey = %identity.pubkey(), "gossip-pong: listening for PING messages");

    let mut buf = [0u8; 2048];
    let mut pongs_sent = 0u64;

    loop {
        let (len, from) = match socket.recv_from(&mut buf) {
            Ok(x) => x,
            Err(e) => {
                warn!(error = %e, "gossip-pong: recv_from error");
                continue;
            }
        };

        if len < 4 {
            continue;
        }

        // Leer variante del protocolo gossip
        let variant = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if variant != VARIANT_PING {
            continue;
        }

        if len < PING_WIRE_SIZE {
            debug!(%from, len, "gossip-pong: undersized PING");
            continue;
        }

        // Extraer token de bytes [36..68]
        // Layout: [4B variant][32B pubkey_from][32B token][64B signature]
        let token: [u8; 32] = match buf[36..68].try_into() {
            Ok(t) => t,
            Err(_) => continue,
        };

        // pong_hash = sha256("SOLANA_PING_PONG" || token)
        let pong_hash = hashv(&[PING_PONG_HASH_PREFIX, &token]);

        // Firmar el hash con nuestra identidad
        let signature = identity.sign_message(pong_hash.as_ref());

        // Construir PongMessage wire: [u32 LE=5][Pubkey 32B][Hash 32B][Sig 64B]
        let mut pong = Vec::with_capacity(4 + 32 + 32 + 64);
        pong.extend_from_slice(&VARIANT_PONG.to_le_bytes());
        pong.extend_from_slice(identity.pubkey().as_ref());
        pong.extend_from_slice(pong_hash.as_ref());
        pong.extend_from_slice(signature.as_ref());

        if let Err(e) = socket.send_to(&pong, from) {
            debug!(%from, error = %e, "gossip-pong: send PONG failed");
        } else {
            pongs_sent += 1;
            if pongs_sent % 100 == 1 {
                info!(%from, pongs_sent, "gossip-pong: PING/PONG exchange OK");
            } else {
                debug!(%from, "gossip-pong: PONG sent");
            }
        }
    }
}
