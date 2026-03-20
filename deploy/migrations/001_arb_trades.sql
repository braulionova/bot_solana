-- Strategy-level PnL tracking for Helios Gold V2
CREATE TABLE IF NOT EXISTS arb_trades (
    id SERIAL PRIMARY KEY,
    strategy VARCHAR(50) NOT NULL,
    timestamp TIMESTAMPTZ DEFAULT NOW(),
    buy_venue VARCHAR(30) NOT NULL,
    sell_venue VARCHAR(30) NOT NULL,
    token_mint VARCHAR(44) NOT NULL,
    token_symbol VARCHAR(20),
    trade_size_sol NUMERIC(18, 9),
    spread_bps NUMERIC(8, 2),
    gross_profit_sol NUMERIC(18, 9),
    jito_tip_sol NUMERIC(18, 9),
    tx_fee_sol NUMERIC(18, 9),
    flash_loan_fee_sol NUMERIC(18, 9),
    net_profit_sol NUMERIC(18, 9),
    latency_ms INTEGER,
    tx_signature VARCHAR(88),
    success BOOLEAN DEFAULT TRUE,
    failure_reason VARCHAR(200)
);

CREATE INDEX IF NOT EXISTS idx_arb_trades_strategy ON arb_trades(strategy, timestamp);
CREATE INDEX IF NOT EXISTS idx_arb_trades_venue ON arb_trades(buy_venue, sell_venue);
CREATE INDEX IF NOT EXISTS idx_arb_trades_token ON arb_trades(token_mint);
