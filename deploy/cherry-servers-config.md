# Cherry Servers — Recommended Configuration for Helios Spy Node

## Hardware Specs (Minimum)

| Component     | Minimum                     | Recommended                  |
|---------------|-----------------------------|-----------------------------|
| CPU           | AMD EPYC 7443P (24c/48t)   | AMD EPYC 9454P (48c/96t)   |
| RAM           | 256 GB DDR4 ECC             | 512 GB DDR5 ECC            |
| NVMe (ledger) | 2 TB Gen4 NVMe             | 4 TB Gen4 NVMe (RAID-0 2x) |
| NVMe (accounts) | 2 TB Gen4 NVMe           | 4 TB Gen4 NVMe             |
| Network       | 10 Gbps unmetered           | 25 Gbps unmetered          |
| Location      | Frankfurt (FRA1)            | Frankfurt (FRA1)           |

Cherry Servers E5 (AMD EPYC 9454P, 384GB, 2x3.84TB NVMe) at ~$350/mo is the sweet spot.

## Disk Layout

```bash
# Separate NVMe for accounts (highest IOPS)
/dev/nvme0n1 → /mnt/ledger      (ledger + rocksdb + snapshots)
/dev/nvme1n1 → /mnt/accounts    (accounts DB — heaviest I/O)

# Mount with performance options
mount -o noatime,discard,lazytime /dev/nvme0n1 /mnt/ledger
mount -o noatime,discard,lazytime /dev/nvme1n1 /mnt/accounts
```

## Kernel + Network Tuning

```bash
# /etc/sysctl.d/99-helios-validator.conf

# UDP buffer sizes (critical for Turbine shreds)
net.core.rmem_max = 134217728
net.core.rmem_default = 134217728
net.core.wmem_max = 134217728
net.core.wmem_default = 134217728

# No UDP rate limiting
net.core.netdev_budget = 600
net.core.netdev_budget_usecs = 6000

# Increase backlog for burst shred traffic
net.core.netdev_max_backlog = 50000
net.core.somaxconn = 65535

# Disable conntrack for UDP (validator traffic)
# Alternative: use iptables NOTRACK rule on TVU/repair ports

# Memory pressure
vm.swappiness = 10
vm.vfs_cache_pressure = 200
vm.dirty_ratio = 10
vm.dirty_background_ratio = 5
vm.max_map_count = 2000000

# File descriptors
fs.file-max = 2097152
fs.nr_open = 2097152
```

```bash
# /etc/security/limits.d/99-helios.conf
* soft nofile 1048576
* hard nofile 1048576
* soft nproc 1048576
* hard nproc 1048576
```

## Firewall — No UDP Rate Limiting

```bash
# CRITICAL: Do NOT use iptables rate limiting on these ports
# Cherry Servers default firewall may throttle UDP — disable or whitelist

# Required open ports (UDP)
# 8801  — Gossip
# 8802  — TVU (Turbine shreds inbound)
# 8803-8812 — TVU forwards, repair, serve-repair, etc.
# 8002  — Helios spy-node ingest (if separate from Agave TVU)
# 20000 — Jito ShredStream (if using proxy)

# Required open ports (TCP)
# 9000  — RPC (localhost only)
# 10000 — Richat gRPC (localhost only)
# 8081  — sim-server (localhost only)

# Example: iptables NOTRACK for validator UDP traffic
iptables -t raw -A PREROUTING -p udp --dport 8800:8820 -j NOTRACK
iptables -t raw -A OUTPUT -p udp --sport 8800:8820 -j NOTRACK
```

## ShredStream Configuration

### Jito ShredStream (official, requires whitelist)
```bash
# Apply for whitelist at https://jito.network/shredstream/
# Register the validator identity pubkey

# Service: jito-shredstream-proxy
# Binary: /usr/local/bin/jito-shredstream-proxy
# Config:
BLOCK_ENGINE_URL=https://fra.mainnet.block-engine.jito.wtf
SHRED_RECEIVER=127.0.0.1:8002
AUTH_KEYPAIR=/root/validator-identity.json

# Expected latency improvement: Turbine p50 ~669ms → ShredStream p50 ~50-100ms
```

### Shredcaster (alternative, no whitelist)
```bash
# ERPC Frankfurt (7-day free trial via Validators DAO Discord)
# gRPC endpoint: shreds-fra.erpc.global
# Requires x-token header (not compatible with jito-shredstream-proxy --block-engine-url)
# Use dedicated gRPC subscriber binary instead

# RPCFast / Solana Vibe Station — other alternatives
# Check current availability and pricing
```

## Agave Validator Service (helios-agave with patches)

```ini
# /etc/systemd/system/helios-agave.service
[Unit]
Description=Helios Agave Validator (no-vote, helios-mode)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=root
Environment="MALLOC_CONF=narenas:4,background_thread:true,metadata_thp:auto,dirty_decay_ms:500,muzzy_decay_ms:1000"
ExecStart=/root/helios-agave/target/release/agave-validator \
    --helios-mode \
    --identity /root/validator-identity.json \
    --ledger /mnt/ledger \
    --accounts /mnt/accounts/accounts \
    --entrypoint entrypoint.mainnet-beta.solana.com:8001 \
    --entrypoint entrypoint2.mainnet-beta.solana.com:8001 \
    --entrypoint entrypoint3.mainnet-beta.solana.com:8001 \
    --entrypoint entrypoint4.mainnet-beta.solana.com:8001 \
    --entrypoint entrypoint5.mainnet-beta.solana.com:8001 \
    --known-validator 5D1fNXzvv5NjV1ysLjirC4WY92RNsVH18vjmcszZd8on \
    --known-validator dDzy5SR3AXdYWVqbDEkVFdvSPCtS9ihF5kJkHCtXoFs \
    --known-validator Ft5fbkqNa76vnsjYNwjDZUXoTWpP7VYm3mtsaQckQADN \
    --known-validator eoKpUABi59aT4rR9HGS3LcMecfut9x7zJyodWWP43YQ \
    --known-validator 9QxCLckBiJc783jnMvXZubK4wH86Eqqvashtrds2K7Zq \
    --known-validator CakcnaRDHka2gXyfbEd2d3xsvkJkqsLw2akB3zsN1D2S \
    --gossip-port 8801 \
    --dynamic-port-range 8802-8820 \
    --rpc-port 9000 \
    --rpc-bind-address 127.0.0.1 \
    --private-rpc \
    --no-port-check \
    --wal-recovery-mode skip_any_corrupted_record \
    --limit-ledger-size 50000000 \
    --accounts-db-cache-limit-mb 4096 \
    --unified-scheduler-handler-threads 8 \
    --rpc-threads 4 \
    --rpc-pubsub-worker-threads 2 \
    --no-incremental-snapshots \
    --maximum-incremental-snapshots-to-retain 0 \
    --accounts-shrink-ratio 0.8 \
    --expected-shred-version 50093 \
    --log /var/log/helios-agave.log
Restart=always
RestartSec=10
LimitNOFILE=1048576
MemoryMax=220G
MemorySwapMax=40G
MemoryHigh=200G

[Install]
WantedBy=multi-user.target
```

## Helios Bot Service

```ini
# /etc/systemd/system/helios-bot.service
[Unit]
Description=Helios Arb Bot
After=network-online.target helios-agave.service
Wants=helios-agave.service

[Service]
Type=simple
User=root
EnvironmentFile=/etc/helios/helios.env
ExecStart=/root/spy_node/target/release/helios
Restart=always
RestartSec=5
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
```

## Migration Checklist

1. [ ] Provision Cherry Servers E5 in Frankfurt
2. [ ] Format NVMe drives, create mount points
3. [ ] Apply kernel tuning (sysctl, limits)
4. [ ] Disable firewall UDP rate limiting
5. [ ] Install Rust 1.93+, libclang-18, PostgreSQL, jq
6. [ ] Clone helios-agave fork, build with patches
7. [ ] Clone spy_node, build workspace
8. [ ] Copy identity keypair, wallet, helios.env
9. [ ] Download fresh snapshot via `agave-validator --no-genesis-fetch`
10. [ ] Start helios-agave, wait for catchup
11. [ ] Apply for Jito ShredStream whitelist (or use ERPC trial)
12. [ ] Start shred-mirror, sim-server, helios-bot
13. [ ] Verify: shreds/s > 1500, repair working, route engine < 20ms
14. [ ] Switch DNS/config to new server, decommission Contabo
