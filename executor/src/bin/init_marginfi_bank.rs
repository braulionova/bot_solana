/// Initialize the SOL bank in our MarginFi lending account.
use anyhow::Result;
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
    transaction::Transaction,
};

const MARGINFI_PROGRAM: &str = "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA";

fn associated_token_address(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    let token_program: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse().unwrap();
    let ata_program: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse().unwrap();
    Pubkey::find_program_address(
        &[wallet.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ata_program,
    ).0
}

fn main() -> Result<()> {
    let payer = read_keypair_file(
        std::env::var("PAYER_KEYPAIR").unwrap_or("/root/solana-bot/wallet.json".into())
    ).map_err(|e| anyhow::anyhow!("{}", e))?;
    let rpc = RpcClient::new_with_commitment(
        String::from("https://solana-rpc.publicnode.com"), CommitmentConfig::confirmed(),
    );

    let marginfi_program: Pubkey = MARGINFI_PROGRAM.parse()?;
    let marginfi_group: Pubkey = "4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8".parse()?;
    let marginfi_account: Pubkey = "Gj3hRZsQCEboWngk64eFGd3ja4JBoAUMYEiQKSFoAEWY".parse()?;
    let sol_bank: Pubkey = "CCKtUs6Cgwo4aaQUmBPmyoApH2gUDErxNZCAntD6LYGh".parse()?;
    let sol_liquidity_vault: Pubkey = "Bn2QApcgspFfao1FG9UMUMLtqMy5dLCYgGBDfRfRHxSF".parse()?;
    let token_program: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse()?;
    let wsol: Pubkey = "So11111111111111111111111111111111111111112".parse()?;

    let user_wsol_ata = associated_token_address(&payer.pubkey(), &wsol);
    println!("WSOL ATA: {}", user_wsol_ata);

    // Create WSOL ATA idempotent
    let ata_program: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse()?;
    let create_ata = Instruction {
        program_id: ata_program,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new(user_wsol_ata, false),
            AccountMeta::new_readonly(payer.pubkey(), false),
            AccountMeta::new_readonly(wsol, false),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data: vec![1], // CreateIdempotent
    };

    // lending_account_deposit(amount=0) to init SOL bank slot
    let mut data = vec![171, 94, 235, 103, 82, 64, 212, 140]; // sha256("global:lending_account_deposit")[..8]
    data.extend_from_slice(&1000000u64.to_le_bytes()); // amount = 0.001 SOL (1M lamports)
    data.push(0u8); // extra byte (padding/flag, matches on-chain TX)

    // Correct account layout from on-chain TX analysis (14 accounts):
    // [0] marginfi_group, [1] marginfi_account, [2] signer, [3] bank (SOL)
    // [4] signer_token_account, [5] bank_liquidity_vault_authority,
    // [6] bank_liquidity_vault, [7] token_program
    // [8..] remaining_accounts: bank/oracle pairs for health check

    let sol_oracle: Pubkey = "4Hmd6PdjVA9auCoScE12iaBogfwS4ZXQ6VZoBeqanwWW".parse()?;
    let (bank_liq_auth, _) = Pubkey::find_program_address(
        &[b"liquidity_vault_auth", sol_bank.as_ref()], &marginfi_program,
    );

    // Exact layout from successful on-chain deposit TX (7 accounts, no vault_auth):
    let deposit_ix = Instruction {
        program_id: marginfi_program,
        accounts: vec![
            AccountMeta::new(marginfi_group, false),          // 0: group
            AccountMeta::new(marginfi_account, false),        // 1: marginfi_account
            AccountMeta::new(payer.pubkey(), true),            // 2: signer
            AccountMeta::new(sol_bank, false),                 // 3: bank
            AccountMeta::new(user_wsol_ata, false),            // 4: signer_token_account
            AccountMeta::new(sol_liquidity_vault, false),      // 5: bank_liquidity_vault
            AccountMeta::new_readonly(token_program, false),   // 6: token_program
        ],
        data,
    };

    // Wrap 1000 lamports as WSOL (transfer SOL to ATA + syncNative)
    let wrap_transfer = solana_sdk::system_instruction::transfer(
        &payer.pubkey(), &user_wsol_ata, 1100000, // 0.0011 SOL (0.001 deposit + buffer for rent)
    );
    let sync_native = Instruction {
        program_id: token_program,
        accounts: vec![AccountMeta::new(user_wsol_ata, false)],
        data: vec![17], // SyncNative instruction
    };

    let blockhash = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[create_ata, wrap_transfer, sync_native, deposit_ix],
        Some(&payer.pubkey()),
        &[&payer],
        blockhash,
    );

    println!("Sending lending_account_deposit(0) to init SOL bank...");
    match rpc.send_and_confirm_transaction(&tx) {
        Ok(sig) => println!("✅ Success! sig={}", sig),
        Err(e) => println!("❌ Error: {}", e),
    }
    Ok(())
}
