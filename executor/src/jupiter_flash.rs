// jupiter_flash.rs — Flash loan arb using Jupiter Swap API for routing.
//
// Jupiter finds the optimal cross-DEX route (e.g., buy via Raydium, sell via GoonFi).
// We wrap it with MarginFi flash loan: borrow → Jupiter swap → repay.
// The entire round-trip is atomic in a single TX via Jito bundle.
//
// Flow:
// 1. Jupiter scanner detects profitable cross-DEX spread
// 2. Get Jupiter swap TX for the arb route (SOL → token → SOL)
// 3. Extract swap instructions from Jupiter TX
// 4. Wrap with MarginFi flash loan (borrow SOL → swap → repay)
// 5. Sign and send via Jito bundle (atomic: fail = 0 cost)

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use solana_sdk::{
    instruction::Instruction,
    message::VersionedMessage,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use tracing::{debug, info, warn};

const WSOL: &str = "So11111111111111111111111111111111111111112";

/// Get Jupiter swap instructions for a round-trip arb (SOL → token → SOL).
/// Returns the extracted instructions that can be wrapped with flash loan.
pub async fn get_jupiter_arb_instructions(
    client: &reqwest::Client,
    api_key: &str,
    token_mint: &str,
    amount_lamports: u64,
    payer: &Pubkey,
) -> Result<Vec<Instruction>> {
    // Step 1: Get buy quote (SOL → token)
    let buy_quote = get_quote(client, api_key, WSOL, token_mint, amount_lamports).await?;
    let tokens_out: u64 = buy_quote.get("outAmount")
        .and_then(|v| v.as_str())
        .unwrap_or("0")
        .parse()
        .context("parse buy outAmount")?;
    if tokens_out == 0 { return Err(anyhow!("buy quote: 0 tokens out")); }

    // Step 2: Get sell quote (token → SOL)
    let sell_quote = get_quote(client, api_key, token_mint, WSOL, tokens_out).await?;
    let sol_back: u64 = sell_quote.get("outAmount")
        .and_then(|v| v.as_str())
        .unwrap_or("0")
        .parse()
        .context("parse sell outAmount")?;

    let profit = sol_back as i64 - amount_lamports as i64;
    if profit <= 0 {
        return Err(anyhow!("unprofitable: {} lamports", profit));
    }

    info!(
        token = token_mint,
        amount = amount_lamports,
        tokens_out,
        sol_back,
        profit,
        "Jupiter arb route profitable"
    );

    // Step 3: Get buy swap TX
    let buy_tx_data = get_swap_tx(client, api_key, &buy_quote, &payer.to_string()).await?;
    let buy_ixs = extract_swap_instructions(&buy_tx_data)?;

    // Step 4: Get sell swap TX
    let sell_tx_data = get_swap_tx(client, api_key, &sell_quote, &payer.to_string()).await?;
    let sell_ixs = extract_swap_instructions(&sell_tx_data)?;

    // Combine: buy instructions + sell instructions
    let mut all_ixs = buy_ixs;
    all_ixs.extend(sell_ixs);

    info!(
        total_instructions = all_ixs.len(),
        profit,
        "Jupiter arb instructions extracted"
    );

    Ok(all_ixs)
}

async fn get_quote(
    client: &reqwest::Client,
    api_key: &str,
    input_mint: &str,
    output_mint: &str,
    amount: u64,
) -> Result<serde_json::Value> {
    let url = format!(
        "https://api.jup.ag/swap/v1/quote?inputMint={}&outputMint={}&amount={}&slippageBps=100",
        input_mint, output_mint, amount
    );
    let resp = client.get(&url)
        .header("x-api-key", api_key)
        .send().await
        .context("quote request")?;
    let body: serde_json::Value = resp.json().await.context("quote json")?;
    if body.get("error").is_some() {
        return Err(anyhow!("quote error: {}", body));
    }
    Ok(body)
}

async fn get_swap_tx(
    client: &reqwest::Client,
    api_key: &str,
    quote: &serde_json::Value,
    user_pubkey: &str,
) -> Result<String> {
    let resp = client.post("https://api.jup.ag/swap/v1/swap")
        .header("Content-Type", "application/json")
        .header("x-api-key", api_key)
        .json(&serde_json::json!({
            "quoteResponse": quote,
            "userPublicKey": user_pubkey,
            "wrapAndUnwrapSol": true,
            "useSharedAccounts": true,
        }))
        .send().await
        .context("swap request")?;
    let body: serde_json::Value = resp.json().await.context("swap json")?;
    body.get("swapTransaction")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("no swapTransaction in response: {}", body))
}

/// Extract only the swap instructions from a Jupiter TX (skip compute budget, ATA setup).
fn extract_swap_instructions(tx_b64: &str) -> Result<Vec<Instruction>> {
    let tx_bytes = base64::engine::general_purpose::STANDARD.decode(tx_b64)
        .context("decode tx")?;
    let tx: VersionedTransaction = bincode::deserialize(&tx_bytes)
        .context("deserialize tx")?;

    // For V0 messages, we need static keys + ALT-resolved keys.
    // Since we can't resolve ALTs without RPC, we use static_account_keys only
    // and skip instructions that reference ALT indices.
    let account_keys: Vec<Pubkey> = match &tx.message {
        VersionedMessage::Legacy(m) => m.account_keys.clone(),
        VersionedMessage::V0(m) => m.account_keys.clone(),
    };
    let n_static_keys = account_keys.len();

    let compiled_ixs = match &tx.message {
        VersionedMessage::Legacy(m) => &m.instructions,
        VersionedMessage::V0(m) => &m.instructions,
    };

    let compute_budget: Pubkey = "ComputeBudget111111111111111111111111111111".parse().unwrap();
    let system_program = solana_sdk::system_program::id();
    let ata_program: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse().unwrap();
    let spl_token: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse().unwrap();

    let mut swap_ixs = Vec::new();
    for cix in compiled_ixs {
        let program_id = account_keys[cix.program_id_index as usize];
        // Skip: compute budget, system transfers, ATA creation, token close
        if program_id == compute_budget { continue; }
        if program_id == ata_program { continue; }
        // Keep system_program for syncNative (WSOL wrapping) and token program for transfers
        // Skip pure system transfers (not syncNative)
        if program_id == system_program && cix.data.is_empty() { continue; }

        // Skip instructions that reference ALT accounts (idx >= n_static_keys)
        let has_alt_ref = cix.accounts.iter().any(|&idx| (idx as usize) >= n_static_keys);
        if has_alt_ref {
            // Can't resolve ALT accounts without RPC — skip this instruction
            continue;
        }

        let accounts: Vec<solana_sdk::instruction::AccountMeta> = cix.accounts.iter().map(|&idx| {
            let pubkey = account_keys[idx as usize];
            let is_signer = tx.message.is_signer(idx as usize);
            let is_writable = tx.message.is_maybe_writable(idx as usize);
            if is_writable {
                solana_sdk::instruction::AccountMeta::new(pubkey, is_signer)
            } else {
                solana_sdk::instruction::AccountMeta::new_readonly(pubkey, is_signer)
            }
        }).collect();

        swap_ixs.push(Instruction {
            program_id,
            accounts,
            data: cix.data.clone(),
        });
    }

    Ok(swap_ixs)
}
