/// Carbium Flash: circular arb with Solend flash loan = 0 capital needed
/// Carbium finds SOL→X→SOL route, we wrap with flash borrow/repay
/// 1 atomic TX: fail = only ~5K fee, success = pure profit

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use solana_sdk::{
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    sysvar,
    transaction::VersionedTransaction,
    address_lookup_table::AddressLookupTableAccount,
};

const CARBIUM_API: &str = "https://api.carbium.io/api/v2/quote";
const CARBIUM_RPC: &str = "https://rpc-service.carbium.io/?apiKey=578475ff-1380";
const SOLEND: &str = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";

fn main() -> Result<()> {
    let api_key = std::env::var("CARBIUM_API_KEY").unwrap_or("814f09dc-d645".into());
    let wb: Vec<u8> = serde_json::from_str(&std::fs::read_to_string("/root/solana-bot/wallet.json")?)?;
    let kp = Keypair::from_bytes(&wb).map_err(|e| anyhow!("{}", e))?;
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build()?;
    let continuous = std::env::var("CONTINUOUS").map(|v| v == "true").unwrap_or(false);
    let min_profit: i64 = std::env::var("MIN_PROFIT").and_then(|v| Ok(v.parse().unwrap_or(8000))).unwrap_or(8000);
    let taker = kp.pubkey().to_string();
    let amount = "200000000"; // 0.2 SOL (keeps TX < 1232 with flash loan)

    println!("Carbium Flash Arb | wallet: {} | min_profit: {}", taker, min_profit);

    loop {
        match run_carbium_flash(&client, &api_key, &kp, amount, &taker, min_profit) {
            Ok(true) => println!("🎉 Flash arb executed!"),
            Ok(false) => {}
            Err(e) => {
                let msg = format!("{}", e);
                if msg.contains("Rate limit") { std::thread::sleep(std::time::Duration::from_secs(5)); }
                else if !msg.contains("Not profitable") { println!("⚠️ {}", msg); }
            }
        }
        if !continuous { break; }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    Ok(())
}

fn run_carbium_flash(client: &reqwest::blocking::Client, api_key: &str, kp: &Keypair,
    amount: &str, taker: &str, min_profit: i64) -> Result<bool> {

    // 1. Get Carbium circular quote + TX
    let url = format!(
        "{}?src_mint=So11111111111111111111111111111111111111112&dst_mint=So11111111111111111111111111111111111111112&amount_in={}&slippage_bps=500&user_account={}",
        CARBIUM_API, amount, taker
    );
    let resp: serde_json::Value = client.get(&url).header("X-API-KEY", api_key).send()?.json()?;

    if let Some(e) = resp.get("error").and_then(|v| v.as_str()) {
        return Err(anyhow!("{}", e));
    }

    let in_amt: i64 = resp.get("srcAmountIn").and_then(|v| v.as_str()).unwrap_or("0").parse()?;
    let out_amt: i64 = resp.get("destAmountOut").and_then(|v| v.as_str()).unwrap_or("0").parse()?;
    let profit = out_amt - in_amt;

    if profit < min_profit {
        return Err(anyhow!("Not profitable: {} < {}", profit, min_profit));
    }

    let txn_b64 = resp.get("txn").and_then(|v| v.as_str()).ok_or(anyhow!("no txn"))?;
    println!("✅ Carbium arb: +{} lam @ {} SOL", profit, in_amt as f64 / 1e9);

    // 2. Extract swap IXs + ALTs from Carbium TX
    let (swap_ixs, alts) = extract_ixs_and_alts(txn_b64)?;
    println!("   Extracted {} swap IXs, {} ALTs", swap_ixs.len(), alts.len());

    // 3. Build Solend flash borrow + repay
    let solend: Pubkey = SOLEND.parse()?;
    let token_program: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse()?;
    let borrow_amount: u64 = amount.parse()?;

    // Solend Turbo Pool
    let reserve: Pubkey = "UTABCRXirrbpCNDogCoqEECtM3V44jXGCsK23ZepV3Z".parse()?;
    let liquidity: Pubkey = "5cSfC32xBUYqGfkURLGfANuK64naHmMp27jUT7LQSujY".parse()?;
    let fee_recv: Pubkey = "5wo1tFpi4HaVKnemqaXeQnBEpezrJXcXvuztYaPhvgC7".parse()?;
    let market: Pubkey = "7RCz8wb6WXxUhAigok9ttgrVgDFFFbibcirECzWSBauM".parse()?;
    let (market_auth, _) = Pubkey::find_program_address(&[market.as_ref()], &solend);
    let wsol: Pubkey = "So11111111111111111111111111111111111111112".parse()?;
    let ata_prog: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse()?;
    let user_ata = Pubkey::find_program_address(
        &[kp.pubkey().as_ref(), token_program.as_ref(), wsol.as_ref()], &ata_prog).0;

    let mut borrow_data = vec![19u8];
    borrow_data.extend_from_slice(&borrow_amount.to_le_bytes());

    let borrow_ix = Instruction {
        program_id: solend,
        accounts: vec![
            AccountMeta::new(liquidity, false),
            AccountMeta::new(user_ata, false),
            AccountMeta::new(reserve, false),
            AccountMeta::new_readonly(market, false),
            AccountMeta::new_readonly(market_auth, false),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data: borrow_data,
    };

    let mut repay_data = vec![20u8];
    repay_data.extend_from_slice(&borrow_amount.to_le_bytes());
    repay_data.push(3u8); // borrow IX index (after 2 compute + 1 create_ata)

    let repay_ix = Instruction {
        program_id: solend,
        accounts: vec![
            AccountMeta::new(user_ata, false),
            AccountMeta::new(liquidity, false),
            AccountMeta::new(fee_recv, false),
            AccountMeta::new_readonly(Pubkey::default(), false),
            AccountMeta::new(reserve, false),
            AccountMeta::new_readonly(market, false),
            AccountMeta::new(kp.pubkey(), true),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data: repay_data,
    };

    // 4. Assemble: [compute, compute, create_ata, borrow, carbium_swaps..., repay]
    let create_ata = Instruction {
        program_id: ata_prog,
        accounts: vec![
            AccountMeta::new(kp.pubkey(), true),
            AccountMeta::new(user_ata, false),
            AccountMeta::new_readonly(kp.pubkey(), false),
            AccountMeta::new_readonly(wsol, false),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data: vec![1],
    };

    let mut instructions = vec![
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(1_000),
        create_ata,
        borrow_ix,
    ];
    instructions.extend(swap_ixs);
    instructions.push(repay_ix);

    // 5. Get blockhash + build V0 TX
    let bh: serde_json::Value = client.post(CARBIUM_RPC)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getLatestBlockhash","params":[{"commitment":"confirmed"}]}))
        .send()?.json()?;
    let blockhash: Hash = bh.get("result").and_then(|r| r.get("value")).and_then(|v| v.get("blockhash"))
        .and_then(|b| b.as_str()).ok_or(anyhow!("no blockhash"))?.parse()?;

    let message = v0::Message::try_compile(&kp.pubkey(), &instructions, &alts, blockhash)
        .map_err(|e| anyhow!("compile: {}", e))?;
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(message), &[&kp])
        .map_err(|e| anyhow!("sign: {}", e))?;
    let tx_bytes = bincode::serialize(&tx)?;

    println!("   Flash TX: {} bytes ({} IXs)", tx_bytes.len(), instructions.len());
    if tx_bytes.len() > 1232 {
        // Fallback: send Carbium TX directly (no flash loan)
        println!("   TX too large for flash loan, sending Carbium TX directly");
        return send_carbium_direct(client, txn_b64, kp, profit);
    }

    // 6. Send
    let tx_b64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);
    let resp: serde_json::Value = client.post(CARBIUM_RPC)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"sendTransaction",
            "params":[tx_b64,{"encoding":"base64","skipPreflight":true}]}))
        .send()?.json()?;

    if let Some(sig) = resp.get("result").and_then(|v| v.as_str()) {
        println!("🎉 FLASH ARB SENT: {} | profit: {}", sig, profit);
        Ok(true)
    } else {
        // Fallback to direct Carbium TX
        println!("   RPC failed, sending Carbium TX directly");
        send_carbium_direct(client, txn_b64, kp, profit)
    }
}

fn send_carbium_direct(client: &reqwest::blocking::Client, txn_b64: &str, kp: &Keypair, profit: i64) -> Result<bool> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(txn_b64)?;
    let mut tx: VersionedTransaction = bincode::deserialize(&bytes)?;
    tx.signatures[0] = kp.sign_message(&tx.message.serialize());
    let signed = base64::engine::general_purpose::STANDARD.encode(&bincode::serialize(&tx)?);
    let resp: serde_json::Value = client.post(CARBIUM_RPC)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"sendTransaction",
            "params":[signed,{"encoding":"base64","skipPreflight":true}]}))
        .send()?.json()?;
    if let Some(sig) = resp.get("result").and_then(|v| v.as_str()) {
        println!("✅ DIRECT TX SENT: {} | profit: {}", sig, profit);
        Ok(true)
    } else {
        Ok(false)
    }
}

fn extract_ixs_and_alts(tx_b64: &str) -> Result<(Vec<Instruction>, Vec<AddressLookupTableAccount>)> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(tx_b64)?;
    let tx: VersionedTransaction = bincode::deserialize(&bytes)?;

    let static_keys = match &tx.message {
        VersionedMessage::Legacy(m) => m.account_keys.clone(),
        VersionedMessage::V0(m) => m.account_keys.clone(),
    };
    let n_static = static_keys.len();
    let mut all_keys = static_keys.clone();
    let mut alts = Vec::new();

    if let VersionedMessage::V0(m) = &tx.message {
        for alt_ref in &m.address_table_lookups {
            if let Ok(addrs) = resolve_alt(&alt_ref.account_key) {
                for &idx in &alt_ref.writable_indexes {
                    if (idx as usize) < addrs.len() { all_keys.push(addrs[idx as usize]); }
                }
                for &idx in &alt_ref.readonly_indexes {
                    if (idx as usize) < addrs.len() { all_keys.push(addrs[idx as usize]); }
                }
                alts.push(AddressLookupTableAccount { key: alt_ref.account_key, addresses: addrs });
            }
        }
    }

    let compiled = match &tx.message {
        VersionedMessage::Legacy(m) => &m.instructions,
        VersionedMessage::V0(m) => &m.instructions,
    };

    let compute: Pubkey = "ComputeBudget111111111111111111111111111111".parse().unwrap();
    let ata: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse().unwrap();
    let sys = solana_sdk::system_program::id();
    let tok: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse().unwrap();

    let mut ixs = Vec::new();
    for cix in compiled {
        if (cix.program_id_index as usize) >= all_keys.len() { continue; }
        let pid = all_keys[cix.program_id_index as usize];
        if pid == compute || pid == ata { continue; }
        if pid == sys && cix.data.is_empty() { continue; }
        if pid == tok && cix.data.len() == 1 && (cix.data[0] == 9 || cix.data[0] == 17) { continue; }
        if cix.accounts.iter().any(|&i| (i as usize) >= all_keys.len()) { continue; }

        let accounts: Vec<AccountMeta> = cix.accounts.iter().map(|&idx| {
            let pk = all_keys[idx as usize];
            let signer = if (idx as usize) < n_static { tx.message.is_signer(idx as usize) } else { false };
            let writable = if (idx as usize) < n_static { tx.message.is_maybe_writable(idx as usize) } else { true };
            if writable { AccountMeta::new(pk, signer) } else { AccountMeta::new_readonly(pk, signer) }
        }).collect();

        ixs.push(Instruction { program_id: pid, accounts, data: cix.data.clone() });
    }
    Ok((ixs, alts))
}

fn resolve_alt(addr: &Pubkey) -> Result<Vec<Pubkey>> {
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(3)).build()?;
    let resp: serde_json::Value = client.post(CARBIUM_RPC)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getAccountInfo","params":[addr.to_string(),{"encoding":"base64"}]}))
        .send()?.json()?;
    let b64 = resp.get("result").and_then(|r| r.get("value")).and_then(|v| v.get("data"))
        .and_then(|d| d.as_array()).and_then(|a| a.first()).and_then(|v| v.as_str()).unwrap_or("");
    let data = base64::engine::general_purpose::STANDARD.decode(b64)?;
    if data.len() < 56 { return Ok(vec![]); }
    Ok(data[56..].chunks_exact(32).filter_map(|c| Pubkey::try_from(c).ok()).collect())
}
