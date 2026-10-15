//! The state transition. `apply` is the only code that changes accounts,
//! orders and matches, it runs inside the same database transaction that
//! stores the entry, and it uses nothing but the entry and prior state, so
//! every node ends up with identical tables.
//!
//! Leader assertions (LegIncluded, MatchClosed, ...) are checked here for
//! consistency with state; followers check them against the chains in
//! `node::verify_assertion` before calling this.

use crate::chain::{self, chain_params, GRACE_BLOCKS};
use crate::fbio::{Body, ParsedEntry, TxBody};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use sqlx::PgConnection;

pub const PRICE_SCALE: u128 = 100_000_000;
pub const TRANSFER_FEE: u64 = 10_000; // 0.0001 BLRD
pub const MATCH_FEE: u64 = 100_000; // 0.001 BLRD
pub const CLAIM_WINDOW_MS: u64 = 180_000;

pub fn subsidy(height: u32) -> u64 {
    let halvings = height / 210_000;
    if halvings >= 64 {
        0
    } else {
        (50u64 * 100_000_000) >> halvings
    }
}

fn i(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| anyhow!("amount too large"))
}

async fn account(c: &mut PgConnection, pk: &[u8]) -> Result<(i64, i64, i64)> {
    sqlx::query("insert into accounts (pubkey) values ($1) on conflict do nothing").bind(pk).execute(&mut *c).await?;
    let row: (i64, i64, i64) = sqlx::query_as("select nonce, balance, locked from accounts where pubkey = $1").bind(pk).fetch_one(&mut *c).await?;
    Ok(row)
}

async fn credit(c: &mut PgConnection, pk: &[u8], amount: i64) -> Result<()> {
    sqlx::query("insert into accounts (pubkey, balance) values ($1, $2) on conflict (pubkey) do update set balance = accounts.balance + $2")
        .bind(pk)
        .bind(amount)
        .execute(c)
        .await?;
    Ok(())
}

/// Move `amount` from balance to locked, failing if the balance is short.
async fn lock(c: &mut PgConnection, pk: &[u8], amount: i64) -> Result<()> {
    let (_, bal, _) = account(c, pk).await?;
    if bal < amount {
        bail!("insufficient balance: have {bal} rill, need {amount}");
    }
    sqlx::query("update accounts set balance = balance - $2, locked = locked + $2 where pubkey = $1").bind(pk).bind(amount).execute(c).await?;
    Ok(())
}

/// Move `amount` out of `from`'s locked funds into `to`'s balance.
async fn unlock_to(c: &mut PgConnection, from: &[u8], to: &[u8], amount: i64) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    sqlx::query("update accounts set locked = locked - $2 where pubkey = $1").bind(from).bind(amount).execute(&mut *c).await?;
    credit(c, to, amount).await
}

async fn bump_nonce(c: &mut PgConnection, pk: &[u8], expected: u64) -> Result<()> {
    let (nonce, _, _) = account(c, pk).await?;
    if nonce as u64 != expected {
        bail!("bad nonce: expected {nonce}, got {expected}");
    }
    sqlx::query("update accounts set nonce = nonce + 1 where pubkey = $1").bind(pk).execute(c).await?;
    Ok(())
}

async fn get_order(c: &mut PgConnection, id: &[u8]) -> Result<crate::db::Order> {
    sqlx::query_as::<_, crate::db::Order>("select * from orders where id = $1").bind(id).fetch_optional(c).await?.ok_or_else(|| anyhow!("unknown order"))
}

async fn get_match(c: &mut PgConnection, id: &[u8]) -> Result<crate::db::Match> {
    sqlx::query_as::<_, crate::db::Match>("select * from matches where id = $1").bind(id).fetch_optional(c).await?.ok_or_else(|| anyhow!("unknown match"))
}

/// Give a claim's quantity and bond back to its order, or to the maker's
/// balance if the order is no longer open.
async fn restore_to_order(c: &mut PgConnection, m: &crate::db::Match) -> Result<()> {
    let o = get_order(c, &m.order_id).await?;
    let (qty, bond) = if o.base == "BLRD" { (m.quantity, m.quantity) } else { (m.quantity, m.bond) };
    if o.status == "open" || o.status == "filled" {
        sqlx::query("update orders set qty_left = qty_left + $2, bond_left = bond_left + $3, status = 'open' where id = $1")
            .bind(&o.id)
            .bind(qty)
            .bind(bond)
            .execute(c)
            .await?;
    } else {
        unlock_to(c, &m.maker, &m.maker, bond).await?;
    }
    Ok(())
}

async fn close_match(c: &mut PgConnection, id: &[u8], status: &str, reason: &str, seq: u64) -> Result<()> {
    sqlx::query("update matches set status = $2, closed_reason = $3, closed_seq = $4 where id = $1")
        .bind(id)
        .bind(status)
        .bind(reason)
        .bind(seq as i64)
        .execute(c)
        .await?;
    Ok(())
}

fn ev(name: &str, v: Value) -> Value {
    let mut v = v;
    v["event"] = json!(name);
    v
}

/// Apply one entry to the state. Returns events for live listeners.
pub async fn apply(c: &mut PgConnection, e: &ParsedEntry) -> Result<Vec<Value>> {
    let seq = e.seq;
    let mut events = Vec::new();
    // Orders cancel themselves at their expiry height, whatever entry brings that height.
    let expired: Vec<(Vec<u8>, Vec<u8>, i64, String)> = sqlx::query_as(
        "update orders set status = 'expired', qty_left = 0, bond_left = 0 where status = 'open' and expiry_height <= $1 returning id, account, case when base = 'BLRD' then qty_left else bond_left end, market",
    )
    .bind(e.height as i32)
    .fetch_all(&mut *c)
    .await?;
    for (id, acct, release, market) in expired {
        unlock_to(c, &acct, &acct, release).await?;
        events.push(ev("OrderExpired", json!({"order": hex::encode(id), "market": market})));
    }
    match &e.body {
        Body::EpochStart { height, entry_txid } => {
            // Mint the block's reward to the leader and expire orders that are due.
            let reward = subsidy(*height);
            if reward > 0 {
                credit(c, &e.leader, i(reward)?).await?;
            }
            sqlx::query("insert into epochs (height, leader_key_hash, leader_key, entry_txid, checkpoint_tip, first_seq) values ($1, $2, $3, nullif($4, ''), $5, $6) on conflict (height) do update set leader_key = excluded.leader_key, first_seq = coalesce(epochs.first_seq, excluded.first_seq)")
                .bind(*height as i32)
                .bind(crate::crypto::hash160(&e.leader).to_vec())
                .bind(e.leader.to_vec())
                .bind(entry_txid)
                .bind(e.prev.to_vec())
                .bind(seq as i64)
                .execute(&mut *c)
                .await?;
            sqlx::query("update epochs set last_seq = $2 where height < $1 and last_seq is null and first_seq is not null")
                .bind(*height as i32)
                .bind(seq as i64 - 1)
                .execute(&mut *c)
                .await?;
            events.push(ev("EpochStart", json!({"height": height, "leader": hex::encode(e.leader), "reward": reward})));
        }

        Body::Tx(tx) => {
            let acct = tx.account.to_vec();
            bump_nonce(c, &acct, tx.nonce).await?;
            match &tx.body {
                TxBody::Transfer { to, amount } => {
                    if to.len() != 33 {
                        bail!("recipient must be a 33-byte public key");
                    }
                    let (_, bal, _) = account(c, &acct).await?;
                    let total = i(amount.checked_add(TRANSFER_FEE).ok_or_else(|| anyhow!("overflow"))?)?;
                    if bal < total {
                        bail!("insufficient balance: have {bal} rill, need {total}");
                    }
                    sqlx::query("update accounts set balance = balance - $2 where pubkey = $1").bind(&acct).bind(total).execute(&mut *c).await?;
                    credit(c, to, i(*amount)?).await?;
                    credit(c, &e.leader, i(TRANSFER_FEE)?).await?;
                    events.push(ev("Transfer", json!({"from": hex::encode(&acct), "to": hex::encode(to), "amount": amount})));
                }

                TxBody::PlaceOrder { market, side, quantity, price, bond, claim_bond, fill_policy, receive_address, expiry_height } => {
                    let (base, quote) = chain::parse_market(market)?;
                    if *side > 1 {
                        bail!("side must be 0 (buy) or 1 (sell)");
                    }
                    if *fill_policy > 2 {
                        bail!("fill_policy must be 0 (seen), 1 (included) or 2 (final)");
                    }
                    if *quantity < base.min_quantity {
                        bail!("quantity below the minimum of {} {}", base.min_quantity, base.id);
                    }
                    if *price == 0 {
                        bail!("price must be positive");
                    }
                    if *expiry_height <= e.height {
                        bail!("expiry_height must be after the current height {}", e.height);
                    }
                    if quote.id == "BLRD" || (base.id == "BLRD" && *side == 0) {
                        bail!("v1 supports BLRD only as the base asset of a sell order");
                    }
                    let quote_amount = *quantity as u128 * *price as u128 / PRICE_SCALE;
                    if quote_amount < quote.min_quantity as u128 {
                        bail!("quote amount below the minimum of {} {}", quote.min_quantity, quote.id);
                    }
                    i(u64::try_from(quote_amount).map_err(|_| anyhow!("quote amount too large"))?)?;
                    // What the maker pays the taker needs an address on the taker's side later;
                    // what the maker is buying arrives at receive_address, which must be given.
                    if receive_address.is_empty() {
                        bail!("receive_address is required");
                    }
                    // The maker is paid in the asset it buys: quote when selling base, base when buying.
                    let buys = if *side == 1 { quote } else { base };
                    if buys.chain != "blrd" {
                        chain::receive_address(receive_address, buys.chain, 0)?;
                    }
                    let locked = if base.id == "BLRD" { *quantity } else { *bond };
                    if locked == 0 {
                        bail!("bond must be positive");
                    }
                    lock(c, &acct, i(locked)?).await?;
                    let (bond_v, bond_left) = if base.id == "BLRD" { (0i64, 0i64) } else { (i(*bond)?, i(*bond)?) };
                    sqlx::query("insert into orders (id, seq, account, market, base, quote, side, quantity, qty_left, price, bond, bond_left, claim_bond, fill_policy, recv_addr, expiry_height, status) values ($1,$2,$3,$4,$5,$6,$7,$8,$8,$9,$10,$11,$12,$13,$14,$15,'open')")
                        .bind(e.hash.to_vec())
                        .bind(seq as i64)
                        .bind(&acct)
                        .bind(market)
                        .bind(base.id)
                        .bind(quote.id)
                        .bind(*side as i16)
                        .bind(i(*quantity)?)
                        .bind(i(*price)?)
                        .bind(bond_v)
                        .bind(bond_left)
                        .bind(i(*claim_bond)?)
                        .bind(*fill_policy as i16)
                        .bind(receive_address)
                        .bind(*expiry_height as i32)
                        .execute(&mut *c)
                        .await?;
                    events.push(ev("OrderPlaced", json!({"order": hex::encode(e.hash), "market": market, "side": side, "quantity": quantity, "price": price})));
                }

                TxBody::CancelOrder { order_id } => {
                    let o = get_order(c, order_id).await?;
                    if o.account != acct {
                        bail!("not your order");
                    }
                    if o.status != "open" {
                        bail!("order is {}", o.status);
                    }
                    let release = if o.base == "BLRD" { o.qty_left } else { o.bond_left };
                    sqlx::query("update orders set status = 'cancelled', qty_left = 0, bond_left = 0 where id = $1").bind(&o.id).execute(&mut *c).await?;
                    unlock_to(c, &acct, &acct, release).await?;
                    events.push(ev("OrderCancelled", json!({"order": hex::encode(&o.id), "market": o.market})));
                }

                TxBody::Claim { order_id, quantity, receive_address } => {
                    let o = get_order(c, order_id).await?;
                    if o.status != "open" {
                        bail!("order is {}", o.status);
                    }
                    if o.expiry_height as u32 <= e.height {
                        bail!("order has expired");
                    }
                    let (base, quote) = chain::parse_market(&o.market)?;
                    let qty = i(*quantity)?;
                    if qty > o.qty_left {
                        bail!("only {} left on this order", o.qty_left);
                    }
                    if *quantity < base.min_quantity {
                        bail!("quantity below the minimum of {} {}", base.min_quantity, base.id);
                    }
                    if receive_address.is_empty() && !(o.side == 1 && base.id == "BLRD") {
                        bail!("receive_address is required");
                    }
                    let quote_amount = u64::try_from(*quantity as u128 * o.price as u128 / PRICE_SCALE).map_err(|_| anyhow!("quote amount too large"))?;
                    if quote_amount < quote.min_quantity {
                        bail!("quote amount below the minimum of {} {}", quote.min_quantity, quote.id);
                    }
                    // Prorate the maker's bond (or escrowed BLRD) to this claim.
                    let bond = if base.id == "BLRD" { qty } else { ((o.bond_left as i128 * qty as i128) / o.qty_left as i128) as i64 };
                    if o.claim_bond > 0 {
                        lock(c, &acct, o.claim_bond).await?;
                    }
                    // Legs. side 1: the maker sells base, so the taker pays quote and the maker pays base.
                    let (taker_asset, taker_amount, maker_asset, maker_amount) = if o.side == 1 {
                        (quote, quote_amount, base, *quantity)
                    } else {
                        (base, *quantity, quote, quote_amount)
                    };
                    let left_qty = o.qty_left - qty;
                    let left_bond = if base.id == "BLRD" { left_qty } else { o.bond_left - bond };
                    let status = if left_qty == 0 { "filled" } else { "open" };
                    // A fresh address for this claim when the maker gave an xpub.
                    let taker_pay_to = if taker_asset.chain == "blrd" { o.recv_addr.clone() } else { chain::receive_address(&o.recv_addr, taker_asset.chain, o.claims as u32)? };
                    sqlx::query("update orders set claims = claims + 1 where id = $1").bind(&o.id).execute(&mut *c).await?;
                    sqlx::query("update orders set qty_left = $2, bond_left = $3, status = $4 where id = $1")
                        .bind(&o.id)
                        .bind(left_qty)
                        .bind(if base.id == "BLRD" { 0 } else { left_bond })
                        .bind(status)
                        .execute(&mut *c)
                        .await?;
                    sqlx::query("insert into matches (id, seq, order_id, maker, taker, market, side, quantity, price, bond, claim_bond, fill_policy, taker_chain, taker_asset, taker_amount, taker_pay_to, maker_chain, maker_asset, maker_amount, maker_pay_to, status, claimed_ts, claimed_height) values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,'open',$21,$22)")
                        .bind(e.hash.to_vec())
                        .bind(seq as i64)
                        .bind(&o.id)
                        .bind(&o.account)
                        .bind(&acct)
                        .bind(&o.market)
                        .bind(o.side)
                        .bind(qty)
                        .bind(o.price)
                        .bind(bond)
                        .bind(o.claim_bond)
                        .bind(o.fill_policy)
                        .bind(taker_asset.chain)
                        .bind(taker_asset.id)
                        .bind(i(taker_amount)?)
                        .bind(&taker_pay_to)
                        .bind(maker_asset.chain)
                        .bind(maker_asset.id)
                        .bind(i(maker_amount)?)
                        .bind(receive_address)
                        .bind(e.time as i64)
                        .bind(e.height as i32)
                        .execute(&mut *c)
                        .await?;
                    events.push(ev("Claimed", json!({"match": hex::encode(e.hash), "order": hex::encode(&o.id), "market": o.market, "quantity": quantity, "taker": hex::encode(&acct)})));
                }

                TxBody::ReportPayment { match_id, leg, txid } => {
                    let m = get_match(c, match_id).await?;
                    if m.status != "open" {
                        bail!("match is {}", m.status);
                    }
                    if txid.is_empty() || txid.len() > 128 {
                        bail!("bad txid");
                    }
                    let used: i64 = sqlx::query_scalar("select count(*) from matches where (taker_txid = $1 or maker_txid = $1) and id <> $2").bind(txid).bind(&m.id).fetch_one(&mut *c).await?;
                    if used > 0 {
                        bail!("that transaction already pays another match");
                    }
                    match leg {
                        0 => {
                            if m.taker != acct {
                                bail!("only the taker reports the taker's leg");
                            }
                            if m.taker_txid.is_some() {
                                bail!("taker payment already reported");
                            }
                            sqlx::query("update matches set taker_txid = $2 where id = $1").bind(&m.id).bind(txid).execute(&mut *c).await?;
                        }
                        1 => {
                            if m.maker_chain == "blrd" {
                                bail!("the maker's leg settles on the ledger; nothing to report");
                            }
                            if m.taker_txid.is_none() {
                                bail!("the taker has not paid yet");
                            }
                            if m.maker_txid.is_some() {
                                bail!("maker payment already reported");
                            }
                            sqlx::query("update matches set maker_txid = $2, maker_payer = $3 where id = $1").bind(&m.id).bind(txid).bind(&acct).execute(&mut *c).await?;
                        }
                        _ => bail!("leg must be 0 (taker) or 1 (maker)"),
                    }
                    events.push(ev("PaymentReported", json!({"match": hex::encode(&m.id), "leg": leg, "txid": txid})));
                }
            }
        }

        Body::ClaimExpired { match_id } => {
            let m = get_match(c, match_id).await?;
            if m.status != "open" || m.taker_txid.is_some() {
                bail!("claim cannot expire: status {} with a reported payment", m.status);
            }
            if e.time < m.claimed_ts as u64 + CLAIM_WINDOW_MS {
                bail!("claim window has not passed");
            }
            restore_to_order(c, &m).await?;
            if m.claim_bond > 0 {
                unlock_to(c, &m.taker, &m.maker, m.claim_bond).await?;
            }
            close_match(c, &m.id, "expired", "no payment reported", seq).await?;
            events.push(ev("ClaimExpired", json!({"match": hex::encode(&m.id), "order": hex::encode(&m.order_id)})));
        }

        Body::LegIncluded { match_id, leg, txid, block_hash, block_height } => {
            let m = get_match(c, match_id).await?;
            if m.status != "open" {
                bail!("match is {}", m.status);
            }
            match leg {
                0 => {
                    if m.taker_txid.as_deref() != Some(txid.as_str()) {
                        bail!("txid does not match the reported taker payment");
                    }
                    if m.taker_included_height.is_some() {
                        bail!("taker leg already included");
                    }
                    sqlx::query("update matches set taker_block_hash = $2, taker_block_height = $3, taker_included_height = $4 where id = $1")
                        .bind(&m.id)
                        .bind(block_hash)
                        .bind(*block_height as i64)
                        .bind(e.height as i32)
                        .execute(&mut *c)
                        .await?;
                    if m.claim_bond > 0 {
                        unlock_to(c, &m.taker, &m.taker, m.claim_bond).await?;
                    }
                    if m.maker_chain == "blrd" {
                        // The maker's BLRD was escrowed at placement; it is the taker's now.
                        unlock_to(c, &m.maker, &m.taker, m.maker_amount).await?;
                        close_match(c, &m.id, "closed", "settled on the ledger", seq).await?;
                        events.push(ev("MatchClosed", json!({"match": hex::encode(&m.id)})));
                    }
                }
                1 => {
                    if m.maker_txid.as_deref() != Some(txid.as_str()) {
                        bail!("txid does not match the reported maker payment");
                    }
                    if m.maker_included_height.is_some() {
                        bail!("maker leg already included");
                    }
                    sqlx::query("update matches set maker_block_hash = $2, maker_block_height = $3, maker_included_height = $4 where id = $1")
                        .bind(&m.id)
                        .bind(block_hash)
                        .bind(*block_height as i64)
                        .bind(e.height as i32)
                        .execute(&mut *c)
                        .await?;
                }
                _ => bail!("bad leg"),
            }
            events.push(ev("LegIncluded", json!({"match": hex::encode(&m.id), "leg": leg, "txid": txid})));
        }

        Body::ClaimVoid { match_id, reason } => {
            let m = get_match(c, match_id).await?;
            if m.status != "open" || m.taker_included_height.is_some() {
                bail!("claim cannot be voided");
            }
            match reason {
                1 => {
                    let deadline = m.claimed_height as u32 + chain_params(&m.taker_chain).taker_deadline;
                    if e.height < deadline {
                        bail!("taker deadline {deadline} has not passed");
                    }
                }
                2 => {
                    if m.taker_txid.is_none() {
                        bail!("no taker payment to have been reorged");
                    }
                }
                _ => bail!("bad reason"),
            }
            restore_to_order(c, &m).await?;
            if m.claim_bond > 0 {
                unlock_to(c, &m.taker, &m.maker, m.claim_bond).await?;
            }
            let why = if *reason == 1 { "taker payment not in a block by the deadline" } else { "taker payment reorged out" };
            close_match(c, &m.id, "void", why, seq).await?;
            events.push(ev("ClaimVoid", json!({"match": hex::encode(&m.id), "order": hex::encode(&m.order_id), "reason": why})));
        }

        Body::LegMissed { match_id, leg } => {
            let m = get_match(c, match_id).await?;
            if *leg != 1 {
                bail!("only the maker's leg can be missed");
            }
            if m.status != "open" {
                bail!("match is {}", m.status);
            }
            let Some(inc) = m.taker_included_height else { bail!("taker leg not included yet") };
            let deadline = inc as u32 + chain_params(&m.maker_chain).maker_deadline + GRACE_BLOCKS;
            if e.height < deadline {
                bail!("maker deadline {deadline} has not passed");
            }
            // The maker's bond goes to the taker in full.
            unlock_to(c, &m.maker, &m.taker, m.bond).await?;
            close_match(c, &m.id, "missed", "maker did not deliver", seq).await?;
            events.push(ev("LegMissed", json!({"match": hex::encode(&m.id), "bond_to_taker": m.bond})));
        }

        Body::MatchClosed { match_id } => {
            let m = get_match(c, match_id).await?;
            if m.status != "open" {
                bail!("match is {}", m.status);
            }
            if m.maker_included_height.is_none() {
                bail!("maker leg not included");
            }
            let payer = m.maker_payer.clone().unwrap_or_else(|| m.maker.clone());
            let fee = i(MATCH_FEE)?.min(m.bond);
            unlock_to(c, &m.maker, &payer, m.bond - fee).await?;
            unlock_to(c, &m.maker, &e.leader, fee).await?;
            close_match(c, &m.id, "closed", "both legs delivered", seq).await?;
            events.push(ev("MatchClosed", json!({"match": hex::encode(&m.id), "bond_to": hex::encode(&payer), "fee": fee})));
        }

        Body::Handoff { reason, replaced } => {
            events.push(ev("Handoff", json!({"reason": reason, "replaced": hex::encode(replaced), "leader": hex::encode(e.leader)})));
        }
    }
    Ok(events)
}

/// Wipe derived state so the log can be replayed.
pub async fn reset_state(c: &mut PgConnection) -> Result<()> {
    sqlx::query("truncate accounts, orders, matches").execute(&mut *c).await?;
    sqlx::query("update epochs set leader_key = null, first_seq = null, last_seq = null").execute(c).await?;
    Ok(())
}
