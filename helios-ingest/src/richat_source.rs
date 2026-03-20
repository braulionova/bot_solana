//! Richat gRPC source — connects to local Richat server (self-hosted Geyser).
//!
//! Uses the same Yellowstone gRPC protocol but connects to local richat-server.
//! Lower latency than SaaS since it's on the same machine.

use crate::event_bus::EventBus;
use crate::grpc_source::{run_grpc_source, GrpcSourceConfig};
use crate::types::Source;
use std::sync::Arc;

pub struct RichatSourceConfig {
    pub endpoint: String,
    pub x_token: Option<String>,
}

/// Run the Richat source. Internally reuses the gRPC source logic
/// since Richat is 100% compatible with Yellowstone gRPC protocol.
pub async fn run_richat_source(
    config: RichatSourceConfig,
    bus: Arc<EventBus>,
) {
    let grpc_config = GrpcSourceConfig {
        endpoint: config.endpoint,
        x_token: config.x_token,
        label: "richat-local".to_string(),
        source_type: Source::Richat,
    };

    run_grpc_source(grpc_config, bus).await;
}
