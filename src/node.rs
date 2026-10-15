//! The node: one writer at a time over the log. As leader it sequences
//! transactions and asserts what it sees on the chains; as follower it
//! replays the leader's stream, checking every assertion itself.

use crate::chain::{self, chain_params, Chains, PaymentStatus, GRACE_BLOCKS};
use crate::config::RunArgs;
use crate::crypto::{self, Keypair};
use crate::fbio::{self, Body, ParsedEntry};
use crate::{api, apply, bitcoin_watch, db};
use anyhow::{anyhow, bail, Result};
use base64::Engine;
use futures::StreamExt;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch, Mutex, RwLock};

#[derive(Clone, Debug, Serialize)]
pub struct EpochInfo {
    pub height: u32,
    #[serde(serialize_with = "fbio::hex_bytes")]
    pub leader_key_hash: [u8; 20],
    pub leader_url: Option<String>,
    pub entry_txid: Option<String>,
    #[serde(serialize_with = "fbio::hex_bytes")]
    pub checkpoint: [u8; 32],
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "role")]
pub enum Role {
    Starting,
    Leader,
    Follower { url: String },
    Stalled { reason: String },
}

pub struct LogMsg {
    pub seq: u64,
    pub hash: [u8; 32],
    pub bytes: Vec<u8>,
}

pub struct Head {
    pub seq: u64,
    pub hash: [u8; 32],
    pub time: u64,
}

pub struct Node {
    pub cfg: RunArgs,
    pub db: PgPool,
    pub kp: Keypair,
    pub chains: Chains,
    pub genesis_leader: [u8; 33],
    pub log_tx: broadcast::Sender<Arc<LogMsg>>,
    pub event_tx: broadcast::Sender<String>,
    pub epoch_tx: watch::Sender<Option<EpochInfo>>,
    pub head: Mutex<Head>,
    pub role: RwLock<Role>,
    pub http: reqwest::Client,
    pub started: u64,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

const EPOCH_UNKNOWN: &str = "epoch unknown";

impl Node {
    pub fn my_key_hash(&self) -> [u8; 20] {
        self.kp.key_hash()
    }

    pub fn epoch(&self) -> Option<EpochInfo> {
        self.epoch_tx.borrow().clone()
    }

    pub fn height(&self) -> u32 {
        self.epoch().map(|e| e.height).unwrap_or(self.cfg.genesis_height)
    }

    pub async fn role(&self) -> Role {
        self.role.read().await.clone()
    }

    async fn set_role(&self, r: Role) {
        let mut w = self.role.write().await;
        if *w != r {
            tracing::info!(role = ?r, "role");
            *w = r;
        }
    }

    /// Store an entry and apply it, atomically.
    async fn store(&self, e: &ParsedEntry) -> Result<Vec<Value>> {
        let mut txn = self.db.begin().await?;
        sqlx::query("insert into log (seq, hash, prev, height, ts, kind, leader, bytes) values ($1,$2,$3,$4,$5,$6,$7,$8)")
            .bind(e.seq as i64)
            .bind(e.hash.to_vec())
            .bind(e.prev.to_vec())
            .bind(e.height as i32)
            .bind(e.time as i64)
            .bind(e.kind())
            .bind(e.leader.to_vec())
            .bind(&e.bytes)
            .execute(&mut *txn)
            .await?;
        let events = apply::apply(&mut txn, e).await?;
        txn.commit().await?;
        Ok(events)
    }

    fn publish(&self, e: &ParsedEntry, events: Vec<Value>) {
        let _ = self.log_tx.send(Arc::new(LogMsg { seq: e.seq, hash: e.hash, bytes: e.bytes.clone() }));
        let summary = json!({"event": "Entry", "seq": e.seq, "hash": hex::encode(e.hash), "kind": e.kind(), "height": e.height, "time": e.time});
        let _ = self.event_tx.send(summary.to_string());
        for ev in events {
            let _ = self.event_tx.send(ev.to_string());
        }
    }

    /// Leader only: sequence a new entry at the current height.
    pub async fn append(&self, body: Body) -> Result<ParsedEntry> {
        self.append_at(self.height(), body).await
    }

    /// Leader only: sequence a new entry stamped with `height` (used to catch
    /// up block rewards for blocks that passed while the node was away).
    pub async fn append_at(&self, height: u32, body: Body) -> Result<ParsedEntry> {
        let mut head = self.head.lock().await;
        if self.role().await != Role::Leader {
            bail!("not the leader");
        }
        let time = now_ms().max(head.time);
        let content = fbio::build_entry_content(head.seq + 1, &head.hash, height, time, &self.kp.pubkey(), &body);
        let bytes = fbio::sign_entry(&content, &self.kp);
        let e = fbio::parse_entry(&bytes)?;
        let events = self.store(&e).await?;
        head.seq = e.seq;
        head.hash = e.hash;
        head.time = e.time;
        drop(head);
        tracing::info!(seq = e.seq, kind = e.kind(), "appended");
        self.publish(&e, events);
        Ok(e)
    }

    /// Follower: take the next entry from the leader's stream.
    pub async fn ingest(&self, bytes: &[u8]) -> Result<ParsedEntry> {
        let e = fbio::parse_entry(bytes)?;
        let mut head = self.head.lock().await;
        if e.seq != head.seq + 1 {
            bail!("expected seq {}, got {}", head.seq + 1, e.seq);
        }
        if e.prev != head.hash {
            bail!("prev mismatch at seq {}", e.seq);
        }
        if e.time < head.time {
            bail!("time went backwards at seq {}", e.seq);
        }
        self.check_leader(&e).await?;
        self.verify_assertion(&e).await?;
        let events = self.store(&e).await?;
        head.seq = e.seq;
        head.hash = e.hash;
        head.time = e.time;
        drop(head);
        tracing::info!(seq = e.seq, kind = e.kind(), "ingested");
        self.publish(&e, events);
        Ok(e)
    }

    /// The entry's leader must be the leader Bitcoin picked for its height.
    async fn check_leader(&self, e: &ParsedEntry) -> Result<()> {
        let kh = crypto::hash160(&e.leader).to_vec();
        let row: Option<(Vec<u8>, Option<String>)> = sqlx::query_as("select leader_key_hash, entry_txid from epochs where height = $1").bind(e.height as i32).fetch_optional(&self.db).await?;
        let Some((expected, entry_txid)) = row else {
            if e.height == self.cfg.genesis_height && e.leader == self.genesis_leader {
                return Ok(());
            }
            bail!("{EPOCH_UNKNOWN} for height {}", e.height);
        };
        if expected != kh {
            bail!("entry at height {} signed by {} but the leader is {}", e.height, hex::encode(&kh), hex::encode(&expected));
        }
        if let Body::EpochStart { height, entry_txid: claimed } = &e.body {
            if *height != e.height || entry_txid.as_deref().unwrap_or("") != claimed {
                bail!("EpochStart does not match the lottery result for height {height}");
            }
        }
        Ok(())
    }

    /// Followers check every leader assertion against their own chain nodes.
    async fn verify_assertion(&self, e: &ParsedEntry) -> Result<()> {
        let check = |ok: bool, what: String| if ok { Ok(()) } else { Err(anyhow!("assertion failed: {what}")) };
        match &e.body {
            Body::LegIncluded { match_id, leg, txid, block_hash, block_height } => {
                let m = self.get_match(match_id).await?;
                let (asset, to, amount) = if *leg == 0 { (m.taker_asset, m.taker_pay_to, m.taker_amount) } else { (m.maker_asset, m.maker_pay_to, m.maker_amount) };
                let asset = chain::asset(&asset).ok_or_else(|| anyhow!("unknown asset"))?;
                let st = self.check_with_retry(|| self.chains.check_payment(asset, txid, &to, amount as u64)).await?;
                check(
                    matches!(&st, PaymentStatus::Included { block_hash: bh, block_height: h, .. } if bh.eq_ignore_ascii_case(block_hash) && h == block_height),
                    format!("LegIncluded {} leg {leg}: chain says {st:?}", hex::encode(match_id)),
                )
            }
            Body::MatchClosed { match_id } => {
                let m = self.get_match(match_id).await?;
                let (Some(txid), Some(bh), Some(h)) = (m.maker_txid, m.maker_block_hash, m.maker_block_height) else { bail!("MatchClosed before the maker leg was included") };
                let deep = self.check_with_retry(|| self.chains.is_deep(&m.maker_chain, &txid, &bh, h as u64)).await?;
                check(deep, format!("MatchClosed {}: maker payment is not deep", hex::encode(match_id)))
            }
            Body::ClaimVoid { match_id, reason: 2 } => {
                let m = self.get_match(match_id).await?;
                let Some(txid) = m.taker_txid else { bail!("ClaimVoid(reorg) with no taker payment") };
                let asset = chain::asset(&m.taker_asset).ok_or_else(|| anyhow!("unknown asset"))?;
                let st = self.check_with_retry(|| self.chains.check_payment(asset, &txid, &m.taker_pay_to, m.taker_amount as u64)).await?;
                check(matches!(st, PaymentStatus::Reorged | PaymentStatus::Unseen), format!("ClaimVoid(reorg) {}: chain says {st:?}", hex::encode(match_id)))
            }
            Body::LegMissed { match_id, .. } => {
                let m = self.get_match(match_id).await?;
                if let (Some(txid), Some(bh), Some(h)) = (m.maker_txid, m.maker_block_hash, m.maker_block_height) {
                    let deep = self.check_with_retry(|| self.chains.is_deep(&m.maker_chain, &txid, &bh, h as u64)).await?;
                    check(!deep, format!("LegMissed {}: maker payment is deep", hex::encode(match_id)))
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }

    async fn check_with_retry<T, F, Fut>(&self, f: F) -> Result<T>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut last = None;
        for _ in 0..4 {
            match f().await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    tracing::warn!("chain check failed, retrying: {e}");
                    last = Some(e);
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("chain check failed")))
    }

    pub async fn get_match(&self, id: &[u8]) -> Result<db::Match> {
        sqlx::query_as::<_, db::Match>("select * from matches where id = $1").bind(id).fetch_optional(&self.db).await?.ok_or_else(|| anyhow!("unknown match"))
    }

    /// A user transaction: sequence it here if leading, otherwise forward it.
    pub async fn submit_tx(&self, signed: &[u8]) -> Result<Value> {
        let tx = fbio::parse_signed_tx(signed)?;
        match self.role().await {
            Role::Leader => {
                let e = self.append(Body::Tx(tx)).await?;
                Ok(json!({"ok": true, "seq": e.seq, "hash": hex::encode(e.hash)}))
            }
            Role::Follower { url } => {
                let res = self.http.post(format!("{url}/tx")).header("content-type", "application/octet-stream").body(signed.to_vec()).send().await?;
                let status = res.status();
                let body: Value = res.json().await.unwrap_or(json!({"error": "leader returned no JSON"}));
                if !status.is_success() {
                    bail!("{}", body.get("error").and_then(|e| e.as_str()).unwrap_or("leader rejected the transaction"));
                }
                Ok(body)
            }
            r => bail!("node is not accepting transactions: {r:?}"),
        }
    }

    /// Drop every entry from `from_seq` on and rebuild state from what is left.
    pub async fn truncate(&self, from_seq: u64) -> Result<()> {
        let mut head = self.head.lock().await;
        let mut txn = self.db.begin().await?;
        sqlx::query("delete from log where seq >= $1").bind(from_seq as i64).execute(&mut *txn).await?;
        apply::reset_state(&mut txn).await?;
        let mut rows = sqlx::query_as::<_, db::LogRow>("select seq, hash, bytes from log order by seq").fetch(&mut *txn);
        let mut replayed = Vec::new();
        while let Some(r) = rows.next().await {
            replayed.push(r?);
        }
        drop(rows);
        let mut last = (0u64, fbio::ZERO32, 0u64);
        for r in replayed {
            let e = fbio::parse_entry(&r.bytes)?;
            apply::apply(&mut txn, &e).await?;
            last = (e.seq, e.hash, e.time);
        }
        txn.commit().await?;
        head.seq = last.0;
        head.hash = last.1;
        head.time = last.2;
        tracing::warn!(from_seq, head = head.seq, "log truncated and state replayed");
        Ok(())
    }

    /// Fetch the leader's recent log and reconcile a fork.
    async fn resync(&self, url: &str) -> Result<()> {
        let head_seq = self.head.lock().await.seq;
        let from = head_seq.saturating_sub(500).max(1);
        let rows: Vec<Value> = self.http.get(format!("{url}/log/range?from={from}&to={head_seq}")).send().await?.error_for_status()?.json().await?;
        let mut divergence = None;
        for r in &rows {
            let seq = r.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
            let hash = r.get("hash").and_then(|h| h.as_str()).unwrap_or("");
            let ours: Option<(Vec<u8>,)> = sqlx::query_as("select hash from log where seq = $1").bind(seq as i64).fetch_optional(&self.db).await?;
            if ours.map(|(h,)| hex::encode(h)) != Some(hash.to_string()) {
                divergence = Some(seq);
                break;
            }
        }
        let Some(d) = divergence else { return Ok(()) };
        self.truncate(d).await?;
        for r in rows.iter().filter(|r| r.get("seq").and_then(|s| s.as_u64()).unwrap_or(0) >= d) {
            let b64 = r.get("bytes").and_then(|b| b.as_str()).unwrap_or("");
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
            self.ingest(&bytes).await?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- roles

    async fn roles_loop(self: Arc<Self>) {
        let mut rx = self.epoch_tx.subscribe();
        let mut follower: Option<tokio::task::JoinHandle<()>> = None;
        loop {
            let info = rx.borrow_and_update().clone();
            if let Some(info) = info {
                if info.leader_key_hash == self.my_key_hash() {
                    if let Some(h) = follower.take() {
                        h.abort();
                    }
                    self.set_role(Role::Leader).await;
                    if let Err(e) = self.start_epoch(&info).await {
                        tracing::error!("could not start epoch {}: {e}", info.height);
                    }
                } else {
                    let url = info.leader_url.clone().or_else(|| {
                        if info.leader_key_hash == crypto::hash160(&self.genesis_leader) { self.cfg.genesis_leader_url.clone() } else { None }
                    });
                    match url {
                        Some(url) => {
                            let changed = self.role().await != (Role::Follower { url: url.clone() });
                            if changed {
                                if let Some(h) = follower.take() {
                                    h.abort();
                                }
                                self.set_role(Role::Follower { url: url.clone() }).await;
                                follower = Some(tokio::spawn(self.clone().follow(url)));
                            }
                        }
                        None => self.set_role(Role::Stalled { reason: format!("no address known for leader {}", hex::encode(info.leader_key_hash)) }).await,
                    }
                }
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Append one EpochStart per Bitcoin block we lead, including blocks with
    /// no lottery (the leader carries over and still earns the block's reward),
    /// and catch up any blocks that passed while this node was away.
    async fn start_epoch(&self, info: &EpochInfo) -> Result<()> {
        // Every block up to the current one that this node led and has not minted yet.
        let missing: Vec<(i32, Option<String>)> = sqlx::query_as(
            "select height, entry_txid from epochs where leader_key_hash = $1 and height <= $2 and height not in (select height from log where kind = 'EpochStart') order by height",
        )
        .bind(self.my_key_hash().to_vec())
        .bind(info.height as i32)
        .fetch_all(&self.db)
        .await?;
        for (h, txid) in missing {
            self.append_at(h as u32, Body::EpochStart { height: h as u32, entry_txid: txid.unwrap_or_default() }).await?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- follower

    async fn follow(self: Arc<Self>, url: String) {
        loop {
            if let Err(e) = self.follow_once(&url).await {
                tracing::warn!("follower stream from {url} ended: {e}");
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    async fn follow_once(&self, url: &str) -> Result<()> {
        let from = self.head.lock().await.seq + 1;
        let res = self.http.get(format!("{url}/log?from={from}")).header("accept", "text/event-stream").send().await?.error_for_status()?;
        let mut stream = res.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
            while let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
                let frame = buf.drain(..pos + 2).collect::<Vec<u8>>();
                let text = String::from_utf8_lossy(&frame);
                let data: String = text.lines().filter_map(|l| l.strip_prefix("data:")).map(|l| l.trim()).collect();
                if data.is_empty() {
                    continue;
                }
                let bytes = base64::engine::general_purpose::STANDARD.decode(data)?;
                self.ingest_with_patience(url, &bytes).await?;
            }
        }
        Ok(())
    }

    /// Ingest, waiting for our own Bitcoin view to catch up if the entry is
    /// for a height we have not decided yet, and resyncing on a fork.
    async fn ingest_with_patience(&self, url: &str, bytes: &[u8]) -> Result<()> {
        for attempt in 0..24 {
            match self.ingest(bytes).await {
                Ok(_) => return Ok(()),
                Err(e) if e.to_string().contains(EPOCH_UNKNOWN) => {
                    if attempt == 0 {
                        tracing::info!("waiting for Bitcoin to catch up: {e}");
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Err(e) if e.to_string().contains("prev mismatch") || e.to_string().contains("expected seq") => {
                    tracing::warn!("fork against leader: {e}; resyncing");
                    self.resync(url).await?;
                    return self.ingest(bytes).await.map(|_| ()).or_else(|e| if e.to_string().contains("expected seq") { Ok(()) } else { Err(e) });
                }
                Err(e) => {
                    self.set_role(Role::Stalled { reason: e.to_string() }).await;
                    return Err(e);
                }
            }
        }
        bail!("gave up waiting for the epoch of an incoming entry")
    }

    // ---------------------------------------------------------------- leader upkeep

    async fn settle_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(self.cfg.settle_secs)).await;
            if self.role().await != Role::Leader {
                continue;
            }
            if let Err(e) = self.settle_once().await {
                tracing::warn!("settlement pass failed: {e}");
            }
        }
    }

    async fn settle_once(&self) -> Result<()> {
        let height = self.height();
        let open = sqlx::query_as::<_, db::Match>("select * from matches where status = 'open' order by seq").fetch_all(&self.db).await?;
        for m in open {
            if let Err(e) = self.settle_match(&m, height).await {
                tracing::warn!("match {}: {e}", hex::encode(&m.id));
            }
        }
        Ok(())
    }

    async fn settle_match(&self, m: &db::Match, height: u32) -> Result<()> {
        let mid: [u8; 32] = m.id.clone().try_into().map_err(|_| anyhow!("bad match id"))?;
        let taker_params = chain_params(&m.taker_chain);
        let maker_params = chain_params(&m.maker_chain);
        let taker_deadline = m.claimed_height as u32 + taker_params.taker_deadline;

        let Some(taker_txid) = m.taker_txid.clone() else {
            if now_ms() >= m.claimed_ts as u64 + apply::CLAIM_WINDOW_MS {
                self.append(Body::ClaimExpired { match_id: mid }).await?;
            }
            return Ok(());
        };

        if m.taker_included_height.is_none() {
            let asset = chain::asset(&m.taker_asset).ok_or_else(|| anyhow!("unknown asset"))?;
            match self.chains.check_payment(asset, &taker_txid, &m.taker_pay_to, m.taker_amount as u64).await? {
                PaymentStatus::Included { block_hash, block_height, .. } => {
                    self.append(Body::LegIncluded { match_id: mid, leg: 0, txid: taker_txid, block_hash, block_height }).await?;
                }
                PaymentStatus::Reorged => {
                    self.append(Body::ClaimVoid { match_id: mid, reason: 2 }).await?;
                }
                st => {
                    if height >= taker_deadline {
                        tracing::info!("match {}: taker payment {st:?} at deadline", hex::encode(&m.id));
                        self.append(Body::ClaimVoid { match_id: mid, reason: 1 }).await?;
                    }
                }
            }
            return Ok(());
        }

        let inc = m.taker_included_height.unwrap_or(0) as u32;
        let maker_deadline = inc + maker_params.maker_deadline + GRACE_BLOCKS;
        let Some(maker_txid) = m.maker_txid.clone() else {
            if height >= maker_deadline {
                self.append(Body::LegMissed { match_id: mid, leg: 1 }).await?;
            }
            return Ok(());
        };

        if m.maker_included_height.is_none() {
            let asset = chain::asset(&m.maker_asset).ok_or_else(|| anyhow!("unknown asset"))?;
            match self.chains.check_payment(asset, &maker_txid, &m.maker_pay_to, m.maker_amount as u64).await? {
                PaymentStatus::Included { block_hash, block_height, .. } => {
                    self.append(Body::LegIncluded { match_id: mid, leg: 1, txid: maker_txid, block_hash, block_height }).await?;
                }
                _ if height >= maker_deadline => {
                    self.append(Body::LegMissed { match_id: mid, leg: 1 }).await?;
                }
                _ => {}
            }
            return Ok(());
        }

        let (bh, bheight) = (m.maker_block_hash.clone().unwrap_or_default(), m.maker_block_height.unwrap_or(0) as u64);
        if self.chains.is_deep(&m.maker_chain, &maker_txid, &bh, bheight).await? {
            self.append(Body::MatchClosed { match_id: mid }).await?;
        } else if height >= maker_deadline {
            self.append(Body::LegMissed { match_id: mid, leg: 1 }).await?;
        }
        Ok(())
    }
}

pub async fn run(cfg: RunArgs) -> Result<()> {
    let kp = Keypair::from_hex(&cfg.key)?;
    let genesis_leader = crypto::pubkey_from_hex(&cfg.genesis_leader)?;
    let pool = db::connect(&cfg.database_url).await?;
    let (seq, hash, time) = db::head(&pool).await?.unwrap_or((0, fbio::ZERO32, 0));
    let (log_tx, _) = broadcast::channel(1024);
    let (event_tx, _) = broadcast::channel(1024);
    let (epoch_tx, _) = watch::channel(None);
    let node = Arc::new(Node {
        chains: Chains::new(&cfg.bitcoin_rpc, &cfg.eth_rpc, &cfg.esplora),
        http: reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?,
        head: Mutex::new(Head { seq, hash, time }),
        role: RwLock::new(Role::Starting),
        started: now_ms(),
        cfg: cfg.clone(),
        db: pool,
        kp,
        genesis_leader,
        log_tx,
        event_tx,
        epoch_tx,
    });
    tracing::info!(
        pubkey = hex::encode(node.kp.pubkey()),
        key_hash = hex::encode(node.my_key_hash()),
        head = seq,
        genesis_height = cfg.genesis_height,
        "bollard node"
    );
    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    tracing::info!("listening on {}", cfg.listen);
    tokio::spawn(bitcoin_watch::run(node.clone()));
    tokio::spawn(node.clone().roles_loop());
    tokio::spawn(node.clone().settle_loop());
    axum::serve(listener, api::router(node)).await?;
    Ok(())
}
