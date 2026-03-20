//! turbine_announcer.rs — Manual gossip PUSH for Turbine TVU participation.
//!
//! GossipService (Agave 1.18.26) tiene un SIGSEGV en el thread t_responder
//! (use-after-free en PinnedVec recycler). Solución: bypassear GossipService
//! por completo construyendo el mensaje `Protocol::PushMessage` a mano.
//!
//! `Protocol` es `pub(crate)` en solana-gossip, pero su formato bincode en
//! wire es estable:
//!
//!   PullRequest  = variant 0
//!   PullResponse = variant 1
//!   PushMessage  = variant 2   ← el que usamos
//!   PruneMessage = variant 3
//!   PingMessage  = variant 4
//!   PongMessage  = variant 5
//!
//! Formato bincode PushMessage:
//!   [u32 LE = 2] [Pubkey 32 bytes] [Vec<CrdsValue> = u64-len + items]
//!
//! Efecto: los validators guardan nuestro LegacyContactInfo (con TVU addr) en
//! su tabla CRDS → nos incluyen en el árbol Turbine → shreds UDP a nuestro
//! puerto TVU (8002).
//!
//! TTL de CRDS = CRDS_GOSSIP_PUSH_MSG_TIMEOUT_MS = 30s → refrescamos cada 20s.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use solana_gossip::{
    contact_info::ContactInfo,
    crds_value::{CrdsData, CrdsValue},
};
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};
use tracing::{info, warn};

/// CRDS entry TTL en Agave es 30s; refrescamos al 66% del TTL.
const PUSH_INTERVAL: Duration = Duration::from_secs(20);
/// Máximo de peers a los que empujamos en cada ciclo.
const MAX_PEERS: usize = 200;

/// Spawn del thread que mantiene nuestra presencia en el árbol Turbine.
///
/// `our_gossip_addr` — IP:port que anunciamos como gossip (debería ser nuestra
///                     IP pública + algún puerto libre; no necesita estar
///                     escuchando para recibir shreds, solo para que el CRDS
///                     entry sea válido).
/// `our_tvu_addr`   — IP:port donde ya escuchan los threads udp_ingest (8002).
///                     Aquí es donde Turbine enviará los shreds UDP.
pub fn spawn_turbine_announcer(
    identity: Arc<Keypair>,
    our_gossip_addr: SocketAddr,
    our_tvu_addr: SocketAddr,
    shred_version: u16,
    entrypoint: SocketAddr,
    rpc_url: String,
) {
    thread::Builder::new()
        .name("turbine-announcer".into())
        .spawn(move || {
            run_push_loop(
                &identity,
                our_gossip_addr,
                our_tvu_addr,
                shred_version,
                entrypoint,
                &rpc_url,
            );
        })
        .expect("spawn turbine-announcer");
}

// ---------------------------------------------------------------------------
// Construcción del ContactInfo
// ---------------------------------------------------------------------------

fn build_contact_info_crds(
    identity: &Keypair,
    gossip_addr: SocketAddr,
    tvu_addr: SocketAddr,
    shred_version: u16,
) -> CrdsValue {
    // ContactInfo::new NO hace bind — construye la estructura pura.
    let wallclock = solana_sdk::timing::timestamp();
    let mut ci = ContactInfo::new(identity.pubkey(), wallclock, shred_version);
    // Anunciar nuestra dirección gossip (los validators la guardan en CRDS).
    let _ = ci.set_gossip(gossip_addr);
    // El campo TVU es lo que Turbine usa para enviarnos shreds UDP.
    let _ = ci.set_tvu(tvu_addr);
    // Serializar y firmar el CRDS value con nuestro keypair.
    CrdsValue::new_signed(CrdsData::ContactInfo(ci), identity)
}

// ---------------------------------------------------------------------------
// Serialización manual de Protocol::PushMessage
// ---------------------------------------------------------------------------
//
// Protocol es pub(crate) en solana-gossip, pero CrdsValue es pub.
// bincode 1.x serializa enums como: u32 (LE) variant index + datos.
// PushMessage(Pubkey, Vec<CrdsValue>) → variant 2.
//
// Construimos los bytes equivalentes a:
//   bincode::serialize(&Protocol::PushMessage(from, values))
// sin necesitar acceso al tipo Protocol.

fn serialize_push_message(from: &Pubkey, values: &[CrdsValue]) -> Option<Vec<u8>> {
    // Serializar (Pubkey, &[CrdsValue]) — idéntico a los campos del variant.
    let payload = bincode::serialize(&(from, values)).ok()?;

    let mut msg = Vec::with_capacity(4 + payload.len());
    // Variant index 2 = PushMessage (u32 little-endian)
    msg.extend_from_slice(&2u32.to_le_bytes());
    msg.extend_from_slice(&payload);
    Some(msg)
}

// ---------------------------------------------------------------------------
// Loop principal
// ---------------------------------------------------------------------------

fn run_push_loop(
    identity: &Keypair,
    our_gossip_addr: SocketAddr,
    our_tvu_addr: SocketAddr,
    shred_version: u16,
    entrypoint: SocketAddr,
    rpc_url: &str,
) {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "turbine-announcer: bind UDP");
            return;
        }
    };

    let rpc = RpcClient::new(rpc_url.to_string());

    info!(
        gossip_addr = %our_gossip_addr,
        tvu_addr    = %our_tvu_addr,
        shred_version,
        "turbine-announcer: gossip PUSH loop started — building Turbine presence"
    );

    loop {
        let crds_val =
            build_contact_info_crds(identity, our_gossip_addr, our_tvu_addr, shred_version);

        let Some(msg) = serialize_push_message(&identity.pubkey(), &[crds_val]) else {
            warn!("turbine-announcer: serialize PushMessage failed");
            thread::sleep(PUSH_INTERVAL);
            continue;
        };

        // Siempre empujamos al entrypoint bootstrap.
        let _ = socket.send_to(&msg, entrypoint);
        let mut pushed = 1usize;

        // Obtener gossip addrs de los validators activos via RPC.
        match rpc.get_cluster_nodes() {
            Ok(nodes) => {
                for node in nodes.iter().take(MAX_PEERS) {
                    let Some(addr) = node.gossip else { continue };
                    if addr == our_gossip_addr {
                        continue;
                    }
                    let _ = socket.send_to(&msg, addr);
                    pushed += 1;
                }
            }
            Err(e) => warn!(error = %e, "turbine-announcer: get_cluster_nodes"),
        }

        info!(
            pushed,
            tvu = %our_tvu_addr,
            "turbine-announcer: ContactInfo PUSH cycle done"
        );

        thread::sleep(PUSH_INTERVAL);
    }
}
