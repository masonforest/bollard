//! The leader watches every account's burn addresses and mints what lands
//! there. Bitcoin burns mint after one confirmation; Ethereum burns mint once
//! their block is finalized, so a minted entry can never be reorged away.

use crate::chain::{self, burn, evm::TRANSFER_TOPIC};
use crate::fbio::Body;
use crate::node::{Node, Role};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub async fn run(node: Arc<Node>) {
    loop {
        tokio::time::sleep(Duration::from_secs(node.cfg.poll_secs.max(10) * 3)).await;
        if node.role().await != Role::Leader {
            continue;
        }
        if let Err(e) = scan_btc(&node).await {
            tracing::warn!("btc burn scan: {e}");
        }
        if let Err(e) = scan_eth(&node).await {
            tracing::warn!("eth burn scan: {e}");
        }
    }
}

/// Every account the node knows: anything that has transacted or registered a passkey.
async fn accounts(node: &Node) -> Result<Vec<Vec<u8>>> {
    let mut v: Vec<(Vec<u8>,)> = sqlx::query_as("select pubkey from accounts union select pubkey from passkeys").fetch_all(&node.db).await?;
    Ok(v.drain(..).map(|r| r.0).collect())
}

async fn already(node: &Node, txid: &str, asset: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("select count(*) from burns where txid = $1 and asset = $2").bind(txid).bind(asset).fetch_one(&node.db).await?;
    Ok(n > 0)
}

async fn scan_btc(node: &Node) -> Result<()> {
    let btc = &node.chains.btc;
    let asset = chain::asset("btc:BTC").expect("btc");
    for pk in accounts(node).await? {
        let addr = burn::btc(&pk);
        let txs: Vec<Value> = match btc.http.get(format!("{}/address/{addr}/txs", btc.esplora)).send().await?.error_for_status() {
            Ok(r) => r.json().await?,
            Err(e) => {
                tracing::debug!("esplora {addr}: {e}");
                continue;
            }
        };
        for tx in txs {
            let confirmed = tx.pointer("/status/confirmed").and_then(|c| c.as_bool()).unwrap_or(false);
            if !confirmed {
                continue;
            }
            let Some(txid) = tx.get("txid").and_then(|t| t.as_str()) else { continue };
            if already(node, txid, asset.id).await? {
                continue;
            }
            let paid: u64 = tx
                .get("vout")
                .and_then(|v| v.as_array())
                .map(|outs| outs.iter().filter(|o| o.get("scriptpubkey_address").and_then(|a| a.as_str()) == Some(addr.as_str())).map(|o| o.get("value").and_then(|v| v.as_u64()).unwrap_or(0)).sum())
                .unwrap_or(0);
            if paid == 0 {
                continue;
            }
            let block_hash = tx.pointer("/status/block_hash").and_then(|h| h.as_str()).unwrap_or("").to_string();
            let block_height = tx.pointer("/status/block_height").and_then(|h| h.as_u64()).unwrap_or(0);
            tracing::info!(%txid, paid, account = %hex::encode(&pk), "bitcoin burned; minting");
            node.append(Body::Minted { account: pk.clone(), asset: asset.id.to_string(), amount: paid, txid: txid.to_string(), block_hash, block_height }).await?;
        }
    }
    Ok(())
}

async fn meta_get(node: &Node, key: &str) -> Result<Option<String>> {
    let v: Option<(String,)> = sqlx::query_as("select value from meta where key = $1").bind(key).fetch_optional(&node.db).await?;
    Ok(v.map(|r| r.0))
}

async fn meta_set(node: &Node, key: &str, value: &str) -> Result<()> {
    sqlx::query("insert into meta (key, value) values ($1, $2) on conflict (key) do update set value = $2").bind(key).bind(value).execute(&node.db).await?;
    Ok(())
}

async fn scan_eth(node: &Node) -> Result<()> {
    let eth = &node.chains.eth;
    let accts = accounts(node).await?;
    if accts.is_empty() {
        return Ok(());
    }
    let by_addr: HashMap<String, Vec<u8>> = accts.iter().map(|pk| (burn::eth(pk).to_lowercase(), pk.clone())).collect();
    let finalized = eth.finalized_block_number().await?;
    let from = match meta_get(node, "eth_burn_scanned").await? {
        Some(v) => v.parse::<u64>().unwrap_or(finalized) + 1,
        None => finalized.saturating_sub(5),
    };
    if from > finalized {
        return Ok(());
    }
    let to = finalized.min(from + 400);
    // Tokens: one getLogs per contract, every burn address in the `to` topic.
    let topics_to: Vec<String> = by_addr.keys().map(|a| format!("0x{:0>64}", a.trim_start_matches("0x"))).collect();
    for asset in chain::ASSETS.iter().filter(|a| a.chain == "eth" && a.contract.is_some()) {
        let logs = eth
            .call("eth_getLogs", json!([{"address": asset.contract, "fromBlock": format!("0x{from:x}"), "toBlock": format!("0x{to:x}"), "topics": [TRANSFER_TOPIC, null, topics_to]}]))
            .await?;
        let mut per_tx: HashMap<(String, String), (u128, String, u64)> = HashMap::new();
        for l in logs.as_array().cloned().unwrap_or_default() {
            let Some(topics) = l.get("topics").and_then(|t| t.as_array()) else { continue };
            let Some(t2) = topics.get(2).and_then(|t| t.as_str()) else { continue };
            let to_addr = format!("0x{}", &t2[t2.len() - 40..]).to_lowercase();
            let Some(txid) = l.get("transactionHash").and_then(|h| h.as_str()) else { continue };
            let data = l.get("data").and_then(|d| d.as_str()).and_then(|d| u128::from_str_radix(d.trim_start_matches("0x"), 16).ok()).unwrap_or(0);
            let bh = l.get("blockHash").and_then(|h| h.as_str()).unwrap_or("").to_string();
            let bn = l.get("blockNumber").and_then(|h| h.as_str()).and_then(|h| u64::from_str_radix(h.trim_start_matches("0x"), 16).ok()).unwrap_or(0);
            let e = per_tx.entry((txid.to_string(), to_addr)).or_insert((0, bh, bn));
            e.0 += data;
        }
        for ((txid, to_addr), (wei, bh, bn)) in per_tx {
            let Some(pk) = by_addr.get(&to_addr) else { continue };
            let atoms = (wei / asset.atom_scale) as u64;
            if atoms == 0 || already(node, &txid, asset.id).await? {
                continue;
            }
            tracing::info!(%txid, atoms, asset = asset.id, account = %hex::encode(pk), "token burned; minting");
            node.append(Body::Minted { account: pk.clone(), asset: asset.id.to_string(), amount: atoms, txid, block_hash: bh, block_height: bn }).await?;
        }
    }
    // Native ETH: look at every transaction in the finalized blocks we have not seen.
    let asset = chain::asset("eth:ETH").expect("eth");
    for n in from..=to {
        let block = eth.call("eth_getBlockByNumber", json!([format!("0x{n:x}"), true])).await?;
        let bh = block.get("hash").and_then(|h| h.as_str()).unwrap_or("").to_string();
        let Some(txs) = block.get("transactions").and_then(|t| t.as_array()) else { continue };
        for tx in txs {
            let Some(to_addr) = tx.get("to").and_then(|t| t.as_str()).map(|t| t.to_lowercase()) else { continue };
            let Some(pk) = by_addr.get(&to_addr) else { continue };
            let wei = tx.get("value").and_then(|v| v.as_str()).and_then(|v| u128::from_str_radix(v.trim_start_matches("0x"), 16).ok()).unwrap_or(0);
            let atoms = (wei / asset.atom_scale) as u64;
            let Some(txid) = tx.get("hash").and_then(|h| h.as_str()) else { continue };
            if atoms == 0 || already(node, txid, asset.id).await? {
                continue;
            }
            tracing::info!(%txid, atoms, account = %hex::encode(pk), "ether burned; minting");
            node.append(Body::Minted { account: pk.clone(), asset: asset.id.to_string(), amount: atoms, txid: txid.to_string(), block_hash: bh.clone(), block_height: n }).await?;
        }
    }
    meta_set(node, "eth_burn_scanned", &to.to_string()).await
}
