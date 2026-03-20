// config.rs – Bot configuration (environment variables).

use clap::Parser;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Config {
    #[arg(long, env = "SPY_TVU_PORT", default_value = "8002")]
    pub tvu_port: u16,

    #[arg(long, env = "SPY_JITO_PORT", default_value = "20000")]
    pub jito_shred_port: u16,

    #[arg(long, env = "SPY_UDP_THREADS", default_value = "4")]
    pub udp_threads: usize,

    #[arg(
        long,
        env = "GOSSIP_ENTRYPOINT",
        default_value = "entrypoint.mainnet-beta.solana.com:8001"
    )]
    pub gossip_entrypoint: String,

    #[arg(long, env = "GOSSIP_HOST")]
    pub gossip_host: Option<String>,

    #[arg(long, env = "GOSSIP_PORT", default_value = "0")]
    pub gossip_port: u16,

    #[arg(long, env = "GOSSIP_SHRED_VERSION")]
    pub gossip_shred_version: Option<u16>,

    #[arg(
        long,
        env = "IDENTITY_KEYPAIR",
        default_value = "/etc/helios/identity.json"
    )]
    pub identity_keypair: String,

    #[arg(long, env = "DEDUP_CACHE_SIZE", default_value = "10000")]
    pub dedup_cache_size: usize,

    #[arg(long, env = "CHANNEL_CAPACITY", default_value = "4096")]
    pub channel_capacity: usize,

    #[arg(long, env = "SIG_VERIFY", default_value = "true")]
    pub sig_verify: bool,

    #[arg(long, env = "BIND_ADDR", default_value = "0.0.0.0")]
    pub bind_addr: String,

    #[arg(
        long,
        env = "RPC_URL",
        default_value = "https://api.mainnet-beta.solana.com"
    )]
    pub rpc_url: String,

    // Executor settings
    #[arg(
        long,
        env = "PAYER_KEYPAIR",
        default_value = "/root/solana-bot/wallet.json"
    )]
    pub payer_keypair: String,

    #[arg(long, env = "MIN_NET_PROFIT", default_value = "50000")]
    pub min_net_profit: i64,

    #[arg(long, env = "DRY_RUN", default_value = "false")]
    pub dry_run: bool,

    #[arg(long, env = "JITO_ENDPOINT")]
    pub jito_endpoint: String,

    #[arg(long, env = "JITO_TIP_LAMPORTS", default_value = "10000")]
    pub jito_tip_lamports: u64,

    #[arg(long, env = "METRICS_PORT", default_value = "9090")]
    pub metrics_port: u16,

    #[arg(long, env = "STATS_INTERVAL_SECS", default_value = "30")]
    pub stats_interval_secs: u64,

    #[arg(long, env = "SQLITE_ROUTE_FEED", default_value = "true")]
    pub sqlite_route_feed: bool,

    #[arg(long, env = "JITO_SEARCHER_ENABLED", default_value = "true")]
    pub jito_searcher_enabled: bool,

    #[arg(long, env = "WS_DEX_FEED_ENABLED", default_value = "true")]
    pub ws_dex_feed_enabled: bool,

    #[arg(long, env = "GEYSER_FEED_ENABLED", default_value = "true")]
    pub geyser_feed_enabled: bool,

    #[arg(
        long,
        env = "SQLITE_ROUTE_DB_PATH",
        default_value = "/root/solana-bot/solana_bot.db"
    )]
    pub sqlite_route_db_path: String,

    #[arg(long, env = "SQLITE_ROUTE_POLL_SECS", default_value = "5")]
    pub sqlite_route_poll_secs: u64,

    #[arg(long, env = "SQLITE_ROUTE_TOP_N", default_value = "10")]
    pub sqlite_route_top_n: usize,

    #[arg(long, env = "SQLITE_ROUTE_COOLDOWN_SECS", default_value = "30")]
    pub sqlite_route_cooldown_secs: u64,

    #[arg(
        long,
        env = "JITO_BLOCK_ENGINE_URL",
        default_value = "https://frankfurt.mainnet.block-engine.jito.wtf"
    )]
    pub jito_block_engine_url: String,

    /// Enable Jito ShredStream gRPC registration (requires whitelisted keypair).
    #[arg(long, env = "JITO_SHREDSTREAM_ENABLED", default_value = "false")]
    pub jito_shredstream_enabled: bool,

    /// Public IP of this server advertised to Jito for UDP shred delivery.
    /// Defaults to gossip_host if set.
    #[arg(long, env = "JITO_SHREDSTREAM_DEST_IP")]
    pub jito_shredstream_dest_ip: Option<String>,

    /// Comma-separated list of Jito regions to subscribe to.
    #[arg(long, env = "JITO_SHREDSTREAM_REGIONS", default_value = "frankfurt")]
    pub jito_shredstream_regions: String,

    /// Keypair used for Jito ShredStream auth (must be whitelisted).
    /// Defaults to identity_keypair if not set.
    #[arg(long, env = "JITO_SHREDSTREAM_AUTH_KEYPAIR")]
    pub jito_shredstream_auth_keypair: Option<String>,
}

impl Config {
    pub fn bind_ip_addr(&self) -> IpAddr {
        self.bind_addr.parse().unwrap()
    }

    pub fn advertised_tvu_addr(&self, advertised_ip: IpAddr) -> SocketAddr {
        SocketAddr::new(advertised_ip, self.tvu_port)
    }

    pub fn tvu_addr(&self) -> SocketAddr {
        format!("{}:{}", self.bind_addr, self.tvu_port)
            .to_socket_addrs()
            .unwrap()
            .next()
            .unwrap()
    }

    pub fn jito_addr(&self) -> SocketAddr {
        format!("{}:{}", self.bind_addr, self.jito_shred_port)
            .to_socket_addrs()
            .unwrap()
            .next()
            .unwrap()
    }

    pub fn jito_block_engine_url(&self) -> String {
        self.jito_block_engine_url.clone()
    }
}
