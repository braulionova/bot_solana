//! oracle.rs – Pyth price oracle fetcher for E5 strategy.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serde_json::Value;
use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info, warn};

const HERMES_PRIMARY_URL: &str = "https://hermes.pyth.network/v2/updates/price/latest";
const HERMES_FALLBACK_URL: &str = "https://hermes.pyth.network/api/latest_price_feeds";

pub mod feed_ids {
    pub const SOL_USD: &str = "0xef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";
    pub const BTC_USD: &str = "0xe62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43";
    pub const ETH_USD: &str = "0xff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace";
    pub const USDC_USD: &str = "0xeaa020c61cc479712813461ce153894a96a6c00b21ed0cfc2798d1f9a9e9c94a";
    pub const USDT_USD: &str = "0x2b89b9dc8fdf9f34709a5b106b472f0f39bb6ca9ce04b0fd7f2e971688e2e53b";
    pub const BONK_USD: &str = "0x72b021217ca3fe68922a19aaf990109cb9d84e9ad004b4d2025ad6f529314419";
    pub const MSOL_USD: &str = "0xc2289a6a43d2ce91c6f55caec370f4acc38a2ed477f58813334c6d03749ff2a4";
    pub const JITOSOL: &str = "0x67be9f519b95cf24338801051f9a808eff0a578024dc6081fd0f87b42f1a1e2c";
}

pub mod mints {
    pub const SOL: &str = "So11111111111111111111111111111111111111112";
    pub const BTC: &str = "9n4nbM75f5Ui33ZbPYXn59EwSgE8CGsHtAeTH5YFeJ9E";
    pub const ETH: &str = "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs";
    pub const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    pub const USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
    pub const BONK: &str = "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263";
    pub const MSOL: &str = "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So";
    pub const JITOSOL: &str = "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn";
}

#[derive(Debug, Clone)]
pub struct OraclePrice {
    pub price_usd: f64,
    pub conf_usd: f64,
    pub fetched_at: Instant,
}

pub struct OracleCache {
    prices: Arc<RwLock<HashMap<Pubkey, OraclePrice>>>,
    client: reqwest::blocking::Client,
}

impl OracleCache {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            prices: Arc::new(RwLock::new(HashMap::new())),
            client: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        })
    }

    pub fn get(&self, mint: &Pubkey) -> Option<f64> {
        self.prices.read().get(mint).map(|p| p.price_usd)
    }

    pub fn snapshot(&self) -> HashMap<Pubkey, f64> {
        self.prices
            .read()
            .iter()
            .map(|(k, v)| (*k, v.price_usd))
            .collect()
    }

    fn update(&self, mint: Pubkey, price_usd: f64, conf_usd: f64) {
        let entry = OraclePrice {
            price_usd,
            conf_usd,
            fetched_at: Instant::now(),
        };
        self.prices.write().insert(mint, entry);
    }
}

static FEED_MAP: &[(&str, &str, &str)] = &[
    (feed_ids::SOL_USD, mints::SOL, "SOL"),
    (feed_ids::BTC_USD, mints::BTC, "BTC"),
    (feed_ids::ETH_USD, mints::ETH, "ETH"),
    (feed_ids::USDC_USD, mints::USDC, "USDC"),
    (feed_ids::USDT_USD, mints::USDT, "USDT"),
    (feed_ids::BONK_USD, mints::BONK, "BONK"),
    (feed_ids::MSOL_USD, mints::MSOL, "mSOL"),
    // jitoSOL feed removed — deprecated on Hermes v2 (returns 404 for entire batch)
];

fn fetch_prices(cache: &Arc<OracleCache>) -> anyhow::Result<()> {
    let feed_ids: Vec<&str> = FEED_MAP.iter().map(|(id, _, _)| *id).collect();
    let mut last_error = None;

    for url in [HERMES_PRIMARY_URL, HERMES_FALLBACK_URL] {
        match fetch_prices_from_url(cache, url, &feed_ids) {
            Ok(updated) => {
                debug!(url, updated, "[oracle] refreshed");
                return Ok(());
            }
            Err(err) => last_error = Some(err),
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no Hermes endpoint succeeded")))
}

fn fetch_prices_from_url(
    cache: &Arc<OracleCache>,
    url: &str,
    feed_ids: &[&str],
) -> anyhow::Result<usize> {
    // The v2 Hermes endpoint requires `parsed=true` to include human-readable
    // price data in the response alongside the binary payload.
    let mut params: Vec<(&str, &str)> = feed_ids.iter().map(|id| ("ids[]", *id)).collect();
    if url.contains("/v2/") {
        params.push(("parsed", "true"));
    }

    let resp = cache.client.get(url).query(&params).send()?;

    if !resp.status().is_success() {
        anyhow::bail!(
            "Pyth Hermes status: {} - URL: {}",
            resp.status(),
            resp.url()
        );
    }

    let v: Value = resp.json()?;
    let parsed = parse_price_items(&v)?;
    let mut updated = 0usize;

    for item in parsed {
        let feed_id = item["id"].as_str().unwrap_or("");
        let feed_id_hex = if feed_id.starts_with("0x") {
            feed_id.to_string()
        } else {
            format!("0x{}", feed_id)
        };

        let Some((_, mint_str, sym)) = FEED_MAP
            .iter()
            .find(|(fid, _, _)| *fid == feed_id_hex.as_str())
        else {
            continue;
        };

        let price_obj = &item["price"];
        let price_raw = price_obj["price"]
            .as_str()
            .and_then(|s: &str| s.parse::<i64>().ok())
            .unwrap_or(0);
        let conf_raw = price_obj["conf"]
            .as_str()
            .and_then(|s: &str| s.parse::<i64>().ok())
            .unwrap_or(0);
        let expo = price_obj["expo"].as_i64().unwrap_or(0);

        let scale = 10f64.powi(expo as i32);
        let price_usd = price_raw as f64 * scale;
        let conf_usd = conf_raw as f64 * scale;

        if price_usd <= 0.0 {
            continue;
        }

        if let Ok(mint) = mint_str.parse::<Pubkey>() {
            cache.update(mint, price_usd, conf_usd);
            debug!(sym, price_usd, conf_usd, "[oracle] updated");
            updated += 1;
        }
    }

    Ok(updated)
}

fn parse_price_items<'a>(payload: &'a Value) -> anyhow::Result<Vec<&'a Value>> {
    if let Some(parsed) = payload.get("parsed").and_then(Value::as_array) {
        return Ok(parsed.iter().collect());
    }

    if let Some(parsed) = payload.as_array() {
        return Ok(parsed.iter().collect());
    }

    anyhow::bail!("unexpected Hermes payload shape")
}

/// Perform a single blocking fetch. Useful at startup to ensure prices are
/// populated before the route engine runs for the first time.
pub fn refresh_blocking(cache: &Arc<OracleCache>) -> anyhow::Result<()> {
    fetch_prices(cache)
}

pub fn spawn_oracle_refresh(cache: Arc<OracleCache>, interval: Duration) {
    std::thread::Builder::new()
        .name("oracle-refresh".into())
        .spawn(move || {
            info!("[oracle] refresh loop started");
            loop {
                if let Err(e) = fetch_prices(&cache) {
                    warn!(error = %e, "[oracle] fetch failed");
                }
                std::thread::sleep(interval);
            }
        })
        .expect("spawn oracle-refresh thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_price_items_accepts_v2_payload() {
        let payload = serde_json::json!({
            "parsed": [
                { "id": "0xabc", "price": { "price": "1", "conf": "1", "expo": 0 } }
            ]
        });

        let items = parse_price_items(&payload).expect("payload should parse");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], "0xabc");
    }

    #[test]
    fn parse_price_items_accepts_legacy_array_payload() {
        let payload = serde_json::json!([
            { "id": "0xdef", "price": { "price": "2", "conf": "1", "expo": 0 } }
        ]);

        let items = parse_price_items(&payload).expect("payload should parse");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], "0xdef");
    }
}
