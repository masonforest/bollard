//! Chain adapters. Each one answers one question: has this payment reached a
//! given address, and how deep is it. Amounts are in each asset's atom.

pub mod btc;
pub mod evm;

use anyhow::{anyhow, Result};
use serde::Serialize;

pub const GRACE_BLOCKS: u32 = 2;

#[derive(Debug, Clone, Serialize)]
pub struct AssetInfo {
    pub id: &'static str,
    pub chain: &'static str,
    pub symbol: &'static str,
    /// Human unit per atom: 1 atom = 10^-decimals units (BTC: 8, ETH in gwei: 9)
    pub decimals: u32,
    pub min_quantity: u64,
    pub contract: Option<&'static str>,
    /// Wei (or smallest chain unit) per atom. 1 for BTC (sat) and USDC; 1e9 for ETH (gwei).
    pub atom_scale: u128,
}

pub const ASSETS: &[AssetInfo] = &[
    AssetInfo { id: "BLRD", chain: "blrd", symbol: "BLRD", decimals: 8, min_quantity: 1, contract: None, atom_scale: 1 },
    AssetInfo { id: "btc:BTC", chain: "btc", symbol: "BTC", decimals: 8, min_quantity: 10_000, contract: None, atom_scale: 1 },
    AssetInfo { id: "eth:ETH", chain: "eth", symbol: "ETH", decimals: 9, min_quantity: 1_000_000, contract: None, atom_scale: 1_000_000_000 },
    AssetInfo { id: "eth:USDC", chain: "eth", symbol: "USDC", decimals: 6, min_quantity: 1_000_000, contract: Some("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"), atom_scale: 1 },
];

pub fn asset(id: &str) -> Option<&'static AssetInfo> {
    ASSETS.iter().find(|a| a.id == id)
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ChainParams {
    pub id: &'static str,
    /// Blocks (Bitcoin clock) from the claim for the taker's payment to be in a block.
    pub taker_deadline: u32,
    /// Blocks (Bitcoin clock) from the taker's inclusion for the maker's payment to be deep.
    pub maker_deadline: u32,
    /// Confirmations for "deep"; 0 means the chain's own finality tag.
    pub depth: u64,
}

pub const CHAINS: &[ChainParams] = &[
    ChainParams { id: "blrd", taker_deadline: 0, maker_deadline: 0, depth: 0 },
    ChainParams { id: "btc", taker_deadline: 3, maker_deadline: 6, depth: 3 },
    ChainParams { id: "eth", taker_deadline: 1, maker_deadline: 3, depth: 0 },
];

pub fn chain_params(id: &str) -> ChainParams {
    CHAINS.iter().copied().find(|c| c.id == id).unwrap_or(CHAINS[1])
}

/// Split "base/quote" into two known, distinct assets.
pub fn parse_market(m: &str) -> Result<(&'static AssetInfo, &'static AssetInfo)> {
    let (b, q) = m.split_once('/').ok_or_else(|| anyhow!("market must be base/quote"))?;
    let base = asset(b).ok_or_else(|| anyhow!("unknown asset {b}"))?;
    let quote = asset(q).ok_or_else(|| anyhow!("unknown asset {q}"))?;
    if base.id == quote.id {
        return Err(anyhow!("base and quote must differ"));
    }
    Ok((base, quote))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum PaymentStatus {
    /// Not in the mempool and not in a block.
    Unseen,
    /// In the mempool.
    Seen,
    /// In a block. `confirmations` counts the block itself as 1.
    Included { block_hash: String, block_height: u64, confirmations: u64 },
    /// Was in a block that is no longer on the chain.
    Reorged,
    /// Found but does not pay the expected address and amount.
    Wrong(String),
}

#[derive(Clone)]
pub struct Chains {
    pub btc: btc::BtcRpc,
    pub eth: evm::EvmRpc,
}

impl Chains {
    pub fn new(btc_url: &str, eth_url: &str) -> Self {
        Self { btc: btc::BtcRpc::new(btc_url), eth: evm::EvmRpc::new(eth_url) }
    }

    /// Where does the payment `txid` of at least `amount` atoms of `asset` to `to` stand?
    pub async fn check_payment(&self, asset: &AssetInfo, txid: &str, to: &str, amount: u64) -> Result<PaymentStatus> {
        match asset.chain {
            "btc" => self.btc.check_payment(txid, to, amount).await,
            "eth" => self.eth.check_payment(asset, txid, to, amount).await,
            other => Err(anyhow!("no adapter for chain {other}")),
        }
    }

    /// Is a payment included at (block_hash, block_height) deep by that chain's rule?
    pub async fn is_deep(&self, chain: &str, txid: &str, block_hash: &str, block_height: u64) -> Result<bool> {
        let p = chain_params(chain);
        match chain {
            "btc" => self.btc.is_deep(txid, block_hash, p.depth).await,
            "eth" => self.eth.is_deep(txid, block_hash, block_height).await,
            other => Err(anyhow!("no adapter for chain {other}")),
        }
    }
}
