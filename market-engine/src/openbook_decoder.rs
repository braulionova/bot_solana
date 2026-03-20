// openbook_decoder.rs — Decode OpenBook V1 (Serum) orderbook state for Raydium V4.
//
// Raydium AMM V4 integrates with OpenBook (formerly Serum). The orderbook
// affects the effective price of swaps. Reading the best bid/ask from the
// orderbook gives a more accurate price than just using reserves.
//
// This is a simplified decoder that reads the top-of-book bid/ask.

use solana_sdk::pubkey::Pubkey;

/// OpenBook V1 (Serum) program.
pub const OPENBOOK_V1: Pubkey = solana_sdk::pubkey!("srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX");

/// Decoded top-of-book from OpenBook orderbook.
#[derive(Debug, Clone, Default)]
pub struct TopOfBook {
    pub best_bid_price: u64,
    pub best_bid_size: u64,
    pub best_ask_price: u64,
    pub best_ask_size: u64,
}

/// Decode the best bid/ask from an OpenBook orderbook account.
/// The orderbook is a slab data structure. We read just the top entries.
///
/// Slab layout (simplified):
/// - Header: 32 bytes
/// - Nodes: array of 68-byte nodes (inner + leaf)
/// - Leaf node: tag=2, price at offset 16 (u64), quantity at offset 24 (u64)
pub fn decode_top_of_book(bids_data: &[u8], asks_data: &[u8]) -> TopOfBook {
    let mut tob = TopOfBook::default();

    // Parse bids (highest price = best bid)
    if let Some((price, size)) = find_best_leaf(bids_data, true) {
        tob.best_bid_price = price;
        tob.best_bid_size = size;
    }

    // Parse asks (lowest price = best ask)
    if let Some((price, size)) = find_best_leaf(asks_data, false) {
        tob.best_ask_price = price;
        tob.best_ask_size = size;
    }

    tob
}

/// Find the best (highest bid / lowest ask) leaf node in a slab.
fn find_best_leaf(data: &[u8], find_highest: bool) -> Option<(u64, u64)> {
    if data.len() < 100 { return None; }

    // Slab header is 32 bytes. Then nodes of 68 bytes each.
    // A leaf node has tag = 2 at offset 0.
    let header_size = 32;
    let node_size = 68;
    let node_count = (data.len() - header_size) / node_size;

    let mut best_price: Option<u64> = None;
    let mut best_size: u64 = 0;

    for i in 0..node_count.min(100) { // scan up to 100 nodes
        let offset = header_size + i * node_size;
        if offset + node_size > data.len() { break; }

        let tag = u32::from_le_bytes(data[offset..offset+4].try_into().ok()?);
        if tag != 2 { continue; } // Not a leaf node

        // Leaf node layout: [tag:4][padding:12][key:16][owner:32][quantity:8]
        // The key is 16 bytes: first 8 bytes = price, next 8 bytes = order_id
        let price = u64::from_le_bytes(data[offset+16..offset+24].try_into().ok()?);
        let quantity = u64::from_le_bytes(data[offset+52..offset+60].try_into().ok()?);

        if price == 0 || quantity == 0 { continue; }

        let is_better = match best_price {
            None => true,
            Some(bp) => if find_highest { price > bp } else { price < bp },
        };

        if is_better {
            best_price = Some(price);
            best_size = quantity;
        }
    }

    best_price.map(|p| (p, best_size))
}

/// Calculate effective price considering orderbook depth.
/// Returns price in quote_tokens per base_token (adjusted by lot sizes).
pub fn effective_price(
    tob: &TopOfBook,
    base_lot_size: u64,
    quote_lot_size: u64,
    is_buy: bool,
) -> Option<f64> {
    if base_lot_size == 0 || quote_lot_size == 0 { return None; }

    let price = if is_buy {
        tob.best_ask_price
    } else {
        tob.best_bid_price
    };

    if price == 0 { return None; }

    // Price = price_lots * quote_lot_size / base_lot_size
    Some(price as f64 * quote_lot_size as f64 / base_lot_size as f64)
}
