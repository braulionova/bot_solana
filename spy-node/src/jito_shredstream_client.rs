//! jito_shredstream_client.rs
//!
//! Cliente nativo Rust para Jito ShredStream gRPC.
//! Flujo:
//!   1. Conectar al Block Engine via gRPC+TLS
//!   2. Autenticar con el keypair (challenge → sign → tokens)
//!   3. Registrar nuestra IP:puerto UDP con heartbeats periódicos
//!   4. El Block Engine empuja shreds directamente a nuestro puerto UDP
//!
//! El keypair DEBE estar en la whitelist de Jito ShredStream.
//! Sin whitelist: PermissionDenied en GenerateAuthChallenge.
//! Registro: https://www.jito.network/shredstream/

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use solana_sdk::signature::{Keypair, Signer};
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, ClientTlsConfig, Uri};
use tonic::Request;
use tracing::{error, info, warn};

// Tipos generados por tonic-build desde los .proto
pub mod proto {
    pub mod auth {
        tonic::include_proto!("auth");
    }
    pub mod shredstream {
        tonic::include_proto!("shredstream");
    }
}

use proto::auth::auth_service_client::AuthServiceClient;
use proto::auth::{GenerateAuthChallengeRequest, GenerateAuthTokensRequest, Role};
use proto::shredstream::shredstream_client::ShredstreamClient;
use proto::shredstream::{Heartbeat, SocketAddr as JitoSocketAddr};

/// Spawn del thread que mantiene la suscripción al ShredStream de Jito.
///
/// Una vez conectado, el Block Engine envía shreds directamente al
/// `dest_ip:dest_port` vía UDP. Las conexiones UDP ya abiertas en ese
/// puerto (ej. el ingest loop en puerto 20000) reciben los shreds sin
/// ningún código adicional.
pub fn spawn_jito_shredstream(
    keypair: Arc<Keypair>,
    block_engine_url: String,
    dest_ip: String,
    dest_port: u16,
    regions: Vec<String>,
    exit: Arc<AtomicBool>,
) {
    thread::Builder::new()
        .name("jito-shredstream".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt for jito-shredstream");

            rt.block_on(async {
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }
                    match run_client(
                        &keypair,
                        &block_engine_url,
                        &dest_ip,
                        dest_port,
                        &regions,
                        &exit,
                    )
                    .await
                    {
                        Ok(()) => {
                            warn!("jito shredstream session ended — reconnecting in 5s");
                        }
                        Err(e) => {
                            // Distinguir entre "no autorizado" y error transitorio.
                            let msg = format!("{e:#}");
                            if msg.contains("PermissionDenied") || msg.contains("not authorized") {
                                warn!(
                                    "Jito ShredStream auth denied — retrying in 5min. \
                                     Register keypair at jito.network/shredstream/"
                                );
                                tokio::time::sleep(Duration::from_secs(300)).await;
                            } else {
                                warn!(error = %e, "jito shredstream error — reconectando en 10s");
                                tokio::time::sleep(Duration::from_secs(10)).await;
                            }
                        }
                    }
                }
            });
        })
        .expect("spawn jito-shredstream");
}

/// Loop principal: conectar, autenticar (o skip auth), latidos.
async fn run_client(
    keypair: &Keypair,
    block_engine_url: &str,
    dest_ip: &str,
    dest_port: u16,
    regions: &[String],
    exit: &AtomicBool,
) -> Result<()> {
    // ── Conexión gRPC + TLS ─────────────────────────────────────────────────
    let uri: Uri = block_engine_url.parse().context("parse block_engine_url")?;

    let tls = ClientTlsConfig::new();
    let channel = Channel::builder(uri)
        .tls_config(tls)
        .context("tls_config")?
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(10))
        .connect()
        .await
        .context("connect to block engine")?;

    info!(
        block_engine_url,
        dest = %format!("{}:{}", dest_ip, dest_port),
        ?regions,
        "connected to Jito block engine"
    );

    // ── Intentar auth, pero si falla probar sin auth ────────────────────────
    let bearer = match authenticate(keypair, channel.clone()).await {
        Ok(access_token) => {
            info!(pubkey = %keypair.pubkey(), "authenticated with Jito block engine");
            Some(format!("Bearer {}", access_token))
        }
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("PermissionDenied") || msg.contains("not authorized") {
                warn!("Jito ShredStream auth rejected — trying without auth (NoAuth mode)");
                None
            } else {
                return Err(e.context("authenticate"));
            }
        }
    };

    // ── Loop de heartbeats ──────────────────────────────────────────────────
    let mut shredstream = ShredstreamClient::new(channel);

    let heartbeat = Heartbeat {
        socket: Some(JitoSocketAddr {
            ip: dest_ip.to_string(),
            port: dest_port as u32,
        }),
        regions: regions.to_vec(),
    };

    let mut ttl_ms = 1_000u32;

    loop {
        if exit.load(Ordering::Relaxed) {
            break;
        }

        let mut req = Request::new(heartbeat.clone());
        if let Some(ref bearer) = bearer {
            req.metadata_mut().insert(
                "authorization",
                MetadataValue::try_from(bearer).context("build auth header")?,
            );
        }

        match shredstream.send_heartbeat(req).await {
            Ok(resp) => {
                ttl_ms = resp.into_inner().ttl_ms.max(200);
                info!(ttl_ms, "Jito ShredStream heartbeat OK");
                // Re-heartbeat al 80% del TTL para evitar expiración.
                let sleep_ms = (ttl_ms as u64 * 80) / 100;
                tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
            }
            Err(status) => {
                if status.code() == tonic::Code::PermissionDenied
                    || status.code() == tonic::Code::Unauthenticated
                {
                    if bearer.is_some() {
                        anyhow::bail!(
                            "PermissionDenied: {} — keypair no autorizado",
                            status.message()
                        );
                    }
                    // NoAuth mode also rejected — ShredStream requires whitelist
                    anyhow::bail!(
                        "PermissionDenied (NoAuth): {} — ShredStream requires whitelisted pubkey. \
                         Register at https://www.jito.network/shredstream/",
                        status.message()
                    );
                }
                warn!(code = ?status.code(), msg = %status.message(), "heartbeat failed");
                tokio::time::sleep(Duration::from_secs(5)).await;
                return Err(anyhow::anyhow!("heartbeat error: {}", status));
            }
        }
    }

    Ok(())
}

/// Handshake de autenticación con el Block Engine.
/// Devuelve el Bearer access token.
async fn authenticate(keypair: &Keypair, channel: Channel) -> Result<String> {
    let mut auth = AuthServiceClient::new(channel);

    // 1. Solicitar challenge
    let pubkey_bytes = keypair.pubkey().to_bytes().to_vec();
    let challenge_resp = auth
        .generate_auth_challenge(GenerateAuthChallengeRequest {
            role: Role::ShredstreamSubscriber as i32,
            pubkey: pubkey_bytes.clone(),
        })
        .await
        .context("generate_auth_challenge")?
        .into_inner();

    // 2. Firmar "{pubkey_base58}-{challenge}" con ed25519
    let to_sign = format!("{}-{}", keypair.pubkey(), challenge_resp.challenge);
    let signature = keypair.sign_message(to_sign.as_bytes());

    // 3. Obtener tokens
    let tokens = auth
        .generate_auth_tokens(GenerateAuthTokensRequest {
            challenge: challenge_resp.challenge,
            client_pubkey: pubkey_bytes,
            signed_challenge: signature.as_ref().to_vec(),
        })
        .await
        .context("generate_auth_tokens")?
        .into_inner();

    let token_value = tokens
        .access_token
        .context("no access_token in response")?
        .value;

    Ok(token_value)
}
