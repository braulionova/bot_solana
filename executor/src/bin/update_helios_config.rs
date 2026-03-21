use anyhow::Result;
use borsh::BorshSerialize;
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
    transaction::Transaction,
};

const RPC_URL: &str = "https://api.mainnet-beta.solana.com";
const HELIOS_ARB_PROGRAM_ID: &str = "DMSsKkNyyxVviDGHJTVpGxmSnzgMiPFrJ2SNvmbjhm64";

#[derive(BorshSerialize)]
struct UpdateConfigArgs {
    new_authority: Option<Pubkey>,
    new_whitelist: Option<Vec<Pubkey>>,
}

fn main() -> Result<()> {
    let payer_path = std::env::var("PAYER_KEYPAIR")
        .unwrap_or_else(|_| "/root/solana-bot/wallet.json".to_string());
    let payer = read_keypair_file(&payer_path).map_err(|err| {
        anyhow::anyhow!("failed to read payer keypair from {}: {}", payer_path, err)
    })?;
    let rpc = RpcClient::new_with_commitment(RPC_URL.to_string(), CommitmentConfig::confirmed());

    let helios_program: Pubkey = HELIOS_ARB_PROGRAM_ID.parse().unwrap();
    let (config_pda, _) = Pubkey::find_program_address(&[b"config"], &helios_program);
    let whitelist = vec![
        "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8".parse::<Pubkey>()?, // Raydium V4
        "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK".parse::<Pubkey>()?, // Raydium CLMM
        "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc".parse::<Pubkey>()?,  // Orca Whirlpool
        "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo".parse::<Pubkey>()?,  // Meteora DLMM
        "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA".parse::<Pubkey>()?,  // PumpSwap
        "goonuddtQRrWqqn5nFyczVKaie28f3kDkHWkHtURSLE".parse::<Pubkey>()?,  // GoonFi V2
        "ZERor4xhbUycZ6gb9ntrhqscUcZmAbQDjEAtCf4hbZY".parse::<Pubkey>()?,  // ZeroFi
        "9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp".parse::<Pubkey>()?, // HumidiFi
        "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C".parse::<Pubkey>()?, // Raydium CPMM
        "Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB".parse::<Pubkey>()?, // Meteora Dynamic AMM
    ];

    // Check if config PDA exists; if not, initialize first
    let config_exists = rpc.get_account(&config_pda).is_ok();

    let ix = if !config_exists {
        println!("Config PDA does not exist, initializing...");
        // initialize discriminator: sha256("global:initialize")[..8]
        let data = vec![175, 175, 109, 31, 13, 152, 155, 237];
        Instruction {
            program_id: helios_program,
            accounts: vec![
                AccountMeta::new(config_pda, false),
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            ],
            data,
        }
    } else {
        println!("Config PDA exists, updating whitelist...");
        let mut data = vec![29, 158, 252, 191, 10, 83, 219, 99];
        UpdateConfigArgs {
            new_authority: None,
            new_whitelist: Some(whitelist.clone()),
        }
        .serialize(&mut data)?;
        Instruction {
            program_id: helios_program,
            accounts: vec![
                AccountMeta::new(config_pda, false),
                AccountMeta::new_readonly(payer.pubkey(), true),
            ],
            data,
        }
    };

    let recent_blockhash = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[&payer],
        recent_blockhash,
    );
    let sig = rpc.send_and_confirm_transaction(&tx)?;

    println!("config_pda={}", config_pda);
    println!("signature={}", sig);
    for (i, program) in whitelist.iter().enumerate() {
        println!("whitelist[{i}]={program}");
    }

    Ok(())
}
