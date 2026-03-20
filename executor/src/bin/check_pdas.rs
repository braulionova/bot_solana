use solana_sdk::pubkey::Pubkey;
fn main() {
    let pool: Pubkey = "4KdvXdxACugkGEkk2HHso1nrmub5fAmZtXah7z8T3eee".parse().unwrap();
    let wsol: Pubkey = "So11111111111111111111111111111111111111112".parse().unwrap();
    let token: Pubkey = "ETkVQcnPLnohfRWuuJDhVguXBU957zRF88YrU6eNWYxs".parse().unwrap();
    let spl_token: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse().unwrap();
    let assoc: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse().unwrap();

    let wsol_vault = Pubkey::find_program_address(
        &[pool.as_ref(), spl_token.as_ref(), wsol.as_ref()],
        &assoc,
    ).0;
    let token_vault = Pubkey::find_program_address(
        &[pool.as_ref(), spl_token.as_ref(), token.as_ref()],
        &assoc,
    ).0;

    println!("Derived WSOL vault (base):  {wsol_vault}");
    println!("Derived token vault (quote): {token_vault}");
    println!();
    println!("On-chain pool_base_vault:    H5EHKBuWxKLZxYu4m1SgHURaYgjH6wR59hVYb2y8b6e5");
    println!("On-chain pool_quote_vault:   F9hGXNWyqjQZpWQTU3sGWv9MUGFXXfXQoDykb4oZjEus");
    println!();

    let on_chain_base: Pubkey = "H5EHKBuWxKLZxYu4m1SgHURaYgjH6wR59hVYb2y8b6e5".parse().unwrap();
    let on_chain_quote: Pubkey = "F9hGXNWyqjQZpWQTU3sGWv9MUGFXXfXQoDykb4oZjEus".parse().unwrap();
    println!("base vault matches:  {}", wsol_vault == on_chain_base);
    println!("quote vault matches: {}", token_vault == on_chain_quote);
}
