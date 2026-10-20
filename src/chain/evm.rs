use super::{AssetInfo, PaymentStatus};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::time::Duration;

const TRANSFER_TOPIC: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

#[derive(Clone)]
pub struct EvmRpc {
    url: String,
    http: reqwest::Client,
    /// Per (address, amount): the newest block already scanned for a native payment.
    scanned: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, u64>>>,
}

fn hex_u128(v: Option<&Value>) -> u128 {
    v.and_then(|v| v.as_str()).and_then(|s| u128::from_str_radix(s.trim_start_matches("0x"), 16).ok()).unwrap_or(0)
}

fn hex_u64(v: Option<&Value>) -> u64 {
    hex_u128(v) as u64
}

fn addr_eq(a: &str, b: &str) -> bool {
    a.trim_start_matches("0x").eq_ignore_ascii_case(b.trim_start_matches("0x"))
}

impl EvmRpc {
    pub fn new(url: &str) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).build().expect("http client");
        Self { url: url.to_string(), http, scanned: Default::default() }
    }

    /// Find a payment of at least `amount` atoms of `asset` to `to` since `since_ms`:
    /// Transfer logs for a token, a scan of recent blocks for native ETH.
    pub async fn find_payment(&self, asset: &AssetInfo, to: &str, amount: u64, since_ms: u64) -> Result<Option<String>> {
        let need: u128 = amount as u128 * asset.atom_scale;
        let head = self.block_number().await?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let elapsed_blocks = (now.saturating_sub(since_ms / 1000) / 12 + 10).min(900);
        let from = head.saturating_sub(elapsed_blocks);
        if let Some(contract) = asset.contract {
            let topic_to = format!("0x{:0>64}", to.trim_start_matches("0x").to_lowercase());
            let logs = self
                .call("eth_getLogs", json!([{"address": contract, "fromBlock": format!("0x{from:x}"), "toBlock": "latest", "topics": [TRANSFER_TOPIC, null, topic_to]}]))
                .await?;
            let mut best: Option<String> = None;
            for l in logs.as_array().cloned().unwrap_or_default() {
                if hex_u128(l.get("data")) >= need {
                    best = l.get("transactionHash").and_then(|h| h.as_str()).map(|h| h.to_string());
                }
            }
            return Ok(best);
        }
        // Native ETH: look at blocks we have not seen for this address, newest first.
        let key = format!("{to}:{amount}:{since_ms}");
        let start = { self.scanned.lock().unwrap().get(&key).copied().map(|b| b + 1).unwrap_or(from) };
        let mut found = None;
        let mut n = head;
        while n >= start && head - n < 60 {
            let block = self.call("eth_getBlockByNumber", json!([format!("0x{n:x}"), true])).await?;
            if let Some(txs) = block.get("transactions").and_then(|t| t.as_array()) {
                for tx in txs {
                    if tx.get("to").and_then(|t| t.as_str()).map(|t| addr_eq(t, to)).unwrap_or(false) && hex_u128(tx.get("value")) >= need {
                        found = tx.get("hash").and_then(|h| h.as_str()).map(|h| h.to_string());
                    }
                }
            }
            if found.is_some() || n == 0 {
                break;
            }
            n -= 1;
        }
        if found.is_none() {
            self.scanned.lock().unwrap().insert(key, head);
        }
        Ok(found)
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let res: Value = self.http.post(&self.url).json(&body).send().await?.error_for_status()?.json().await?;
        if let Some(err) = res.get("error").filter(|e| !e.is_null()) {
            bail!("eth rpc {method}: {err}");
        }
        Ok(res.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn block_number(&self) -> Result<u64> {
        Ok(hex_u64(Some(&self.call("eth_blockNumber", json!([])).await?)))
    }

    pub async fn finalized_block_number(&self) -> Result<u64> {
        let b = self.call("eth_getBlockByNumber", json!(["finalized", false])).await?;
        Ok(hex_u64(b.get("number")))
    }

    /// Was `txid` sent from one of `sources`: the transaction sender for ether, the Transfer's `from` for a token?
    pub async fn paid_from(&self, asset: &AssetInfo, txid: &str, sources: &[String]) -> Result<bool> {
        let hit = |a: &str| sources.iter().any(|s| addr_eq(s, a));
        let tx = self.call("eth_getTransactionByHash", json!([txid])).await?;
        if asset.contract.is_none() {
            return Ok(tx.get("from").and_then(|f| f.as_str()).map(hit).unwrap_or(false));
        }
        let receipt = self.call("eth_getTransactionReceipt", json!([txid])).await?;
        Ok(receipt
            .get("logs")
            .and_then(|l| l.as_array())
            .map(|logs| {
                logs.iter().any(|l| {
                    let topics = l.get("topics").and_then(|t| t.as_array());
                    matches!(topics, Some(t) if t.len() == 3 && t[0].as_str().map(|s| addr_eq(s, TRANSFER_TOPIC)).unwrap_or(false) && t[1].as_str().map(|s| s.len() >= 40 && hit(&s[s.len() - 40..])).unwrap_or(false))
                })
            })
            .unwrap_or(false))
    }

    pub async fn check_payment(&self, asset: &AssetInfo, txid: &str, to: &str, amount: u64) -> Result<PaymentStatus> {
        let tx = self.call("eth_getTransactionByHash", json!([txid])).await?;
        if tx.is_null() {
            return Ok(PaymentStatus::Unseen);
        }
        let need: u128 = amount as u128 * asset.atom_scale;
        let block_hash = tx.get("blockHash").and_then(|b| b.as_str()).map(|s| s.to_string());
        let Some(block_hash) = block_hash else {
            // Pending. Check it at least targets the right place.
            let ok = match asset.contract {
                None => tx.get("to").and_then(|t| t.as_str()).map(|t| addr_eq(t, to)).unwrap_or(false) && hex_u128(tx.get("value")) >= need,
                Some(c) => tx.get("to").and_then(|t| t.as_str()).map(|t| addr_eq(t, c)).unwrap_or(false),
            };
            return Ok(if ok { PaymentStatus::Seen } else { PaymentStatus::Wrong("pending transaction does not target the payee".into()) });
        };
        let receipt = self.call("eth_getTransactionReceipt", json!([txid])).await?;
        if receipt.is_null() {
            return Ok(PaymentStatus::Seen);
        }
        if hex_u64(receipt.get("status")) != 1 {
            return Ok(PaymentStatus::Wrong("transaction reverted".into()));
        }
        let paid: u128 = match asset.contract {
            None => {
                if tx.get("to").and_then(|t| t.as_str()).map(|t| addr_eq(t, to)).unwrap_or(false) {
                    hex_u128(tx.get("value"))
                } else {
                    0
                }
            }
            Some(contract) => receipt
                .get("logs")
                .and_then(|l| l.as_array())
                .map(|logs| {
                    logs.iter()
                        .filter(|l| l.get("address").and_then(|a| a.as_str()).map(|a| addr_eq(a, contract)).unwrap_or(false))
                        .filter(|l| {
                            let topics = l.get("topics").and_then(|t| t.as_array());
                            match topics {
                                Some(t) if t.len() == 3 => {
                                    t[0].as_str().map(|s| addr_eq(s, TRANSFER_TOPIC)).unwrap_or(false)
                                        && t[2].as_str().map(|s| s.len() >= 40 && addr_eq(&s[s.len() - 40..], to)).unwrap_or(false)
                                }
                                _ => false,
                            }
                        })
                        .map(|l| hex_u128(l.get("data")))
                        .sum()
                })
                .unwrap_or(0),
        };
        if paid < need {
            return Ok(PaymentStatus::Wrong(format!("pays {paid} to {to}, needs {need}")));
        }
        let block_height = hex_u64(receipt.get("blockNumber"));
        let head = self.block_number().await?;
        let confirmations = head.saturating_sub(block_height) + 1;
        Ok(PaymentStatus::Included { block_hash, block_height, confirmations })
    }

    /// Deep means the block is at or below the chain's finalized block and the
    /// transaction is still in that block.
    pub async fn is_deep(&self, txid: &str, block_hash: &str, block_height: u64) -> Result<bool> {
        let finalized = self.finalized_block_number().await?;
        if finalized < block_height {
            return Ok(false);
        }
        let receipt = self.call("eth_getTransactionReceipt", json!([txid])).await?;
        Ok(receipt.get("blockHash").and_then(|b| b.as_str()).map(|b| addr_eq(b, block_hash)).unwrap_or(false))
    }

    /// Has the transaction left the block it was in?
    pub async fn is_reorged(&self, txid: &str, block_hash: &str) -> Result<bool> {
        let receipt = self.call("eth_getTransactionReceipt", json!([txid])).await?;
        Ok(!receipt.get("blockHash").and_then(|b| b.as_str()).map(|b| addr_eq(b, block_hash)).unwrap_or(false))
    }
}
