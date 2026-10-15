use crate::fbio::hex_bytes;
use anyhow::Result;
use serde::Serialize;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::FromRow;

pub async fn connect(url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new().max_connections(8).connect(url).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Account {
    #[serde(serialize_with = "hex_bytes")]
    pub pubkey: Vec<u8>,
    pub nonce: i64,
    pub balance: i64,
    pub locked: i64,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Order {
    #[serde(serialize_with = "hex_bytes")]
    pub id: Vec<u8>,
    pub seq: i64,
    #[serde(serialize_with = "hex_bytes")]
    pub account: Vec<u8>,
    pub market: String,
    pub base: String,
    pub quote: String,
    pub side: i16,
    pub quantity: i64,
    pub qty_left: i64,
    pub price: i64,
    pub bond: i64,
    pub bond_left: i64,
    pub claim_bond: i64,
    pub fill_policy: i16,
    pub recv_addr: String,
    pub expiry_height: i32,
    pub status: String,
    pub claims: i32,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Match {
    #[serde(serialize_with = "hex_bytes")]
    pub id: Vec<u8>,
    pub seq: i64,
    #[serde(serialize_with = "hex_bytes")]
    pub order_id: Vec<u8>,
    #[serde(serialize_with = "hex_bytes")]
    pub maker: Vec<u8>,
    #[serde(serialize_with = "hex_bytes")]
    pub taker: Vec<u8>,
    pub market: String,
    pub side: i16,
    pub quantity: i64,
    pub price: i64,
    pub bond: i64,
    pub claim_bond: i64,
    pub fill_policy: i16,
    pub taker_chain: String,
    pub taker_asset: String,
    pub taker_amount: i64,
    pub taker_pay_to: String,
    pub maker_chain: String,
    pub maker_asset: String,
    pub maker_amount: i64,
    pub maker_pay_to: String,
    pub status: String,
    pub claimed_ts: i64,
    pub claimed_height: i32,
    pub taker_txid: Option<String>,
    pub taker_block_hash: Option<String>,
    pub taker_block_height: Option<i64>,
    pub taker_included_height: Option<i32>,
    pub maker_txid: Option<String>,
    #[serde(serialize_with = "hex_opt")]
    pub maker_payer: Option<Vec<u8>>,
    pub maker_block_hash: Option<String>,
    pub maker_block_height: Option<i64>,
    pub maker_included_height: Option<i32>,
    pub closed_seq: Option<i64>,
    pub closed_reason: Option<String>,
}

pub fn hex_opt<S: serde::Serializer>(v: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(b) => s.serialize_str(&hex::encode(b)),
        None => s.serialize_none(),
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Epoch {
    pub height: i32,
    #[serde(serialize_with = "hex_bytes")]
    pub leader_key_hash: Vec<u8>,
    #[serde(serialize_with = "hex_opt")]
    pub leader_key: Option<Vec<u8>>,
    pub leader_url: Option<String>,
    pub entry_txid: Option<String>,
    #[serde(serialize_with = "hex_bytes")]
    pub checkpoint_tip: Vec<u8>,
    pub first_seq: Option<i64>,
    pub last_seq: Option<i64>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct LotteryEntry {
    pub txid: String,
    pub height: i32,
    #[serde(serialize_with = "hex_bytes")]
    pub key_hash: Vec<u8>,
    pub ip: String,
    pub port: i32,
    #[serde(serialize_with = "hex_bytes")]
    pub tip: Vec<u8>,
    pub fee_sats: i64,
    pub rank: Option<i32>,
    pub valid: Option<bool>,
}

#[derive(Debug, Clone, FromRow)]
pub struct LogRow {
    pub seq: i64,
    pub hash: Vec<u8>,
    pub bytes: Vec<u8>,
}

/// The newest log entry, or None at genesis.
pub async fn head<'e, E: sqlx::PgExecutor<'e>>(ex: E) -> Result<Option<(u64, [u8; 32], u64)>> {
    let row: Option<(i64, Vec<u8>, i64)> = sqlx::query_as("select seq, hash, ts from log order by seq desc limit 1").fetch_optional(ex).await?;
    Ok(row.map(|(s, h, t)| (s as u64, h.try_into().unwrap_or([0u8; 32]), t as u64)))
}
