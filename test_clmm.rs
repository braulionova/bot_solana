fn main() {
    let sqrt_price: u128 = 2999450371457805177893; // example Q64.64
    let price = (sqrt_price as f64 / (1u128 << 64) as f64).powi(2);
    println!("Price: {}", price);
}
