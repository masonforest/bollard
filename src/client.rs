//! A small client: make keys, build and sign transactions, submit them.

use crate::crypto::Keypair;
use crate::fbio::{self, TxBody};
use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use serde_json::{json, Value};

#[derive(Args, Debug)]
pub struct TxArgs {
    /// Account secret key, 32 bytes hex
    #[arg(long, env = "BOLLARD_KEY")]
    pub key: String,
    /// Node to submit to
    #[arg(long, env = "BOLLARD_NODE", default_value = "https://mainnet.bollard.exchange")]
    pub node: String,
    /// Use this nonce instead of asking the node
    #[arg(long)]
    pub nonce: Option<u64>,
    #[command(subcommand)]
    pub cmd: TxCmd,
}

#[derive(Subcommand, Debug)]
pub enum TxCmd {
    /// Send BLRD to another account
    Transfer {
        /// Recipient public key, 33 bytes hex
        #[arg(long)]
        to: String,
        /// Amount in rill (1 BLRD = 100,000,000 rill)
        #[arg(long)]
        amount: u64,
    },
    /// Rest an order
    PlaceOrder {
        /// e.g. btc:BTC/eth:ETH
        #[arg(long)]
        market: String,
        /// buy or sell (of the base asset)
        #[arg(long)]
        side: String,
        /// Base atoms (sats for BTC, gwei for ETH, 1e-6 for USDC, rill for BLRD)
        #[arg(long)]
        quantity: u64,
        /// Quote atoms per 100,000,000 base atoms
        #[arg(long)]
        price: u64,
        /// Total bond in rill locked for this order (ignored for BLRD sells)
        #[arg(long, default_value_t = 0)]
        bond: u64,
        /// Rill a claimant must lock
        #[arg(long, default_value_t = 0)]
        claim_bond: u64,
        /// seen, included or final: when you will deliver
        #[arg(long, default_value = "included")]
        fill_policy: String,
        /// Your address on the chain of the asset you are buying
        #[arg(long)]
        receive_address: String,
        /// Bitcoin height at which the order cancels itself
        #[arg(long)]
        expiry_height: u32,
        /// ledger (both sides settle instantly) or chain (the taker pays on chain)
        #[arg(long, default_value = "ledger")]
        settle: String,
        /// Mint on claim against your stake instead of escrowing options now (deposit orders only)
        #[arg(long, default_value_t = false)]
        mint: bool,
    },
    /// Issue options on an asset you will deliver on demand; your stake backs them
    Mint {
        #[arg(long)]
        asset: String,
        #[arg(long)]
        quantity: u64,
        /// The on-chain address the coins behind these options sit at; deliveries must come from it
        #[arg(long)]
        source: String,
    },
    /// Stake BLRD (rill), or another maker's options (e.g. --asset btc:BTC): one pool that backs every option you issue
    Stake {
        #[arg(long)]
        amount: u64,
        #[arg(long, default_value = "BLRD")]
        asset: String,
    },
    /// Ask for staked BLRD back; it returns after the unstake delay and is slashable until then
    Unstake {
        #[arg(long)]
        amount: u64,
        #[arg(long, default_value = "BLRD")]
        asset: String,
    },
    /// Redeem an under-collateralized issuer's options for BLRD from its stake, at a premium
    Liquidate {
        #[arg(long)]
        issuer: String,
        #[arg(long)]
        asset: String,
        #[arg(long)]
        quantity: u64,
    },
    /// Take deposits on a chain without resting orders: an address (or xpub) where takers pay you; the node keeps minting orders open for every asset on that chain
    SetReceiveAddress {
        #[arg(long)]
        chain: String,
        /// An address, or an xpub for a fresh address per claim; empty to stop taking deposits
        #[arg(long, default_value = "")]
        address: String,
    },
    /// Destroy options you issued and hold
    Burn {
        #[arg(long)]
        asset: String,
        #[arg(long)]
        quantity: u64,
    },
    /// Send BLRD or options to an account; the ledger converts to their currency
    Send {
        #[arg(long)]
        to: String,
        #[arg(long)]
        asset: String,
        #[arg(long)]
        amount: u64,
    },
    /// Burn options and have the real asset delivered to an address
    Exercise {
        #[arg(long)]
        asset: String,
        #[arg(long)]
        quantity: u64,
        #[arg(long)]
        receive_address: String,
        /// Call a particular maker (hex account); default lets the ledger choose
        #[arg(long, default_value = "")]
        issuer: String,
    },
    /// The currency you want to receive in
    SetCurrency {
        #[arg(long)]
        asset: String,
    },
    /// Cancel an order's unclaimed remainder
    Cancel {
        #[arg(long)]
        order: String,
    },
    /// Claim part of a resting order
    Claim {
        #[arg(long)]
        order: String,
        #[arg(long)]
        quantity: u64,
        /// Your address on the chain of the asset you are buying
        #[arg(long, default_value = "")]
        receive_address: String,
    },
    /// Report the txid that pays a leg (taker or maker)
    Report {
        #[arg(long = "match")]
        match_id: String,
        /// taker or maker
        #[arg(long)]
        leg: String,
        #[arg(long)]
        txid: String,
    },
}

pub fn keygen() -> Result<()> {
    let kp = Keypair::generate();
    let out = json!({
        "secret": kp.secret_hex(),
        "pubkey": hex::encode(kp.pubkey()),
        "key_hash": hex::encode(kp.key_hash()),
        "bitcoin_address": crate::enter::address_of(&kp).to_string(),
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn side(s: &str) -> Result<u8> {
    match s {
        "buy" | "0" => Ok(0),
        "sell" | "1" => Ok(1),
        _ => bail!("side must be buy or sell"),
    }
}

fn policy(s: &str) -> Result<u8> {
    match s {
        "seen" | "0" => Ok(0),
        "included" | "1" => Ok(1),
        "final" | "2" => Ok(2),
        _ => bail!("fill_policy must be seen, included or final"),
    }
}

pub async fn tx(a: TxArgs) -> Result<()> {
    let kp = Keypair::from_hex(&a.key)?;
    let pk = kp.pubkey();
    let node = a.node.trim_end_matches('/');
    let http = reqwest::Client::new();
    let nonce = match a.nonce {
        Some(n) => n,
        None => {
            let v: Value = http.get(format!("{node}/account/{}", hex::encode(pk))).send().await?.json().await?;
            v.get("nonce").and_then(|n| n.as_u64()).unwrap_or(0)
        }
    };
    let body = match a.cmd {
        TxCmd::Transfer { to, amount } => TxBody::Transfer { to: hex::decode(to)?, amount },
        TxCmd::PlaceOrder { market, side: s, quantity, price, bond, claim_bond, fill_policy, receive_address, expiry_height, settle, mint } => TxBody::PlaceOrder {
            mint,
            market,
            side: side(&s)?,
            quantity,
            price,
            bond,
            claim_bond,
            fill_policy: policy(&fill_policy)?,
            receive_address,
            expiry_height,
            settle: match settle.as_str() {
                "ledger" | "0" => 0,
                "chain" | "1" => 1,
                _ => bail!("settle must be ledger or chain"),
            },
        },
        TxCmd::Mint { asset, quantity, source } => TxBody::Mint { asset, quantity, source },
        TxCmd::Stake { amount, asset } => TxBody::Stake { amount, asset },
        TxCmd::Unstake { amount, asset } => TxBody::Unstake { amount, asset },
        TxCmd::Liquidate { issuer, asset, quantity } => TxBody::Liquidate { issuer: hex::decode(issuer)?, asset, quantity },
        TxCmd::SetReceiveAddress { chain, address } => TxBody::SetReceiveAddress { chain, address },
        TxCmd::Burn { asset, quantity } => TxBody::Burn { asset, quantity },
        TxCmd::Send { to, asset, amount } => TxBody::Send { to: hex::decode(to)?, asset, amount },
        TxCmd::Exercise { asset, quantity, receive_address, issuer } => TxBody::Exercise { asset, quantity, receive_address, issuer: if issuer.is_empty() { Vec::new() } else { hex::decode(issuer)? } },
        TxCmd::SetCurrency { asset } => TxBody::SetCurrency { asset },
        TxCmd::Cancel { order } => TxBody::CancelOrder { order_id: hex::decode(order)? },
        TxCmd::Claim { order, quantity, receive_address } => TxBody::Claim { order_id: hex::decode(order)?, quantity, receive_address },
        TxCmd::Report { match_id, leg, txid } => TxBody::ReportPayment {
            match_id: hex::decode(match_id)?,
            leg: match leg.as_str() {
                "taker" | "0" => 0,
                "maker" | "1" => 1,
                _ => bail!("leg must be taker or maker"),
            },
            txid,
        },
    };
    let tx_bytes = fbio::build_tx(&pk, nonce, &body);
    let signed = fbio::sign_tx(&tx_bytes, &kp);
    let res = http.post(format!("{node}/tx")).header("content-type", "application/octet-stream").body(signed).send().await?;
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(json!({"error": "no JSON in reply"}));
    println!("{}", serde_json::to_string_pretty(&v)?);
    if !status.is_success() {
        std::process::exit(1);
    }
    Ok(())
}
