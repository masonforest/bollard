//! Follow Bitcoin block by block: record lottery entries, run the draw, and
//! tell the node who leads each height.

use crate::crypto;
use crate::fbio::ZERO32;
use crate::lottery::{self, RawEntry};
use crate::node::{EpochInfo, Node};
use anyhow::{anyhow, Result};
use std::sync::Arc;
use std::time::Duration;

pub async fn run(node: Arc<Node>) {
    loop {
        if let Err(e) = step(&node).await {
            tracing::warn!("bitcoin watch: {e}");
        }
        tokio::time::sleep(Duration::from_secs(node.cfg.poll_secs)).await;
    }
}

async fn last_block(node: &Node) -> Result<Option<(u32, String)>> {
    let row: Option<(i32, String)> = sqlx::query_as("select height, hash from btc_blocks order by height desc limit 1").fetch_optional(&node.db).await?;
    Ok(row.map(|(h, s)| (h as u32, s)))
}

async fn publish_epoch(node: &Node, height: u32) -> Result<()> {
    let e: Option<crate::db::Epoch> = sqlx::query_as("select * from epochs where height = $1").bind(height as i32).fetch_optional(&node.db).await?;
    let Some(e) = e else { return Ok(()) };
    let info = EpochInfo {
        height,
        leader_key_hash: e.leader_key_hash.try_into().map_err(|_| anyhow!("bad key hash"))?,
        leader_url: e.leader_url,
        entry_txid: e.entry_txid,
        checkpoint: e.checkpoint_tip.try_into().map_err(|_| anyhow!("bad checkpoint"))?,
    };
    node.epoch_tx.send_replace(Some(info));
    Ok(())
}

async fn step(node: &Node) -> Result<()> {
    let btc = &node.chains.btc;
    let tip_hash = btc.best_block_hash().await?;
    let tip_header = btc.block_header(&tip_hash).await?.ok_or_else(|| anyhow!("tip header missing"))?;
    let tip_height = tip_header.get("height").and_then(|h| h.as_u64()).unwrap_or(0) as u32;

    let mut last = last_block(node).await?;
    if last.is_none() {
        // First run: the genesis block starts the network with the genesis leader.
        let g = node.cfg.genesis_height;
        if tip_height < g {
            tracing::info!("waiting for Bitcoin height {g} (tip {tip_height})");
            return Ok(());
        }
        let hash = btc.block_hash(g as u64).await?;
        let header = btc.block_header(&hash).await?.ok_or_else(|| anyhow!("genesis header missing"))?;
        sqlx::query("insert into btc_blocks (height, hash, prev_hash, ts) values ($1,$2,$3,$4) on conflict do nothing")
            .bind(g as i32)
            .bind(&hash)
            .bind(header.get("previousblockhash").and_then(|p| p.as_str()).unwrap_or(""))
            .bind(header.get("time").and_then(|t| t.as_i64()).unwrap_or(0))
            .execute(&node.db)
            .await?;
        sqlx::query("insert into epochs (height, leader_key_hash, leader_key, leader_url, entry_txid, checkpoint_tip) values ($1,$2,$3,$4,null,$5) on conflict (height) do update set leader_key_hash = excluded.leader_key_hash, leader_url = excluded.leader_url")
            .bind(g as i32)
            .bind(crypto::hash160(&node.genesis_leader).to_vec())
            .bind(node.genesis_leader.to_vec())
            .bind(&node.cfg.genesis_leader_url)
            .bind(ZERO32.to_vec())
            .execute(&node.db)
            .await?;
        tracing::info!(height = g, hash, "genesis block");
        last = Some((g, hash));
    }

    let (mut last_height, mut last_hash) = last.unwrap();
    if node.epoch().is_none() {
        publish_epoch(node, last_height).await?;
    }

    while last_height < tip_height {
        let height = last_height + 1;
        let hash = btc.block_hash(height as u64).await?;
        let block = btc.block_full(&hash).await?;
        let prev = block.get("previousblockhash").and_then(|p| p.as_str()).unwrap_or("").to_string();
        if prev != last_hash {
            // Bitcoin reorged under us: drop what we knew from the last block on and look again.
            tracing::warn!(height = last_height, "bitcoin reorg; dropping block {last_hash}");
            sqlx::query("delete from btc_blocks where height >= $1").bind(last_height as i32).execute(&node.db).await?;
            sqlx::query("delete from entries where height >= $1").bind(last_height as i32).execute(&node.db).await?;
            sqlx::query("delete from epochs where height >= $1").bind(last_height as i32).execute(&node.db).await?;
            let back = last_block(node).await?.ok_or_else(|| anyhow!("reorg past genesis"))?;
            last_height = back.0;
            last_hash = back.1;
            continue;
        }
        let ts = block.get("time").and_then(|t| t.as_i64()).unwrap_or(0);
        let raw = lottery::entries_in_block(&block);
        process_block(node, height, &hash, &prev, ts, raw).await?;
        publish_epoch(node, height).await?;
        last_height = height;
        last_hash = hash;
    }
    Ok(())
}

async fn process_block(node: &Node, height: u32, hash: &str, prev_hash: &str, ts: i64, raw: Vec<RawEntry>) -> Result<()> {
    let prev: crate::db::Epoch = sqlx::query_as("select * from epochs where height = $1")
        .bind(height as i32 - 1)
        .fetch_optional(&node.db)
        .await?
        .ok_or_else(|| anyhow!("no epoch for height {}", height - 1))?;
    let checkpoint: [u8; 32] = prev.checkpoint_tip.clone().try_into().unwrap_or(ZERO32);

    let mut txn = node.db.begin().await?;
    sqlx::query("insert into btc_blocks (height, hash, prev_hash, ts) values ($1,$2,$3,$4)").bind(height as i32).bind(hash).bind(prev_hash).bind(ts).execute(&mut *txn).await?;
    for e in &raw {
        sqlx::query("insert into entries (txid, height, key_hash, ip, port, tip, fee_sats) values ($1,$2,$3,$4,$5,$6,$7) on conflict do nothing")
            .bind(&e.txid)
            .bind(height as i32)
            .bind(e.payload.key_hash.to_vec())
            .bind(e.payload.ip.to_string())
            .bind(e.payload.port as i32)
            .bind(e.payload.tip.to_vec())
            .bind(e.fee_sats as i64)
            .execute(&mut *txn)
            .await?;
    }
    let ranked = lottery::rank(hash, &raw.iter().map(|e| (e.txid.clone(), e.fee_sats)).collect::<Vec<_>>());
    let mut winner: Option<&RawEntry> = None;
    for (rank, txid) in ranked.iter().enumerate() {
        let e = raw.iter().find(|e| &e.txid == txid).unwrap();
        let valid = winner.is_none() && tip_is_valid(node, &e.payload.tip, &checkpoint).await?;
        sqlx::query("update entries set rank = $2, valid = $3 where txid = $1").bind(txid).bind(rank as i32).bind(valid).execute(&mut *txn).await?;
        if valid {
            winner = Some(e);
        }
    }
    match winner {
        Some(w) => {
            let known_key: Option<(Vec<u8>,)> = sqlx::query_as("select leader_key from epochs where leader_key_hash = $1 and leader_key is not null order by height desc limit 1")
                .bind(w.payload.key_hash.to_vec())
                .fetch_optional(&mut *txn)
                .await?;
            let my_key = if w.payload.key_hash == node.my_key_hash() { Some(node.kp.pubkey().to_vec()) } else { known_key.map(|k| k.0) };
            sqlx::query("insert into epochs (height, leader_key_hash, leader_key, leader_url, entry_txid, checkpoint_tip) values ($1,$2,$3,$4,$5,$6) on conflict (height) do update set leader_key_hash = excluded.leader_key_hash, leader_key = excluded.leader_key, leader_url = excluded.leader_url, entry_txid = excluded.entry_txid, checkpoint_tip = excluded.checkpoint_tip")
                .bind(height as i32)
                .bind(w.payload.key_hash.to_vec())
                .bind(my_key)
                .bind(w.payload.url())
                .bind(&w.txid)
                .bind(w.payload.tip.to_vec())
                .execute(&mut *txn)
                .await?;
            tracing::info!(height, txid = w.txid, leader = hex::encode(w.payload.key_hash), url = w.payload.url(), "lottery winner");
        }
        None => {
            sqlx::query("insert into epochs (height, leader_key_hash, leader_key, leader_url, entry_txid, checkpoint_tip) values ($1,$2,$3,$4,null,$5) on conflict (height) do update set leader_key_hash = excluded.leader_key_hash, leader_key = excluded.leader_key, leader_url = excluded.leader_url, checkpoint_tip = excluded.checkpoint_tip")
                .bind(height as i32)
                .bind(&prev.leader_key_hash)
                .bind(&prev.leader_key)
                .bind(&prev.leader_url)
                .bind(&prev.checkpoint_tip)
                .execute(&mut *txn)
                .await?;
            if raw.is_empty() {
                tracing::info!(height, "block has no entries; leader carries over");
            } else {
                tracing::info!(height, "block has {} entries, none valid; leader carries over", raw.len());
            }
        }
    }
    txn.commit().await?;
    Ok(())
}

/// A committed tip is valid if we hold it and it is at or after the checkpoint.
async fn tip_is_valid(node: &Node, tip: &[u8; 32], checkpoint: &[u8; 32]) -> Result<bool> {
    if tip == &ZERO32 {
        return Ok(checkpoint == &ZERO32);
    }
    let tip_seq: Option<(i64,)> = sqlx::query_as("select seq from log where hash = $1").bind(tip.to_vec()).fetch_optional(&node.db).await?;
    let Some((tip_seq,)) = tip_seq else {
        tracing::warn!("entry commits to unknown tip {}", hex::encode(tip));
        return Ok(false);
    };
    if checkpoint == &ZERO32 {
        return Ok(true);
    }
    let cp_seq: Option<(i64,)> = sqlx::query_as("select seq from log where hash = $1").bind(checkpoint.to_vec()).fetch_optional(&node.db).await?;
    Ok(cp_seq.map(|(c,)| tip_seq >= c).unwrap_or(false))
}
