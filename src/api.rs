use crate::chain;
use crate::db;
use crate::node::Node;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use futures::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::CorsLayer;

type S = State<Arc<Node>>;

pub fn router(node: Arc<Node>) -> Router {
    let router = Router::new()
        .route("/", get(index))
        .route("/status", get(status))
        .route("/markets", get(markets))
        .route("/book", get(book))
        .route("/account/{pubkey}", get(account))
        .route("/orders", get(orders))
        .route("/matches", get(matches))
        .route("/trades", get(trades))
        .route("/find-payment", get(find_payment))
        .route("/balances/{account}", get(balances))
        .route("/activity/{account}", get(activity))
        .route("/issuers", get(issuers))
        .route("/oracle", get(oracle))
        .route("/keys", post(register_key))
        .route("/keys/{cred}", get(lookup_key))
        .route("/log", get(log_stream))
        .route("/log/range", get(log_range))
        .route("/log/seq/{seq}", get(log_by_seq))
        .route("/log/{hash}", get(log_by_hash))
        .route("/entries", get(entries))
        .route("/epochs", get(epochs))
        .route("/events", get(events_stream))
        .route("/tx", post(submit_tx));
    let router = match node.cfg.web_dir.clone() {
        Some(dir) => router.fallback_service(tower_http::services::ServeDir::new(dir)),
        None => router,
    };
    router.layer(CorsLayer::permissive()).with_state(node)
}

struct ApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, Json(json!({"error": self.0.to_string()}))).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/dashboard/index.html"))
}

async fn status(State(n): S) -> ApiResult {
    let head = n.head.lock().await;
    let (seq, hash, time) = (head.seq, head.hash, head.time);
    drop(head);
    let btc: Option<(i32, String)> = sqlx::query_as("select height, hash from btc_blocks order by height desc limit 1").fetch_optional(&n.db).await?;
    let (entries, accounts, orders, matches): (i64, i64, i64, i64) = (
        sqlx::query_scalar("select count(*) from entries").fetch_one(&n.db).await?,
        sqlx::query_scalar("select count(*) from accounts").fetch_one(&n.db).await?,
        sqlx::query_scalar("select count(*) from orders where status = 'open'").fetch_one(&n.db).await?,
        sqlx::query_scalar("select count(*) from matches where status = 'open'").fetch_one(&n.db).await?,
    );
    Ok(Json(json!({
        "network": "bollard",
        "version": env!("CARGO_PKG_VERSION"),
        "role": n.role().await,
        "node": { "pubkey": hex::encode(n.kp.pubkey()), "key_hash": hex::encode(n.my_key_hash()), "up_since": n.started },
        "head": { "seq": seq, "hash": hex::encode(hash), "time": time },
        "epoch": n.epoch(),
        "bitcoin": btc.map(|(h, hash)| json!({"height": h, "hash": hash})),
        "genesis_height": n.cfg.genesis_height,
        "counts": { "entries": entries, "accounts": accounts, "open_orders": orders, "open_matches": matches },
    })))
}

async fn markets(State(n): S) -> ApiResult {
    let active: Vec<(String, i64)> = sqlx::query_as("select market, count(*) from orders where status = 'open' group by market order by market").fetch_all(&n.db).await?;
    Ok(Json(json!({
        "assets": chain::ASSETS,
        "chains": chain::CHAINS,
        "price_scale": crate::apply::PRICE_SCALE,
        "fees": { "transfer": crate::apply::TRANSFER_FEE, "match": crate::apply::MATCH_FEE },
        "claim_window_ms": crate::apply::CLAIM_WINDOW_MS,
        "active": active.iter().map(|(m, c)| json!({"market": m, "open_orders": c})).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct BookQuery {
    market: String,
}

async fn book(State(n): S, Query(q): Query<BookQuery>) -> ApiResult {
    chain::parse_market(&q.market)?;
    let bids = sqlx::query_as::<_, db::Order>("select * from orders where market = $1 and status = 'open' and side = 0 and settle = 0 order by price desc, seq asc limit 200").bind(&q.market).fetch_all(&n.db).await?;
    let asks = sqlx::query_as::<_, db::Order>("select * from orders where market = $1 and status = 'open' and side = 1 and settle = 0 order by price asc, seq asc limit 200").bind(&q.market).fetch_all(&n.db).await?;
    let deposits = sqlx::query_as::<_, db::Order>("select * from orders where market = $1 and status = 'open' and settle = 1 order by price asc, seq asc limit 200").bind(&q.market).fetch_all(&n.db).await?;
    // A minting order can only sell what the maker's stake (plus free BLRD) covers right now.
    let height = n.height();
    let mut c = n.db.acquire().await?;
    let mut deposits: Vec<Value> = deposits.into_iter().map(|o| json!(o)).collect();
    for d in deposits.iter_mut() {
        if d["mint"].as_bool() == Some(true) {
            let acct = hex::decode(d["account"].as_str().unwrap_or("")).unwrap_or_default();
            let base = d["base"].as_str().unwrap_or("").to_string();
            let cap = crate::apply::mint_capacity(&mut c, &acct, height).await.unwrap_or(0);
            let price = crate::apply::oracle_price(&mut c, &base, height).await.ok().flatten().unwrap_or(0) as i128;
            let qty_cap = if price > 0 { cap * crate::apply::PRICE_SCALE as i128 / price } else { 0 };
            let left = d["qty_left"].as_i64().unwrap_or(0) as i128;
            d["qty_left"] = json!(left.min(qty_cap) as i64);
        }
    }
    Ok(Json(json!({"market": q.market, "bids": bids, "asks": asks, "deposits": deposits})))
}

async fn account(State(n): S, Path(pubkey): Path<String>) -> ApiResult {
    let pk = hex::decode(&pubkey)?;
    let a = sqlx::query_as::<_, db::Account>("select * from accounts where pubkey = $1").bind(&pk).fetch_optional(&n.db).await?;
    Ok(Json(match a {
        Some(a) => json!(a),
        None => json!({"pubkey": pubkey, "nonce": 0, "balance": 0, "locked": 0, "preferred": null, "staked": 0, "unstaking": 0, "unstake_height": 0}),
    }))
}

/// Everything an account holds: BLRD plus option balances summed per asset,
/// with the issuer lots behind them.
async fn balances(State(n): S, Path(pubkey): Path<String>) -> ApiResult {
    let pk = hex::decode(&pubkey)?;
    let a = sqlx::query_as::<_, db::Account>("select * from accounts where pubkey = $1").bind(&pk).fetch_optional(&n.db).await?;
    let lots = sqlx::query_as::<_, db::Balance>("select * from balances where account = $1 and amount > 0 order by asset, issuer").bind(&pk).fetch_all(&n.db).await?;
    let mut totals: Vec<(String, i64)> = Vec::new();
    for l in &lots {
        match totals.iter_mut().find(|(a, _)| a == &l.asset) {
            Some(t) => t.1 += l.amount,
            None => totals.push((l.asset.clone(), l.amount)),
        }
    }
    let issuing = sqlx::query_as::<_, db::Issuer>("select * from issuers where account = $1 and issued > 0").bind(&pk).fetch_all(&n.db).await?;
    Ok(Json(json!({
        "account": pubkey,
        "preferred": a.as_ref().and_then(|a| a.preferred.clone()),
        "blrd": a.as_ref().map(|a| a.balance).unwrap_or(0),
        "blrd_locked": a.as_ref().map(|a| a.locked).unwrap_or(0),
        "blrd_staked": a.as_ref().map(|a| a.staked).unwrap_or(0),
        "blrd_unstaking": a.as_ref().map(|a| a.unstaking).unwrap_or(0),
        "unstake_height": a.as_ref().map(|a| a.unstake_height).unwrap_or(0),
        "nonce": a.as_ref().map(|a| a.nonce).unwrap_or(0),
        "assets": totals.iter().map(|(a, t)| json!({"asset": a, "amount": t})).collect::<Vec<_>>(),
        "lots": lots,
        "issuing": issuing,
    })))
}

#[derive(sqlx::FromRow, serde::Serialize)]
struct TransferRow {
    seq: i64,
    ts: i64,
    #[serde(serialize_with = "crate::fbio::hex_bytes")]
    sender: Vec<u8>,
    #[serde(serialize_with = "crate::fbio::hex_bytes")]
    receiver: Vec<u8>,
    asset: String,
    amount: i64,
    delivered_asset: String,
    delivered: i64,
}

/// What happened to an account: sends in and out, deposits, exercises, trades.
async fn activity(State(n): S, Path(pubkey): Path<String>) -> ApiResult {
    let pk = hex::decode(&pubkey)?;
    let transfers = sqlx::query_as::<_, TransferRow>("select * from transfers where sender = $1 or receiver = $1 order by seq desc limit 200").bind(&pk).fetch_all(&n.db).await?;
    let matches = sqlx::query_as::<_, db::Match>("select * from matches where maker = $1 or taker = $1 order by seq desc limit 200").bind(&pk).fetch_all(&n.db).await?;
    Ok(Json(json!({"transfers": transfers, "matches": matches})))
}

/// Prices the ledger uses for collateral: trade averages from its own books, the
/// required stake ratio, and every issuer's standing against it.
async fn oracle(State(n): S) -> ApiResult {
    let height = n.height();
    let mut c = n.db.acquire().await?;
    let mut prices = serde_json::Map::new();
    for a in chain::ASSETS {
        prices.insert(a.id.to_string(), json!(crate::apply::oracle_price(&mut c, a.id, height).await?));
    }
    let need = crate::apply::required_ratio_bps(&mut c, height).await?;
    let issuers: Vec<(Vec<u8>,)> = sqlx::query_as("select distinct account from issuers where issued > 0").fetch_all(&mut *c).await?;
    let mut standing = Vec::new();
    for (acct,) in issuers {
        let issued = crate::apply::issued_value(&mut c, &acct, height).await.unwrap_or(0);
        let stake = crate::apply::stake_value(&mut c, &acct, height).await?;
        standing.push(json!({"account": hex::encode(&acct), "issued_usdc": issued as i64, "stake_usdc": stake as i64, "ratio_bps": if issued > 0 { (stake * 10_000 / issued) as i64 } else { -1 }, "ok": issued == 0 || stake * 10_000 >= issued * need}));
    }
    Ok(Json(json!({"height": height, "prices": prices, "required_ratio_bps": need as i64, "base_ratio_bps": crate::apply::BASE_RATIO_BPS as i64, "liquidation_premium_bps": crate::apply::LIQUIDATION_PREMIUM_BPS as i64, "issuers": standing})))
}

async fn issuers(State(n): S) -> ApiResult {
    let rows: Vec<(Vec<u8>, String, i64, i64, i64, String)> = sqlx::query_as("select i.account, i.asset, i.issued, a.staked, a.unstaking, i.source from issuers i join accounts a on a.pubkey = i.account where i.issued > 0 order by i.asset, a.staked desc").fetch_all(&n.db).await?;
    let height = n.height();
    let mut c = n.db.acquire().await?;
    let mut out = Vec::new();
    for (acct, asset, issued, staked, unstaking, source) in rows {
        let iv = crate::apply::issued_value(&mut c, &acct, height).await.unwrap_or(0);
        let sv = crate::apply::stake_value(&mut c, &acct, height).await.unwrap_or(0);
        out.push(json!({"account": hex::encode(&acct), "asset": asset, "issued": issued, "staked": staked, "unstaking": unstaking, "source": source, "ratio_bps": if iv > 0 { (sv * 10_000 / iv) as i64 } else { -1 }}));
    }
    let rows = out;
    Ok(Json(json!(rows)))
}

#[derive(Deserialize)]
struct KeyReg {
    cred_id: String,
    pubkey: String,
}

/// Remember which passkey credential belongs to which account, so a new
/// device can sign in by credential id. Anyone can register; the key must be a
/// valid P-256 point and the first registration of a credential id wins.
async fn register_key(State(n): S, Json(k): Json<KeyReg>) -> ApiResult {
    let pk = hex::decode(&k.pubkey)?;
    if pk.len() != 65 || !crate::crypto::valid_account(&pk) {
        return Err(anyhow::anyhow!("pubkey must be a 65-byte P-256 key").into());
    }
    if k.cred_id.is_empty() || k.cred_id.len() > 512 {
        return Err(anyhow::anyhow!("bad credential id").into());
    }
    sqlx::query("insert into passkeys (cred_id, pubkey, created) values ($1, $2, $3) on conflict do nothing").bind(&k.cred_id).bind(&pk).bind(crate::node::now_ms() as i64).execute(&n.db).await?;
    let stored: (Vec<u8>,) = sqlx::query_as("select pubkey from passkeys where cred_id = $1").bind(&k.cred_id).fetch_one(&n.db).await?;
    Ok(Json(json!({"cred_id": k.cred_id, "pubkey": hex::encode(stored.0)})))
}

async fn lookup_key(State(n): S, Path(cred): Path<String>) -> Result<Response, ApiError> {
    let row: Option<(Vec<u8>,)> = sqlx::query_as("select pubkey from passkeys where cred_id = $1").bind(&cred).fetch_optional(&n.db).await?;
    Ok(match row {
        Some((pk,)) => Json(json!({"cred_id": cred, "pubkey": hex::encode(pk)})).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "unknown passkey"}))).into_response(),
    })
}

#[derive(Deserialize)]
struct ListQuery {
    account: Option<String>,
    market: Option<String>,
    status: Option<String>,
    kind: Option<String>,
    limit: Option<i64>,
}

async fn orders(State(n): S, Query(q): Query<ListQuery>) -> ApiResult {
    let acct = q.account.as_deref().map(hex::decode).transpose()?;
    let rows = sqlx::query_as::<_, db::Order>(
        "select * from orders where ($1::bytea is null or account = $1) and ($2::text is null or market = $2) and ($3::text is null or status = $3) order by seq desc limit $4",
    )
    .bind(acct)
    .bind(q.market)
    .bind(q.status)
    .bind(q.limit.unwrap_or(100).clamp(1, 1000))
    .fetch_all(&n.db)
    .await?;
    Ok(Json(json!(rows)))
}

async fn matches(State(n): S, Query(q): Query<ListQuery>) -> ApiResult {
    let acct = q.account.as_deref().map(hex::decode).transpose()?;
    let rows = sqlx::query_as::<_, db::Match>(
        "select * from matches where ($1::bytea is null or maker = $1 or taker = $1) and ($2::text is null or market = $2) and ($3::text is null or status = $3) and ($5::text is null or kind = $5) order by seq desc limit $4",
    )
    .bind(acct)
    .bind(q.market)
    .bind(q.status)
    .bind(q.limit.unwrap_or(100).clamp(1, 1000))
    .bind(q.kind)
    .fetch_all(&n.db)
    .await?;
    Ok(Json(json!(rows)))
}

/// Trades for a chart: every claim on a market, oldest first, with its price in
/// quote atoms per 1e8 base atoms and the leader time it was claimed at.
async fn trades(State(n): S, Query(q): Query<ListQuery>) -> ApiResult {
    let Some(market) = q.market else { return Err(anyhow::anyhow!("market is required").into()) };
    let rows: Vec<(Vec<u8>, i64, i64, i64, i16, String)> = sqlx::query_as(
        "select id, price, quantity, claimed_ts, side, status from matches where market = $1 order by seq desc limit $2",
    )
    .bind(&market)
    .bind(q.limit.unwrap_or(2000).clamp(1, 10000))
    .fetch_all(&n.db)
    .await?;
    Ok(Json(json!(rows
        .iter()
        .rev()
        .map(|(id, price, qty, ts, side, status)| json!({"match": hex::encode(id), "price": price, "quantity": qty, "time": ts, "side": side, "status": status}))
        .collect::<Vec<_>>())))
}

#[derive(Deserialize)]
struct FindQuery {
    #[serde(rename = "match")]
    match_id: String,
    leg: Option<u8>,
}

/// Look on the chain for the payment a leg is waiting for. A convenience for
/// wallets so nobody has to paste a txid; the report itself is still a signed
/// transaction from the payer (or anyone, for the maker's leg).
async fn find_payment(State(n): S, Query(q): Query<FindQuery>) -> ApiResult {
    let m = n.get_match(&hex::decode(&q.match_id)?).await?;
    let leg = q.leg.unwrap_or(0);
    let (asset, to, amount, already) = if leg == 0 { (m.taker_asset, m.taker_pay_to, m.taker_amount, m.taker_txid) } else { (m.maker_asset, m.maker_pay_to, m.maker_amount, m.maker_txid) };
    if let Some(t) = already {
        return Ok(Json(json!({"txid": t, "reported": true})));
    }
    let asset = chain::asset(&asset).ok_or_else(|| anyhow::anyhow!("unknown asset"))?;
    if asset.chain == "blrd" {
        return Ok(Json(json!({"txid": null, "ledger": true})));
    }
    let txid = n.chains.find_payment(asset, &to, amount as u64, m.claimed_ts as u64).await?;
    Ok(Json(json!({"txid": txid, "reported": false})))
}

fn entry_json(bytes: &[u8]) -> Value {
    match crate::fbio::parse_entry(bytes) {
        Ok(e) => {
            let mut v = json!(e);
            v["kind"] = json!(e.kind());
            v["bytes"] = json!(base64::engine::general_purpose::STANDARD.encode(bytes));
            v
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn log_by_seq(State(n): S, Path(seq): Path<i64>) -> Result<Response, ApiError> {
    let row: Option<(Vec<u8>,)> = sqlx::query_as("select bytes from log where seq = $1").bind(seq).fetch_optional(&n.db).await?;
    Ok(match row {
        Some((b,)) => Json(entry_json(&b)).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "no such entry"}))).into_response(),
    })
}

async fn log_by_hash(State(n): S, Path(hash): Path<String>) -> Result<Response, ApiError> {
    let h = hex::decode(&hash)?;
    let row: Option<(Vec<u8>,)> = sqlx::query_as("select bytes from log where hash = $1").bind(&h).fetch_optional(&n.db).await?;
    Ok(match row {
        Some((b,)) => Json(entry_json(&b)).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "no such entry"}))).into_response(),
    })
}

#[derive(Deserialize)]
struct RangeQuery {
    from: Option<i64>,
    to: Option<i64>,
}

async fn log_range(State(n): S, Query(q): Query<RangeQuery>) -> ApiResult {
    let from = q.from.unwrap_or(1).max(1);
    let to = q.to.unwrap_or(from + 99).min(from + 999);
    let rows = sqlx::query_as::<_, db::LogRow>("select seq, hash, bytes from log where seq between $1 and $2 order by seq").bind(from).bind(to).fetch_all(&n.db).await?;
    Ok(Json(json!(rows
        .iter()
        .map(|r| json!({"seq": r.seq, "hash": hex::encode(&r.hash), "bytes": base64::engine::general_purpose::STANDARD.encode(&r.bytes)}))
        .collect::<Vec<_>>())))
}

async fn entries(State(n): S) -> ApiResult {
    let rows = sqlx::query_as::<_, db::LotteryEntry>("select * from entries order by height desc, rank asc nulls last limit 200").fetch_all(&n.db).await?;
    Ok(Json(json!(rows)))
}

async fn epochs(State(n): S) -> ApiResult {
    let rows = sqlx::query_as::<_, db::Epoch>("select * from epochs where entry_txid is not null or height = (select min(height) from epochs) order by height desc limit 200").fetch_all(&n.db).await?;
    Ok(Json(json!(rows)))
}

#[derive(Deserialize)]
struct FromQuery {
    from: Option<i64>,
}

/// The log as server-sent events. Each event's id is the entry's seq and its
/// data is the entry's bytes in base64, so a client resumes with Last-Event-ID.
async fn log_stream(State(n): S, Query(q): Query<FromQuery>, headers: axum::http::HeaderMap) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let last_id = headers.get("last-event-id").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok());
    let from = last_id.map(|l| l + 1).or(q.from).unwrap_or(1).max(1);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(256);
    let node = n.clone();
    tokio::spawn(async move {
        let mut live = node.log_tx.subscribe();
        let mut next = from;
        loop {
            let rows = sqlx::query_as::<_, db::LogRow>("select seq, hash, bytes from log where seq >= $1 order by seq limit 500").bind(next).fetch_all(&node.db).await.unwrap_or_default();
            if rows.is_empty() {
                break;
            }
            for r in rows {
                let ev = Event::default().id(r.seq.to_string()).data(base64::engine::general_purpose::STANDARD.encode(&r.bytes));
                if tx.send(Ok(ev)).await.is_err() {
                    return;
                }
                next = r.seq + 1;
            }
        }
        loop {
            match live.recv().await {
                Ok(m) => {
                    if (m.seq as i64) < next {
                        continue;
                    }
                    let ev = Event::default().id(m.seq.to_string()).data(base64::engine::general_purpose::STANDARD.encode(&m.bytes));
                    if tx.send(Ok(ev)).await.is_err() {
                        return;
                    }
                    next = m.seq as i64 + 1;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            }
        }
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

/// Human-readable events (orders, claims, settlement, epochs) as JSON.
async fn events_stream(State(n): S) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(256);
    let mut live = n.event_tx.subscribe();
    tokio::spawn(async move {
        loop {
            match live.recv().await {
                Ok(s) => {
                    if tx.send(Ok(Event::default().data(s))).await.is_err() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            }
        }
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

/// Submit a signed transaction: the SignedTx bytes, raw or as {"tx": base64}.
async fn submit_tx(State(n): S, headers: axum::http::HeaderMap, body: Bytes) -> ApiResult {
    let is_json = headers.get("content-type").and_then(|v| v.to_str().ok()).map(|c| c.contains("json")).unwrap_or(false);
    let bytes = if is_json {
        let v: Value = serde_json::from_slice(&body)?;
        let b64 = v.get("tx").and_then(|t| t.as_str()).ok_or_else(|| anyhow::anyhow!("expected {{\"tx\": base64}}"))?;
        base64::engine::general_purpose::STANDARD.decode(b64)?
    } else {
        body.to_vec()
    };
    Ok(Json(n.submit_tx(&bytes).await?))
}
