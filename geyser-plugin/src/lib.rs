//! helios-geyser-plugin — Minimal Geyser plugin for real-time vault balance streaming.
//!
//! Loaded by agave-validator via --geyser-plugin-config. Monitors SPL Token vault
//! accounts and sends balance updates via UDP to the helios bot at ~0ms latency.
//!
//! Config JSON:
//! {
//!   "libpath": "/root/spy_node/target/release/libhelios_geyser_plugin.so",
//!   "udp_dest": "127.0.0.1:8855",
//!   "vaults_file": "/root/spy_node/geyser-plugin/vaults.json"
//! }
//!
//! Wire format (UDP packet, 41 bytes):
//!   [0..32]  vault pubkey
//!   [32..40] amount (u64 LE)
//!   [40]     is_vault_a (0 or 1)

use agave_geyser_plugin_interface::geyser_plugin_interface::{
    GeyserPlugin, GeyserPluginError, ReplicaAccountInfoVersions, Result as PluginResult,
};
use dashmap::DashSet;
use std::net::UdpSocket;
use std::sync::OnceLock;

/// SPL Token Account: amount at offset 64, 8 bytes LE.
const TOKEN_AMOUNT_OFFSET: usize = 64;
const MIN_TOKEN_ACCOUNT_LEN: usize = 165;

/// UDP packet size: 32 (pubkey) + 8 (amount) + 1 (side) = 41 bytes.
const PACKET_SIZE: usize = 41;

static UDP_SOCKET: OnceLock<UdpSocket> = OnceLock::new();

#[derive(Debug)]
pub struct HeliosGeyserPlugin {
    /// Set of vault pubkeys to monitor (32-byte keys).
    vaults: DashSet<[u8; 32]>,
    udp_dest: String,
}

impl Default for HeliosGeyserPlugin {
    fn default() -> Self {
        Self {
            vaults: DashSet::new(),
            udp_dest: "127.0.0.1:8855".to_string(),
        }
    }
}

impl GeyserPlugin for HeliosGeyserPlugin {
    fn name(&self) -> &'static str {
        "helios-geyser-plugin"
    }

    fn on_load(&mut self, config_file: &str, _is_reload: bool) -> PluginResult<()> {
        // Parse config JSON.
        let config_str = std::fs::read_to_string(config_file)
            .map_err(|e| GeyserPluginError::Custom(Box::new(e)))?;
        let config: serde_json::Value = serde_json::from_str(&config_str)
            .map_err(|e| GeyserPluginError::Custom(Box::new(e)))?;

        self.udp_dest = config["udp_dest"]
            .as_str()
            .unwrap_or("127.0.0.1:8855")
            .to_string();

        // Load vault pubkeys from file.
        let vaults_file = config["vaults_file"]
            .as_str()
            .unwrap_or("/root/spy_node/geyser-plugin/vaults.json");

        if let Ok(vaults_str) = std::fs::read_to_string(vaults_file) {
            if let Ok(vaults_json) = serde_json::from_str::<serde_json::Value>(&vaults_str) {
                if let Some(arr) = vaults_json.as_array() {
                    for v in arr {
                        if let Some(pubkey_str) = v["pubkey"].as_str() {
                            if let Ok(bytes) = bs58_decode(pubkey_str) {
                                if bytes.len() == 32 {
                                    let mut key = [0u8; 32];
                                    key.copy_from_slice(&bytes);
                                    self.vaults.insert(key);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Init UDP socket.
        let socket = UdpSocket::bind("0.0.0.0:0")
            .map_err(|e| GeyserPluginError::Custom(Box::new(e)))?;
        socket
            .connect(&self.udp_dest)
            .map_err(|e| GeyserPluginError::Custom(Box::new(e)))?;
        socket.set_nonblocking(true).ok();
        let _ = UDP_SOCKET.set(socket);

        log::info!(
            "helios-geyser-plugin loaded: {} vaults, udp_dest={}",
            self.vaults.len(),
            self.udp_dest
        );

        Ok(())
    }

    fn update_account(
        &self,
        account: ReplicaAccountInfoVersions,
        _slot: u64,
        is_startup: bool,
    ) -> PluginResult<()> {
        // Skip startup snapshot — too noisy, we'll hydrate from RPC.
        if is_startup {
            return Ok(());
        }

        let info = match account {
            ReplicaAccountInfoVersions::V0_0_1(info) => {
                // V1: pubkey, data, owner, etc.
                (info.pubkey, info.data)
            }
            ReplicaAccountInfoVersions::V0_0_2(info) => (info.pubkey, info.data),
            ReplicaAccountInfoVersions::V0_0_3(info) => (info.pubkey, info.data),
        };

        let (pubkey_bytes, data) = info;

        // Quick filter: is this a vault we care about?
        if pubkey_bytes.len() != 32 {
            return Ok(());
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(pubkey_bytes);
        if !self.vaults.contains(&key) {
            return Ok(());
        }

        // Parse SPL Token amount.
        if data.len() < MIN_TOKEN_ACCOUNT_LEN {
            return Ok(());
        }
        let amount = u64::from_le_bytes(
            data[TOKEN_AMOUNT_OFFSET..TOKEN_AMOUNT_OFFSET + 8]
                .try_into()
                .unwrap_or([0; 8]),
        );

        // Send via UDP: [pubkey 32B][amount 8B][side 1B]
        // Side=0xFF means "unknown, let receiver look up in vault_index"
        let mut packet = [0u8; PACKET_SIZE];
        packet[..32].copy_from_slice(&key);
        packet[32..40].copy_from_slice(&amount.to_le_bytes());
        packet[40] = 0xFF; // receiver resolves side from vault index

        if let Some(socket) = UDP_SOCKET.get() {
            let _ = socket.send(&packet); // non-blocking, drop on failure
        }

        Ok(())
    }

    fn account_data_notifications_enabled(&self) -> bool {
        true
    }

    fn account_data_snapshot_notifications_enabled(&self) -> bool {
        false // Skip startup snapshot
    }

    fn transaction_notifications_enabled(&self) -> bool {
        false // Only need account updates
    }
}

/// Minimal bs58 decoder (no external dep needed in plugin .so).
fn bs58_decode(input: &str) -> Result<Vec<u8>, ()> {
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let mut result = vec![0u8; 64];
    let mut len = 0usize;

    for &c in input.as_bytes() {
        let mut carry = match ALPHABET.iter().position(|&a| a == c) {
            Some(idx) => idx as u32,
            None => return Err(()),
        };
        for byte in result[..len].iter_mut().rev() {
            carry += 58 * (*byte as u32);
            *byte = (carry & 0xFF) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            len += 1;
            if len > result.len() {
                return Err(());
            }
            result[len - 1] = (carry & 0xFF) as u8;
            carry >>= 8;
        }
    }

    // Count leading zeros in input.
    let leading_zeros = input.bytes().take_while(|&b| b == b'1').count();
    let mut output = vec![0u8; leading_zeros];
    // result is in reverse order
    for i in (0..len).rev() {
        output.push(result[i]);
    }
    Ok(output)
}

/// Required C export: creates the plugin instance.
#[no_mangle]
#[allow(improper_ctypes_definitions)]
pub unsafe extern "C" fn _create_plugin() -> *mut dyn GeyserPlugin {
    let plugin = HeliosGeyserPlugin::default();
    let boxed: Box<dyn GeyserPlugin> = Box::new(plugin);
    Box::into_raw(boxed)
}
