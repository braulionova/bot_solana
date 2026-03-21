/// Flash Fill Arb: Jupiter-detected arb executed via flash loan in 1 atomic TX.
/// Pattern: [compute_budget, kamino_borrow, jupiter_buy_ixs, jupiter_sell_ixs, kamino_repay]
/// All in 1 TX with ALTs → < 1232 bytes → atomic (fail = 0 cost)

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    sysvar,
    transaction::VersionedTransaction,
};

const KAMINO: &str = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD";
const WSOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

fn main() -> Result<()> {
    let api_key = std::env::var("JUPITER_API_KEY").context("JUPITER_API_KEY")?;
    let wb: Vec<u8> = serde_json::from_str(&std::fs::read_to_string("/root/solana-bot/wallet.json")?)?;
    let kp = Keypair::from_bytes(&wb).map_err(|e| anyhow!("{}", e))?;
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build()?;
    let continuous = std::env::var("CONTINUOUS").map(|v| v == "true").unwrap_or(false);
    let amount = std::env::var("ARB_AMOUNT").unwrap_or("200000000".into());
    let min_profit: i64 = std::env::var("MIN_PROFIT").and_then(|v| Ok(v.parse().unwrap_or(10000))).unwrap_or(10000);
    let taker = kp.pubkey().to_string();

    println!("Flash Fill Arb | wallet: {} | amount: {} | min_profit: {}", taker, amount, min_profit);

    let tokens = [USDC, USDT];
    let mut round = 0u64;

    loop {
        round += 1;
        for &token in &tokens {
            match run_flash_arb(&client, &api_key, &kp, token, &amount, &taker, min_profit) {
                Ok(true) => println!("🎉 Round {} — flash TX sent!", round),
                Ok(false) => {}
                Err(e) => {
                    let msg = format!("{}", e);
                    if !msg.contains("Not profitable") && !msg.contains("no quote") {
                        println!("⚠️ {}", msg);
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        if !continuous { break; }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    Ok(())
}

fn run_flash_arb(client: &reqwest::blocking::Client, api_key: &str, kp: &Keypair,
    token: &str, amount: &str, taker: &str, min_profit: i64) -> Result<bool> {

    // 1. Jupiter quotes
    let buy_quote = jup_quote(client, api_key, WSOL, token, amount)?;
    let tokens_out = buy_quote.get("outAmount").and_then(|v| v.as_str()).unwrap_or("0");
    if tokens_out == "0" { return Err(anyhow!("no quote")); }

    // Use 98% of buy output for sell to account for buy slippage/fees
    // Flash loan repay enforces profitability, so leaving 2% buffer is safe
    let sell_amount: u64 = (tokens_out.parse::<u64>().unwrap_or(0) as f64 * 0.995) as u64;
    let sell_amount_str = sell_amount.to_string();
    let sell_quote = jup_quote(client, api_key, token, WSOL, &sell_amount_str)?;
    let sol_back = sell_quote.get("outAmount").and_then(|v| v.as_str()).unwrap_or("0");
    let profit: i64 = sol_back.parse::<i64>().unwrap_or(0) - amount.parse::<i64>().unwrap_or(0);

    if profit < min_profit {
        return Err(anyhow!("Not profitable: {} < {}", profit, min_profit));
    }
    println!("✅ Arb +{} lam | {} → {} {} → {} SOL", profit, amount, tokens_out, &token[..8], sol_back);

    // 2. Get Jupiter swap TXs
    let buy_tx_b64 = jup_swap_tx(client, api_key, &buy_quote, taker)?;
    let sell_tx_b64 = jup_swap_tx(client, api_key, &sell_quote, taker)?;

    // 3. Extract IXs + ALTs from both TXs
    let (buy_ixs, buy_alts) = extract_ixs_and_alts(&buy_tx_b64)?;
    let (sell_ixs, sell_alts) = extract_ixs_and_alts(&sell_tx_b64)?;

    // 4. Build Solend flash borrow + repay (confirmed working, no "reserve not supported")
    let solend_program: Pubkey = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo".parse()?;
    let token_program: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse()?;
    let borrow_amount: u64 = amount.parse()?;

    // Solend SOL reserve accounts (from flash_accounts.env)
    let reserve: Pubkey = std::env::var("SOLEND_SOL_RESERVE")
        .unwrap_or("8PbodeaosQP19SjYFx855UMqWxH2HynZLdBXmsrbac36".into()).parse()?;
    let liquidity_supply: Pubkey = std::env::var("SOLEND_SOL_LIQUIDITY_SUPPLY")
        .unwrap_or("8UviNr47S8eL6J3WfDxMRa3hvLta1VDJwNWqsDgtN3Cv".into()).parse()?;
    let fee_receiver: Pubkey = std::env::var("SOLEND_SOL_FEE_RECEIVER")
        .unwrap_or("5wo1tFpi4HaVKnemqaXeQnBEpezrJXcXvuztYaPhvgC7".into()).parse()?;
    let lending_market: Pubkey = std::env::var("SOLEND_SOL_LENDING_MARKET")
        .unwrap_or("4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY".into()).parse()?;
    let (market_authority, _) = Pubkey::find_program_address(
        &[lending_market.as_ref()], &solend_program);

    // User's WSOL ATA
    let wsol_mint: Pubkey = WSOL.parse()?;
    let user_wsol_ata = Pubkey::find_program_address(
        &[kp.pubkey().as_ref(), token_program.as_ref(), wsol_mint.as_ref()],
        &"ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse::<Pubkey>()?,
    ).0;

    // Solend flash borrow: data = [19] + amount(u64 LE)
    let mut borrow_data = vec![19u8];
    borrow_data.extend_from_slice(&borrow_amount.to_le_bytes());

    let borrow_ix = Instruction {
        program_id: solend_program,
        accounts: vec![
            AccountMeta::new(liquidity_supply, false),
            AccountMeta::new(user_wsol_ata, false),
            AccountMeta::new(reserve, false),
            AccountMeta::new_readonly(lending_market, false),
            AccountMeta::new_readonly(market_authority, false),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data: borrow_data,
    };

    // Solend flash repay: data = [20] + amount(u64 LE) + borrow_ix_index(u8)
    let preceding = 3u8; // compute_budget IXs + create_ata before borrow
    let mut repay_data = vec![20u8];
    repay_data.extend_from_slice(&borrow_amount.to_le_bytes());
    repay_data.push(preceding);

    let repay_ix = Instruction {
        program_id: solend_program,
        accounts: vec![
            AccountMeta::new(user_wsol_ata, false),
            AccountMeta::new(liquidity_supply, false),
            AccountMeta::new(fee_receiver, false),
            AccountMeta::new_readonly(Pubkey::default(), false), // host_fee_receiver (optional)
            AccountMeta::new(reserve, false),
            AccountMeta::new_readonly(lending_market, false),
            AccountMeta::new(kp.pubkey(), true),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data: repay_data,
    };

    // 5. Assemble: [compute, compute, create_wsol_ata, borrow, buy_ixs..., sell_ixs..., repay]
    // Create WSOL ATA idempotent (needed for Solend to deposit borrowed SOL)
    let ata_program: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse()?;
    let create_ata_ix = Instruction {
        program_id: ata_program,
        accounts: vec![
            AccountMeta::new(kp.pubkey(), true),
            AccountMeta::new(user_wsol_ata, false),
            AccountMeta::new_readonly(kp.pubkey(), false),
            AccountMeta::new_readonly(wsol_mint, false),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data: vec![1], // CreateIdempotent
    };

    let mut instructions = vec![
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(1_000),
        create_ata_ix,
        borrow_ix,
    ];
    instructions.extend(buy_ixs);
    instructions.extend(sell_ixs);
    instructions.push(repay_ix); // MUST be last

    // 6. Merge ALTs
    let mut all_alts = buy_alts;
    all_alts.extend(sell_alts);
    all_alts.sort_by_key(|a| a.key);
    all_alts.dedup_by_key(|a| a.key);

    // 7. Get blockhash
    let bh: serde_json::Value = client.post("https://solana-rpc.publicnode.com")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getLatestBlockhash","params":[{"commitment":"confirmed"}]}))
        .send()?.json()?;
    let blockhash: Hash = bh.get("result").and_then(|r| r.get("value")).and_then(|v| v.get("blockhash"))
        .and_then(|b| b.as_str()).ok_or(anyhow!("no blockhash"))?.parse()?;

    // 8. Build V0 TX with ALTs
    let message = v0::Message::try_compile(&kp.pubkey(), &instructions, &all_alts, blockhash)
        .map_err(|e| anyhow!("compile: {}", e))?;
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(message), &[&kp])
        .map_err(|e| anyhow!("sign: {}", e))?;
    let tx_bytes = bincode::serialize(&tx)?;

    println!("⚡ Flash TX: {} bytes ({} IXs, {} ALTs)", tx_bytes.len(), instructions.len(), all_alts.len());

    if tx_bytes.len() > 1232 {
        println!("❌ TX too large: {} > 1232", tx_bytes.len());
        return Ok(false);
    }

    // 9. Send via RPC
    let tx_b64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);
    let resp: serde_json::Value = client.post("https://solana-rpc.publicnode.com")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"sendTransaction",
            "params":[tx_b64,{"encoding":"base64","skipPreflight":true}]}))
        .send()?.json()?;

    if let Some(sig) = resp.get("result").and_then(|v| v.as_str()) {
        println!("✅ FLASH ARB SENT: {} | profit: {} lam", sig, profit);
        Ok(true)
    } else {
        println!("❌ Send error: {}", resp);
        Ok(false)
    }
}

fn jup_quote(client: &reqwest::blocking::Client, api_key: &str, input: &str, output: &str, amount: &str) -> Result<serde_json::Value> {
    // 10000 bps = 100% slippage tolerance. Flash loan repay is the REAL safety net.
    // Jupiter won't reject for slippage; if arb unprofitable → repay fails → atomic revert.
    let url = format!("https://api.jup.ag/swap/v1/quote?inputMint={}&outputMint={}&amount={}&slippageBps=10000&onlyDirectRoutes=true", input, output, amount);
    let resp: serde_json::Value = client.get(&url).header("x-api-key", api_key).send()?.json()?;
    if resp.get("outAmount").is_none() { return Err(anyhow!("no quote")); }
    Ok(resp)
}

fn jup_swap_tx(client: &reqwest::blocking::Client, api_key: &str, quote: &serde_json::Value, taker: &str) -> Result<String> {
    let resp: serde_json::Value = client.post("https://api.jup.ag/swap/v1/swap")
        .header("Content-Type", "application/json").header("x-api-key", api_key)
        .json(&serde_json::json!({"quoteResponse": quote, "userPublicKey": taker, "wrapAndUnwrapSol": false, "useSharedAccounts": true}))
        .send()?.json()?;
    resp.get("swapTransaction").and_then(|v| v.as_str()).map(String::from)
        .ok_or(anyhow!("no swapTransaction"))
}

fn extract_ixs_and_alts(tx_b64: &str) -> Result<(Vec<Instruction>, Vec<AddressLookupTableAccount>)> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(tx_b64)?;
    let tx: VersionedTransaction = bincode::deserialize(&bytes)?;

    let static_keys = match &tx.message {
        VersionedMessage::Legacy(m) => m.account_keys.clone(),
        VersionedMessage::V0(m) => m.account_keys.clone(),
    };
    let n_static = static_keys.len();

    // Resolve ALTs
    let mut all_keys = static_keys.clone();
    let mut alts = Vec::new();
    if let VersionedMessage::V0(m) = &tx.message {
        for alt_ref in &m.address_table_lookups {
            if let Ok(alt_accounts) = resolve_alt(&alt_ref.account_key) {
                for &idx in &alt_ref.writable_indexes {
                    if (idx as usize) < alt_accounts.len() { all_keys.push(alt_accounts[idx as usize]); }
                }
                for &idx in &alt_ref.readonly_indexes {
                    if (idx as usize) < alt_accounts.len() { all_keys.push(alt_accounts[idx as usize]); }
                }
                alts.push(AddressLookupTableAccount { key: alt_ref.account_key, addresses: alt_accounts });
            }
        }
    }

    let compiled_ixs = match &tx.message {
        VersionedMessage::Legacy(m) => &m.instructions,
        VersionedMessage::V0(m) => &m.instructions,
    };

    let compute_budget: Pubkey = "ComputeBudget111111111111111111111111111111".parse().unwrap();
    let ata_program: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse().unwrap();
    let spl_token: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse().unwrap();
    let system_program = solana_sdk::system_program::id();

    let mut swap_ixs = Vec::new();
    for cix in compiled_ixs {
        if (cix.program_id_index as usize) >= all_keys.len() { continue; }
        let program_id = all_keys[cix.program_id_index as usize];
        if program_id == compute_budget || program_id == ata_program { continue; }
        if program_id == system_program && cix.data.is_empty() { continue; }
        if program_id == spl_token && cix.data.len() == 1 && (cix.data[0] == 9 || cix.data[0] == 17) { continue; }

        let has_bad_ref = cix.accounts.iter().any(|&idx| (idx as usize) >= all_keys.len());
        if has_bad_ref { continue; }

        let accounts: Vec<AccountMeta> = cix.accounts.iter().map(|&idx| {
            let pubkey = all_keys[idx as usize];
            let is_signer = if (idx as usize) < n_static { tx.message.is_signer(idx as usize) } else { false };
            let is_writable = if (idx as usize) < n_static { tx.message.is_maybe_writable(idx as usize) } else { true };
            if is_writable { AccountMeta::new(pubkey, is_signer) } else { AccountMeta::new_readonly(pubkey, is_signer) }
        }).collect();

        swap_ixs.push(Instruction { program_id, accounts, data: cix.data.clone() });
    }

    Ok((swap_ixs, alts))
}

fn resolve_alt(addr: &Pubkey) -> Result<Vec<Pubkey>> {
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(3)).build()?;
    let resp: serde_json::Value = client.post("https://solana-rpc.publicnode.com")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getAccountInfo","params":[addr.to_string(),{"encoding":"base64"}]}))
        .send()?.json()?;
    let data_b64 = resp.get("result").and_then(|r| r.get("value")).and_then(|v| v.get("data"))
        .and_then(|d| d.as_array()).and_then(|a| a.first()).and_then(|v| v.as_str()).unwrap_or("");
    let data = base64::engine::general_purpose::STANDARD.decode(data_b64)?;
    if data.len() < 56 { return Ok(vec![]); }
    Ok(data[56..].chunks_exact(32).filter_map(|c| Pubkey::try_from(c).ok()).collect())
}
