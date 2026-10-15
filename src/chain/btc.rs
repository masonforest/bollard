use super::PaymentStatus;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct BtcRpc {
    url: String,
    esplora: String,
    http: reqwest::Client,
}

impl BtcRpc {
    pub fn new(url: &str, esplora: &str) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(120)).build().expect("http client");
        Self { url: url.to_string(), esplora: esplora.trim_end_matches('/').to_string(), http }
    }

    /// The newest transaction (mempool included) paying at least `amount` sats to `to`
    /// since `since_ms`, via the Esplora API.
    pub async fn find_payment(&self, to: &str, amount: u64, since_ms: u64) -> Result<Option<String>> {
        let txs: Vec<Value> = self.http.get(format!("{}/address/{to}/txs", self.esplora)).send().await?.error_for_status()?.json().await?;
        let since = since_ms / 1000;
        for tx in txs {
            let confirmed = tx.pointer("/status/confirmed").and_then(|c| c.as_bool()).unwrap_or(false);
            let block_time = tx.pointer("/status/block_time").and_then(|t| t.as_u64()).unwrap_or(0);
            if confirmed && block_time + 1800 < since {
                continue; // mined well before the claim
            }
            let paid: u64 = tx
                .get("vout")
                .and_then(|v| v.as_array())
                .map(|outs| outs.iter().filter(|o| o.get("scriptpubkey_address").and_then(|a| a.as_str()) == Some(to)).map(|o| o.get("value").and_then(|v| v.as_u64()).unwrap_or(0)).sum())
                .unwrap_or(0);
            if paid >= amount {
                return Ok(tx.get("txid").and_then(|t| t.as_str()).map(|t| t.to_string()));
            }
        }
        Ok(None)
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let res: Value = self.http.post(&self.url).json(&body).send().await?.error_for_status()?.json().await?;
        if let Some(err) = res.get("error").filter(|e| !e.is_null()) {
            bail!("bitcoin rpc {method}: {err}");
        }
        Ok(res.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Like `call`, but a JSON-RPC error with this code becomes Ok(None).
    async fn call_opt(&self, method: &str, params: Value, missing_code: i64) -> Result<Option<Value>> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let res: Value = self.http.post(&self.url).json(&body).send().await?.error_for_status()?.json().await?;
        if let Some(err) = res.get("error").filter(|e| !e.is_null()) {
            if err.get("code").and_then(|c| c.as_i64()) == Some(missing_code) {
                return Ok(None);
            }
            bail!("bitcoin rpc {method}: {err}");
        }
        Ok(res.get("result").cloned())
    }

    pub async fn best_block_hash(&self) -> Result<String> {
        Ok(self.call("getbestblockhash", json!([])).await?.as_str().unwrap_or("").to_string())
    }

    pub async fn block_count(&self) -> Result<u64> {
        Ok(self.call("getblockcount", json!([])).await?.as_u64().unwrap_or(0))
    }

    pub async fn block_hash(&self, height: u64) -> Result<String> {
        Ok(self.call("getblockhash", json!([height])).await?.as_str().unwrap_or("").to_string())
    }

    /// Header only: height, previousblockhash, time, confirmations (-1 if off the chain).
    pub async fn block_header(&self, hash: &str) -> Result<Option<Value>> {
        self.call_opt("getblockheader", json!([hash, true]), -5).await
    }

    /// Full block with every transaction decoded and each transaction's fee.
    pub async fn block_full(&self, hash: &str) -> Result<Value> {
        self.call("getblock", json!([hash, 2]), ).await
    }

    pub async fn send_raw_transaction(&self, hex: &str) -> Result<String> {
        Ok(self.call("sendrawtransaction", json!([hex])).await?.as_str().unwrap_or("").to_string())
    }

    pub async fn get_transaction(&self, txid: &str) -> Result<Option<Value>> {
        self.call_opt("getrawtransaction", json!([txid, true]), -5).await
    }

    /// Does `txid` pay at least `amount` sats to `to`, and where is it?
    pub async fn check_payment(&self, txid: &str, to: &str, amount: u64) -> Result<PaymentStatus> {
        let Some(tx) = self.get_transaction(txid).await? else { return Ok(PaymentStatus::Unseen) };
        let paid: u64 = tx
            .get("vout")
            .and_then(|v| v.as_array())
            .map(|outs| {
                outs.iter()
                    .filter(|o| o.pointer("/scriptPubKey/address").and_then(|a| a.as_str()) == Some(to))
                    .map(|o| (o.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0) * 1e8).round() as u64)
                    .sum()
            })
            .unwrap_or(0);
        if paid < amount {
            return Ok(PaymentStatus::Wrong(format!("pays {paid} sats to {to}, needs {amount}")));
        }
        let confirmations = tx.get("confirmations").and_then(|c| c.as_i64()).unwrap_or(0);
        match tx.get("blockhash").and_then(|b| b.as_str()) {
            None if confirmations < 0 => Ok(PaymentStatus::Reorged),
            None => Ok(PaymentStatus::Seen),
            Some(bh) => {
                let header = self.block_header(bh).await?.ok_or_else(|| anyhow!("block {bh} unknown"))?;
                let height = header.get("height").and_then(|h| h.as_u64()).unwrap_or(0);
                if confirmations < 1 {
                    return Ok(PaymentStatus::Reorged);
                }
                Ok(PaymentStatus::Included { block_hash: bh.to_string(), block_height: height, confirmations: confirmations as u64 })
            }
        }
    }

    pub async fn is_deep(&self, txid: &str, block_hash: &str, depth: u64) -> Result<bool> {
        let Some(tx) = self.get_transaction(txid).await? else { return Ok(false) };
        if tx.get("blockhash").and_then(|b| b.as_str()) != Some(block_hash) {
            return Ok(false);
        }
        let c = tx.get("confirmations").and_then(|c| c.as_i64()).unwrap_or(0);
        Ok(c >= depth as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Network test: the node's own address received 11,978 sats once.
    #[tokio::test]
    #[ignore]
    async fn finds_a_known_payment() {
        let rpc = BtcRpc::new("https://bitcoin.publicnode.com", "https://mempool.space/api");
        let found = rpc.find_payment("bc1quud6049g028awqeq73tm3htsdzggdjdum4cwvw", 10_000, 0).await.unwrap();
        assert!(found.is_some(), "expected the funding transaction");
        let none = rpc.find_payment("bc1quud6049g028awqeq73tm3htsdzggdjdum4cwvw", 10_000_000, 0).await.unwrap();
        assert!(none.is_none(), "nobody paid 0.1 BTC");
    }
}
