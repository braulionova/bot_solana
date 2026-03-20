#!/bin/bash
# Restore Helios DB from backup
# Usage: DB_URL=postgres://user:pass@host/helios ./restore.sh
DB=${DB_URL:-postgres://postgres:YOUR_PASSWORD@127.0.0.1/helios}

echo "=== Creating schema ==="
psql "$DB" < schema.sql

echo "=== Restoring small tables (SQL inserts) ==="
for f in pg-full/*.sql.gz; do
  TABLE=$(basename "$f" .sql.gz)
  echo "  $TABLE..."
  gunzip -c "$f" | psql "$DB" 2>/dev/null
done

echo "=== Restoring large tables (pg_dump custom format) ==="
for f in pg-full/*.dump; do
  TABLE=$(basename "$f" .dump)
  echo "  $TABLE..."
  pg_restore -d "$DB" --data-only --no-owner "$f" 2>/dev/null
done

echo "=== Restoring split tables ==="
for PREFIX in pg-full/wallet_swaps.dump pg-full/pool_snapshots.dump; do
  if ls ${PREFIX}.part-* 1>/dev/null 2>&1; then
    TABLE=$(basename "$PREFIX" .dump)
    echo "  $TABLE (reassembling parts)..."
    cat ${PREFIX}.part-* > /tmp/${TABLE}.dump
    pg_restore -d "$DB" --data-only --no-owner /tmp/${TABLE}.dump 2>/dev/null
    rm /tmp/${TABLE}.dump
  fi
done

echo "=== Restoring ML models ==="
gunzip -c ml-data/ml_models.sql.gz | psql "$DB" 2>/dev/null

echo "=== Done ==="
psql "$DB" -c "SELECT table_name, pg_size_pretty(pg_total_relation_size(quote_ident(table_name))) FROM information_schema.tables WHERE table_schema='public' ORDER BY pg_total_relation_size(quote_ident(table_name)) DESC;"
