#!/usr/bin/env bash
# setup.sh – Deploy Helios Gold V2 on Contabo VPS (Ubuntu 24)
set -euo pipefail

BINARY_DIR="/root/spy_node/target/release"
CONFIG_DIR="/etc/helios"
SERVICE_FILE="/etc/systemd/system/helios-bot.service"
PROXY_SERVICE_FILE="/etc/systemd/system/jito-shredstream-proxy.service"

echo "=== Helios Gold V2 – Setup ==="

# 1. Kernel tuning for UDP performance
echo "── Kernel tuning ──"
cat > /etc/sysctl.d/99-helios.conf << 'EOF'
# Increase UDP receive buffer (Solana shreds)
net.core.rmem_max=134217728
net.core.rmem_default=134217728
net.core.netdev_max_backlog=30000
net.core.optmem_max=65536
net.ipv4.udp_mem=102400 873800 134217728
EOF
sysctl -p /etc/sysctl.d/99-helios.conf

# 2. Config directory
echo "── Config directory ──"
mkdir -p "$CONFIG_DIR"
if [ ! -f "$CONFIG_DIR/helios.env" ]; then
    cp /root/spy_node/deploy/helios.env.example "$CONFIG_DIR/helios.env"
    echo "  → Copied helios.env.example to $CONFIG_DIR/helios.env"
    echo "  → EDIT $CONFIG_DIR/helios.env before starting!"
fi

# 3. Generate identity keypair if missing
if [ ! -f "$CONFIG_DIR/identity.json" ]; then
    echo "── Generating identity keypair ──"
    solana-keygen new --no-bip39-passphrase --outfile "$CONFIG_DIR/identity.json"
fi

# 4. Install systemd service
echo "── Installing systemd service ──"
cp /root/spy_node/deploy/helios-bot.service "$SERVICE_FILE"
cp /root/spy_node/deploy/jito-shredstream-proxy.service "$PROXY_SERVICE_FILE"
systemctl daemon-reload
systemctl enable helios-bot
systemctl enable jito-shredstream-proxy

echo ""
echo "=== Setup complete ==="
echo ""
echo "Next steps:"
echo "  1. Edit $CONFIG_DIR/helios.env (set RPC_URL, PAYER_KEYPAIR, JITO_ENDPOINT)"
echo "  2. Install jito-shredstream-proxy and set JITO_SHREDSTREAM_* vars"
echo "  3. Place the Jito auth keypair at the configured path"
echo "  4. Set DRY_RUN=true in helios.env for initial testing"
echo "  5. Start: systemctl start jito-shredstream-proxy helios-bot"
echo "  6. Logs:  journalctl -fu jito-shredstream-proxy"
echo "  7. Logs:  journalctl -fu helios-bot"
echo "  8. Metrics: curl http://localhost:9090/metrics"
