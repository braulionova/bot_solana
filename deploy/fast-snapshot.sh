#!/bin/bash
# Fast snapshot download: cluster peers + SolanaCDN CDN + incrementals
# Usage: ./fast-snapshot.sh [peer_url]
#
# Strategy:
# 1. Auto-discover freshest peer from gossip
# 2. Download full snapshot via snap-fetch (parallel + io_uring)
# 3. Download incremental from peer to minimize repair gap
# 4. Agave loads full + incremental = minimal catch-up

set -euo pipefail
PEER=${1:-""}
OUTDIR="/root/solana-spy-ledger/remote"
SNAP_FETCH="/root/snap-fetch/target/release/snap-fetch"

mkdir -p "$OUTDIR"

# ─── Auto-discover peer ───
if [ -z "$PEER" ]; then
    echo "Discovering peers..."
    for P in \
        "http://67.213.121.219:8899" \
        "http://207.148.14.220:8899" \
        "http://85.195.100.131:8899" \
        "http://64.130.33.184:8899" \
        "http://63.254.162.18:8899"; do
        SLOT=$(curl -s --max-time 3 "$P/snapshot.tar.bz2" -w '%{redirect_url}' -o /dev/null 2>/dev/null | grep -oP 'snapshot-\K[0-9]+' || true)
        if [ -n "$SLOT" ]; then
            PEER="$P"
            echo "  Found: $P (slot $SLOT)"
            break
        fi
    done
fi

if [ -z "$PEER" ]; then
    echo "No peer found, using CDN"
    $SNAP_FETCH --output-dir "$OUTDIR" --latest-only
    exit 0
fi

# ─── Download full snapshot ───
SNAP_NAME=$(curl -s --max-time 5 "$PEER/snapshot.tar.bz2" -w '%{redirect_url}' -o /dev/null 2>/dev/null | sed 's|.*/||')
FULL_SLOT=$(echo "$SNAP_NAME" | grep -oP 'snapshot-\K[0-9]+' || true)
echo "Full snapshot: $SNAP_NAME (slot $FULL_SLOT)"

if [ ! -f "$OUTDIR/$SNAP_NAME" ]; then
    echo "Downloading full snapshot via snap-fetch..."
    $SNAP_FETCH --base-url "$PEER" --output-dir "$OUTDIR" --latest-only 2>&1 || \
        curl -L --progress-bar -o "$OUTDIR/$SNAP_NAME" "$PEER/$SNAP_NAME"
fi

# ─── Download incremental ───
INCR_REDIR=$(curl -s --max-time 5 "$PEER/incremental-snapshot.tar.bz2" -w '%{redirect_url}' -o /dev/null 2>/dev/null || true)
INCR_NAME=$(echo "$INCR_REDIR" | sed 's|.*/||')
INCR_SLOT=$(echo "$INCR_NAME" | grep -oP 'incremental-snapshot-[0-9]+-\K[0-9]+' || true)

if [ -n "$INCR_NAME" ] && [ -n "$INCR_SLOT" ]; then
    echo "Incremental: $INCR_NAME (slot $INCR_SLOT)"
    if [ ! -f "$OUTDIR/$INCR_NAME" ]; then
        echo "Downloading incremental..."
        curl -L --progress-bar -o "$OUTDIR/$INCR_NAME" "$PEER/$INCR_NAME"
    fi
fi

# ─── Also try CDN incrementals ───
CDN_MANIFEST=$(curl -s --max-time 5 "https://data.pipedev.network/snapshot-manifest.json" 2>/dev/null || true)
if [ -n "$CDN_MANIFEST" ] && [ -n "$FULL_SLOT" ]; then
    CDN_INCR=$(echo "$CDN_MANIFEST" | python3 -c "
import sys,json
try:
    d=json.load(sys.stdin)
    for f in d.get('files', d.get('snapshots', [])):
        n = f if isinstance(f,str) else f.get('name','')
        if 'incremental' in n and '$FULL_SLOT' in n:
            print(n)
except: pass
" 2>/dev/null | head -1)
    if [ -n "$CDN_INCR" ]; then
        CDN_INCR_NAME=$(basename "$CDN_INCR")
        CDN_INCR_SLOT=$(echo "$CDN_INCR_NAME" | grep -oP 'incremental-snapshot-[0-9]+-\K[0-9]+' || true)
        if [ -n "$CDN_INCR_SLOT" ] && [ ! -f "$OUTDIR/$CDN_INCR_NAME" ]; then
            # Only use CDN incremental if newer than peer incremental
            if [ -z "$INCR_SLOT" ] || [ "$CDN_INCR_SLOT" -gt "$INCR_SLOT" ]; then
                echo "CDN incremental: $CDN_INCR_NAME (slot $CDN_INCR_SLOT)"
                $SNAP_FETCH --output-dir "$OUTDIR" --latest-only 2>&1 || true
            fi
        fi
    fi
fi

echo ""
echo "=== Done ==="
ls -lh "$OUTDIR"/*.zst 2>/dev/null
echo ""
echo "Start: systemctl start helios-agave"
