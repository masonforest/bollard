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
    #[arg(long, env = "BOLLARD_NODE", default_value = "https://bollard.exchange")]
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
        #[arg(long, name = "match")]
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
        TxCmd::PlaceOrder { market, side: s, quantity, price, bond, claim_bond, fill_policy, receive_address, expiry_height } => TxBody::PlaceOrder {
            market,
            side: side(&s)?,
            quantity,
            price,
            bond,
            claim_bond,
            fill_policy: policy(&fill_policy)?,
            receive_address,
            expiry_height,
        },
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
