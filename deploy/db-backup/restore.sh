#!/bin/bash
# Restore Helios DB from backup
# Usage: DB_URL=postgres://user:pass@host/helios ./restore.sh

DB=${DB_URL:-postgres://postgres:YOUR_PASSWORD@127.0.0.1/helios}

echo "Creating schema..."
psql "$DB" < schema.sql

echo "Loading onchain_arbs..."
psql "$DB" < onchain_arbs.sql

echo "Loading ml_model_state..."
psql "$DB" < ml_model_state.sql

echo "Loading hot_routes..."
psql "$DB" < hot_routes.sql

echo "Loading pool_performance..."
psql "$DB" < pool_performance.sql

echo "Done."
