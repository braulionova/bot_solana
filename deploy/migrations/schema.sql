--
-- PostgreSQL database dump
--

\restrict XDHqrJB7oEMnwW236FyzUAUc7opPbftoKbDnGdkFw5hVJm7rO9YWd1QVWNsQkZF

-- Dumped from database version 16.13 (Ubuntu 16.13-0ubuntu0.24.04.1)
-- Dumped by pg_dump version 16.13 (Ubuntu 16.13-0ubuntu0.24.04.1)

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: arb_trades; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.arb_trades (
    id integer NOT NULL,
    strategy character varying(50) NOT NULL,
    "timestamp" timestamp with time zone DEFAULT now(),
    buy_venue character varying(30) NOT NULL,
    sell_venue character varying(30) NOT NULL,
    token_mint character varying(44) NOT NULL,
    token_symbol character varying(20),
    trade_size_sol numeric(18,9),
    spread_bps numeric(8,2),
    gross_profit_sol numeric(18,9),
    jito_tip_sol numeric(18,9),
    tx_fee_sol numeric(18,9),
    flash_loan_fee_sol numeric(18,9),
    net_profit_sol numeric(18,9),
    latency_ms integer,
    tx_signature character varying(88),
    success boolean DEFAULT true,
    failure_reason character varying(200)
);


--
-- Name: arb_trades_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.arb_trades_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: arb_trades_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.arb_trades_id_seq OWNED BY public.arb_trades.id;


--
-- Name: executions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.executions (
    id bigint NOT NULL,
    opp_db_id bigint,
    tx_signature text,
    send_channels text[],
    fast_arb boolean,
    detect_to_build_us bigint,
    build_to_send_us bigint,
    total_us bigint,
    jito_tip bigint,
    tpu_leaders smallint,
    landed boolean,
    landed_slot bigint,
    actual_profit bigint,
    error_message text,
    created_at timestamp with time zone DEFAULT now()
);


--
-- Name: executions_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.executions_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: executions_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.executions_id_seq OWNED BY public.executions.id;


--
-- Name: historical_executions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.historical_executions (
    id integer NOT NULL,
    ts timestamp with time zone,
    mint text,
    route text,
    stage text,
    status text,
    reason_code text,
    latency_ms integer,
    tx_size integer,
    source text DEFAULT 'sqlite_import'::text
);


--
-- Name: historical_executions_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.historical_executions_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: historical_executions_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.historical_executions_id_seq OWNED BY public.historical_executions.id;


--
-- Name: historical_missed; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.historical_missed (
    id integer NOT NULL,
    ts timestamp with time zone,
    mint text,
    symbol text,
    reason text,
    est_profit_lamports bigint,
    amount_in_lamports bigint,
    liquidity_usd double precision,
    dex_path text,
    reason_code text,
    spread_bps integer,
    source text DEFAULT 'sqlite_import'::text
);


--
-- Name: historical_missed_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.historical_missed_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: historical_missed_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.historical_missed_id_seq OWNED BY public.historical_missed.id;


--
-- Name: hot_routes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.hot_routes (
    id bigint NOT NULL,
    route_key text NOT NULL,
    hops smallint NOT NULL,
    tokens text[] NOT NULL,
    pools text[] NOT NULL,
    dex_types text[] NOT NULL,
    borrow_mint text NOT NULL,
    borrow_amount bigint NOT NULL,
    gross_profit_lamports bigint NOT NULL,
    net_profit_lamports bigint NOT NULL,
    min_liquidity_usd double precision NOT NULL,
    total_fee_bps integer NOT NULL,
    price_impact_pct double precision NOT NULL,
    competition_score double precision DEFAULT 0.0 NOT NULL,
    ml_score double precision DEFAULT 0.0 NOT NULL,
    route_uniqueness double precision DEFAULT 0.0 NOT NULL,
    dex_diversity smallint DEFAULT 1 NOT NULL,
    scanned_at timestamp with time zone DEFAULT now(),
    expires_at timestamp with time zone DEFAULT (now() + '01:00:00'::interval)
);


--
-- Name: hot_routes_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.hot_routes_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: hot_routes_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.hot_routes_id_seq OWNED BY public.hot_routes.id;


--
-- Name: ml_model_state; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.ml_model_state (
    model_name text NOT NULL,
    weights_json jsonb NOT NULL,
    n_samples bigint DEFAULT 0,
    updated_at timestamp with time zone DEFAULT now()
);


--
-- Name: onchain_arbs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.onchain_arbs (
    id bigint NOT NULL,
    signature text NOT NULL,
    slot bigint NOT NULL,
    block_time bigint,
    pools text[] NOT NULL,
    dex_types text[] NOT NULL,
    n_swaps integer NOT NULL,
    uses_flash_loan boolean NOT NULL,
    profit_lamports bigint NOT NULL,
    route_key text NOT NULL,
    hour smallint NOT NULL,
    created_at timestamp with time zone DEFAULT now()
);


--
-- Name: onchain_arbs_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.onchain_arbs_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: onchain_arbs_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.onchain_arbs_id_seq OWNED BY public.onchain_arbs.id;


--
-- Name: opportunities; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.opportunities (
    id bigint NOT NULL,
    opp_id bigint NOT NULL,
    slot bigint,
    detected_at timestamp with time zone DEFAULT now(),
    signal_type text,
    source_strategy text,
    token_mints text[],
    borrow_amount bigint,
    flash_provider text,
    gross_profit bigint,
    net_profit bigint,
    score double precision,
    risk_factor double precision,
    n_hops smallint,
    fast_arb boolean DEFAULT false,
    created_at timestamp with time zone DEFAULT now()
);


--
-- Name: opportunities_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.opportunities_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: opportunities_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.opportunities_id_seq OWNED BY public.opportunities.id;


--
-- Name: opportunity_hops; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.opportunity_hops (
    id bigint NOT NULL,
    opp_db_id bigint,
    hop_index smallint,
    pool text,
    dex_type text,
    token_in text,
    token_out text,
    amount_in bigint,
    amount_out bigint,
    price_impact double precision,
    pool_reserve_a bigint,
    pool_reserve_b bigint,
    reserve_age_ms bigint
);


--
-- Name: opportunity_hops_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.opportunity_hops_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: opportunity_hops_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.opportunity_hops_id_seq OWNED BY public.opportunity_hops.id;


--
-- Name: pool_discoveries; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.pool_discoveries (
    id bigint NOT NULL,
    pool_address text NOT NULL,
    dex_type text,
    token_a text,
    token_b text,
    initial_reserve_a bigint,
    initial_reserve_b bigint,
    discovery_source text,
    discovery_slot bigint,
    created_at timestamp with time zone DEFAULT now()
);


--
-- Name: pool_discoveries_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.pool_discoveries_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: pool_discoveries_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.pool_discoveries_id_seq OWNED BY public.pool_discoveries.id;


--
-- Name: pool_performance; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.pool_performance (
    pool_address text NOT NULL,
    dex_type text NOT NULL,
    total_opps bigint DEFAULT 0,
    total_sent bigint DEFAULT 0,
    total_landed bigint DEFAULT 0,
    total_profit bigint DEFAULT 0,
    avg_score double precision DEFAULT 0.0,
    last_success timestamp with time zone,
    last_failure timestamp with time zone,
    updated_at timestamp with time zone DEFAULT now()
);


--
-- Name: pool_snapshots; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.pool_snapshots (
    id bigint NOT NULL,
    pool_address text NOT NULL,
    reserve_a bigint,
    reserve_b bigint,
    slot bigint,
    ts timestamp with time zone DEFAULT now(),
    dex_type text,
    token_a text,
    token_b text,
    fee_bps smallint
);


--
-- Name: pool_snapshots_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.pool_snapshots_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: pool_snapshots_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.pool_snapshots_id_seq OWNED BY public.pool_snapshots.id;


--
-- Name: route_candidates; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.route_candidates (
    id bigint NOT NULL,
    slot bigint,
    detected_at timestamp with time zone,
    strategy text,
    borrow_amount bigint,
    n_hops smallint,
    gross_profit bigint,
    net_profit bigint,
    score double precision,
    ml_boost double precision,
    risk_factor double precision,
    emitted boolean,
    pools text[],
    token_mints text[],
    reserves_a bigint[],
    reserves_b bigint[],
    created_at timestamp with time zone DEFAULT now(),
    reject_reason text
);


--
-- Name: route_candidates_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.route_candidates_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: route_candidates_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.route_candidates_id_seq OWNED BY public.route_candidates.id;


--
-- Name: send_results; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.send_results (
    id bigint NOT NULL,
    opp_id bigint,
    channel text,
    tx_signature text,
    success boolean,
    latency_us bigint,
    error_message text,
    created_at timestamp with time zone DEFAULT now()
);


--
-- Name: send_results_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.send_results_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: send_results_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.send_results_id_seq OWNED BY public.send_results.id;


--
-- Name: shred_predictor_state; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.shred_predictor_state (
    id integer NOT NULL,
    model_type text NOT NULL,
    model_key text NOT NULL,
    model_json jsonb NOT NULL,
    updated_at timestamp with time zone DEFAULT now()
);


--
-- Name: shred_predictor_state_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.shred_predictor_state_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: shred_predictor_state_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.shred_predictor_state_id_seq OWNED BY public.shred_predictor_state.id;


--
-- Name: sniper_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.sniper_events (
    id bigint NOT NULL,
    slot bigint,
    token_mint text,
    pool text,
    source text,
    safety_passed boolean,
    safety_reason text,
    pool_sol_reserve bigint,
    snipe_lamports bigint,
    tx_signature text,
    build_ms bigint,
    send_ms bigint,
    sell_triggered boolean,
    sell_reason text,
    sell_tx_sig text,
    sell_profit_lamports bigint,
    sell_ms bigint,
    created_at timestamp with time zone DEFAULT now(),
    sell_sol_received bigint,
    hold_duration_ms bigint,
    pnl_lamports bigint
);


--
-- Name: sniper_events_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.sniper_events_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: sniper_events_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.sniper_events_id_seq OWNED BY public.sniper_events.id;


--
-- Name: tracked_wallets; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tracked_wallets (
    wallet_address text NOT NULL,
    label text,
    category text,
    total_swaps bigint DEFAULT 0,
    total_profit_lamports bigint DEFAULT 0,
    win_rate double precision DEFAULT 0,
    last_swap_slot bigint,
    last_swap_at timestamp with time zone,
    updated_at timestamp with time zone DEFAULT now()
);


--
-- Name: wallet_patterns; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.wallet_patterns (
    id bigint NOT NULL,
    pattern_type text NOT NULL,
    wallet_address text NOT NULL,
    token_mint text,
    pool text,
    dex_type text,
    occurrences bigint DEFAULT 0,
    success_count bigint DEFAULT 0,
    avg_profit double precision DEFAULT 0,
    avg_delay_ms bigint DEFAULT 0,
    confidence double precision DEFAULT 0,
    features jsonb,
    updated_at timestamp with time zone DEFAULT now()
);


--
-- Name: wallet_patterns_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.wallet_patterns_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: wallet_patterns_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.wallet_patterns_id_seq OWNED BY public.wallet_patterns.id;


--
-- Name: wallet_swaps; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.wallet_swaps (
    id bigint NOT NULL,
    wallet_address text NOT NULL,
    tx_signature text,
    slot bigint,
    dex_type text,
    pool text,
    token_in text,
    token_out text,
    amount_in bigint,
    amount_out bigint,
    is_buy boolean,
    price_impact_bps double precision,
    pool_reserve_a bigint,
    pool_reserve_b bigint,
    created_at timestamp with time zone DEFAULT now()
);


--
-- Name: wallet_swaps_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.wallet_swaps_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: wallet_swaps_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.wallet_swaps_id_seq OWNED BY public.wallet_swaps.id;


--
-- Name: arb_trades id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.arb_trades ALTER COLUMN id SET DEFAULT nextval('public.arb_trades_id_seq'::regclass);


--
-- Name: executions id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.executions ALTER COLUMN id SET DEFAULT nextval('public.executions_id_seq'::regclass);


--
-- Name: historical_executions id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.historical_executions ALTER COLUMN id SET DEFAULT nextval('public.historical_executions_id_seq'::regclass);


--
-- Name: historical_missed id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.historical_missed ALTER COLUMN id SET DEFAULT nextval('public.historical_missed_id_seq'::regclass);


--
-- Name: hot_routes id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.hot_routes ALTER COLUMN id SET DEFAULT nextval('public.hot_routes_id_seq'::regclass);


--
-- Name: onchain_arbs id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.onchain_arbs ALTER COLUMN id SET DEFAULT nextval('public.onchain_arbs_id_seq'::regclass);


--
-- Name: opportunities id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.opportunities ALTER COLUMN id SET DEFAULT nextval('public.opportunities_id_seq'::regclass);


--
-- Name: opportunity_hops id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.opportunity_hops ALTER COLUMN id SET DEFAULT nextval('public.opportunity_hops_id_seq'::regclass);


--
-- Name: pool_discoveries id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.pool_discoveries ALTER COLUMN id SET DEFAULT nextval('public.pool_discoveries_id_seq'::regclass);


--
-- Name: pool_snapshots id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.pool_snapshots ALTER COLUMN id SET DEFAULT nextval('public.pool_snapshots_id_seq'::regclass);


--
-- Name: route_candidates id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.route_candidates ALTER COLUMN id SET DEFAULT nextval('public.route_candidates_id_seq'::regclass);


--
-- Name: send_results id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.send_results ALTER COLUMN id SET DEFAULT nextval('public.send_results_id_seq'::regclass);


--
-- Name: shred_predictor_state id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.shred_predictor_state ALTER COLUMN id SET DEFAULT nextval('public.shred_predictor_state_id_seq'::regclass);


--
-- Name: sniper_events id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.sniper_events ALTER COLUMN id SET DEFAULT nextval('public.sniper_events_id_seq'::regclass);


--
-- Name: wallet_patterns id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.wallet_patterns ALTER COLUMN id SET DEFAULT nextval('public.wallet_patterns_id_seq'::regclass);


--
-- Name: wallet_swaps id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.wallet_swaps ALTER COLUMN id SET DEFAULT nextval('public.wallet_swaps_id_seq'::regclass);


--
-- Name: arb_trades arb_trades_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.arb_trades
    ADD CONSTRAINT arb_trades_pkey PRIMARY KEY (id);


--
-- Name: executions executions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.executions
    ADD CONSTRAINT executions_pkey PRIMARY KEY (id);


--
-- Name: historical_executions historical_executions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.historical_executions
    ADD CONSTRAINT historical_executions_pkey PRIMARY KEY (id);


--
-- Name: historical_missed historical_missed_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.historical_missed
    ADD CONSTRAINT historical_missed_pkey PRIMARY KEY (id);


--
-- Name: hot_routes hot_routes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.hot_routes
    ADD CONSTRAINT hot_routes_pkey PRIMARY KEY (id);


--
-- Name: hot_routes hot_routes_route_key_borrow_amount_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.hot_routes
    ADD CONSTRAINT hot_routes_route_key_borrow_amount_key UNIQUE (route_key, borrow_amount);


--
-- Name: ml_model_state ml_model_state_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ml_model_state
    ADD CONSTRAINT ml_model_state_pkey PRIMARY KEY (model_name);


--
-- Name: onchain_arbs onchain_arbs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.onchain_arbs
    ADD CONSTRAINT onchain_arbs_pkey PRIMARY KEY (id);


--
-- Name: onchain_arbs onchain_arbs_signature_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.onchain_arbs
    ADD CONSTRAINT onchain_arbs_signature_key UNIQUE (signature);


--
-- Name: opportunities opportunities_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.opportunities
    ADD CONSTRAINT opportunities_pkey PRIMARY KEY (id);


--
-- Name: opportunity_hops opportunity_hops_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.opportunity_hops
    ADD CONSTRAINT opportunity_hops_pkey PRIMARY KEY (id);


--
-- Name: pool_discoveries pool_discoveries_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.pool_discoveries
    ADD CONSTRAINT pool_discoveries_pkey PRIMARY KEY (id);


--
-- Name: pool_performance pool_performance_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.pool_performance
    ADD CONSTRAINT pool_performance_pkey PRIMARY KEY (pool_address);


--
-- Name: pool_snapshots pool_snapshots_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.pool_snapshots
    ADD CONSTRAINT pool_snapshots_pkey PRIMARY KEY (id);


--
-- Name: route_candidates route_candidates_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.route_candidates
    ADD CONSTRAINT route_candidates_pkey PRIMARY KEY (id);


--
-- Name: send_results send_results_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.send_results
    ADD CONSTRAINT send_results_pkey PRIMARY KEY (id);


--
-- Name: shred_predictor_state shred_predictor_state_model_type_model_key_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.shred_predictor_state
    ADD CONSTRAINT shred_predictor_state_model_type_model_key_key UNIQUE (model_type, model_key);


--
-- Name: shred_predictor_state shred_predictor_state_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.shred_predictor_state
    ADD CONSTRAINT shred_predictor_state_pkey PRIMARY KEY (id);


--
-- Name: sniper_events sniper_events_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.sniper_events
    ADD CONSTRAINT sniper_events_pkey PRIMARY KEY (id);


--
-- Name: tracked_wallets tracked_wallets_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tracked_wallets
    ADD CONSTRAINT tracked_wallets_pkey PRIMARY KEY (wallet_address);


--
-- Name: wallet_patterns wallet_patterns_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.wallet_patterns
    ADD CONSTRAINT wallet_patterns_pkey PRIMARY KEY (id);


--
-- Name: wallet_swaps wallet_swaps_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.wallet_swaps
    ADD CONSTRAINT wallet_swaps_pkey PRIMARY KEY (id);


--
-- Name: idx_arb_trades_strategy; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_arb_trades_strategy ON public.arb_trades USING btree (strategy, "timestamp");


--
-- Name: idx_arb_trades_token; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_arb_trades_token ON public.arb_trades USING btree (token_mint);


--
-- Name: idx_arb_trades_venue; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_arb_trades_venue ON public.arb_trades USING btree (buy_venue, sell_venue);


--
-- Name: idx_executions_landed; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_executions_landed ON public.executions USING btree (landed);


--
-- Name: idx_hot_routes_competition; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_hot_routes_competition ON public.hot_routes USING btree (competition_score);


--
-- Name: idx_hot_routes_expires; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_hot_routes_expires ON public.hot_routes USING btree (expires_at);


--
-- Name: idx_hot_routes_profit; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_hot_routes_profit ON public.hot_routes USING btree (net_profit_lamports DESC);


--
-- Name: idx_hot_routes_score; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_hot_routes_score ON public.hot_routes USING btree (ml_score DESC);


--
-- Name: idx_onchain_arbs_profit; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_onchain_arbs_profit ON public.onchain_arbs USING btree (profit_lamports DESC);


--
-- Name: idx_onchain_arbs_route; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_onchain_arbs_route ON public.onchain_arbs USING btree (route_key);


--
-- Name: idx_opportunities_strategy; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_opportunities_strategy ON public.opportunities USING btree (source_strategy);


--
-- Name: idx_pool_snapshots_pool; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_pool_snapshots_pool ON public.pool_snapshots USING btree (pool_address);


--
-- Name: idx_pool_snapshots_ts; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_pool_snapshots_ts ON public.pool_snapshots USING btree (ts);


--
-- Name: executions executions_opp_db_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.executions
    ADD CONSTRAINT executions_opp_db_id_fkey FOREIGN KEY (opp_db_id) REFERENCES public.opportunities(id);


--
-- Name: opportunity_hops opportunity_hops_opp_db_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.opportunity_hops
    ADD CONSTRAINT opportunity_hops_opp_db_id_fkey FOREIGN KEY (opp_db_id) REFERENCES public.opportunities(id);


--
-- PostgreSQL database dump complete
--

\unrestrict XDHqrJB7oEMnwW236FyzUAUc7opPbftoKbDnGdkFw5hVJm7rO9YWd1QVWNsQkZF

