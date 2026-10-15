//! The state transition. `apply` is the only code that changes accounts,
//! balances, orders and matches. It runs inside the same database
//! transaction that stores the entry and uses nothing but the entry and prior
//! state, so every node ends up with identical tables.
//!
//! Money on the ledger comes in two kinds: BLRD, the network's own token, and
//! options, bonded promises by an issuer to deliver a real asset on demand.
//! Trades between ledger assets settle instantly. The chain is touched only
//! when options are bought with an on-chain payment (a deposit) or exercised
//! (a withdrawal).

use crate::chain::{self, chain_params, GRACE_BLOCKS};
use crate::fbio::{Body, ParsedEntry, TxBody};
use crate::ledger;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use sqlx::PgConnection;

pub const PRICE_SCALE: u128 = 100_000_000;
pub const TRANSFER_FEE: u64 = 0;
pub const MATCH_FEE: u64 = 100_000; // 0.001 BLRD, taken from an exercised issuer's bond
pub const CLAIM_WINDOW_MS: u64 = 180_000;
pub const BLRD: &str = "BLRD";

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

fn quote_of(qty: i64, price: i64) -> Result<i64> {
    i64::try_from(qty as u128 * price as u128 / PRICE_SCALE).map_err(|_| anyhow!("amount too large"))
}

// ------------------------------------------------------------------ BLRD

async fn account(c: &mut PgConnection, pk: &[u8]) -> Result<(i64, i64, i64)> {
    sqlx::query("insert into accounts (pubkey) values ($1) on conflict do nothing").bind(pk).execute(&mut *c).await?;
    let row: (i64, i64, i64) = sqlx::query_as("select nonce, balance, locked from accounts where pubkey = $1").bind(pk).fetch_one(&mut *c).await?;
    Ok(row)
}

async fn credit(c: &mut PgConnection, pk: &[u8], amount: i64) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    sqlx::query("insert into accounts (pubkey, balance) values ($1, $2) on conflict (pubkey) do update set balance = accounts.balance + $2").bind(pk).bind(amount).execute(c).await?;
    Ok(())
}

async fn debit(c: &mut PgConnection, pk: &[u8], amount: i64) -> Result<()> {
    let (_, bal, _) = account(c, pk).await?;
    if bal < amount {
        bail!("insufficient BLRD: have {bal} rill, need {amount}");
    }
    sqlx::query("update accounts set balance = balance - $2 where pubkey = $1").bind(pk).bind(amount).execute(c).await?;
    Ok(())
}

/// Move `amount` from balance to locked, failing if the balance is short.
async fn lock(c: &mut PgConnection, pk: &[u8], amount: i64) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    let (_, bal, _) = account(c, pk).await?;
    if bal < amount {
        bail!("insufficient BLRD: have {bal} rill, need {amount}");
    }
    sqlx::query("update accounts set balance = balance - $2, locked = locked + $2 where pubkey = $1").bind(pk).bind(amount).execute(c).await?;
    Ok(())
}

/// Move `amount` out of `from`'s locked BLRD into `to`'s balance.
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

// ------------------------------------------------------------------ any ledger asset

/// Pay `amount` of `asset` from one account to another: BLRD from the balance,
/// options as issuer lots.
async fn pay(c: &mut PgConnection, from: &[u8], to: &[u8], asset: &str, amount: i64) -> Result<()> {
    if asset == BLRD {
        debit(c, from, amount).await?;
        credit(c, to, amount).await
    } else {
        ledger::transfer(c, from, to, asset, amount).await
    }
}

/// Set aside `amount` of `asset` for an order: BLRD is locked in the maker's
/// account, options move to the order's own escrow account.
async fn escrow(c: &mut PgConnection, maker: &[u8], order: &[u8], asset: &str, amount: i64) -> Result<()> {
    if asset == BLRD {
        lock(c, maker, amount).await
    } else {
        ledger::transfer(c, maker, order, asset, amount).await
    }
}

/// Release `amount` of an order's escrow to `to`.
async fn release(c: &mut PgConnection, o: &crate::db::Order, to: &[u8], amount: i64) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    if o.escrow_asset == BLRD {
        unlock_to(c, &o.account, to, amount).await?;
    } else {
        ledger::transfer(c, &o.id, to, &o.escrow_asset, amount).await?;
    }
    sqlx::query("update orders set escrow_left = escrow_left - $2 where id = $1").bind(&o.id).bind(amount).execute(c).await?;
    Ok(())
}

async fn get_order(c: &mut PgConnection, id: &[u8]) -> Result<crate::db::Order> {
    sqlx::query_as::<_, crate::db::Order>("select * from orders where id = $1").bind(id).fetch_optional(c).await?.ok_or_else(|| anyhow!("unknown order"))
}

async fn get_match(c: &mut PgConnection, id: &[u8]) -> Result<crate::db::Match> {
    sqlx::query_as::<_, crate::db::Match>("select * from matches where id = $1").bind(id).fetch_optional(c).await?.ok_or_else(|| anyhow!("unknown match"))
}

/// Give a deposit claim's quantity back to its order, or its escrow back to
/// the maker if the order is no longer open.
async fn restore_to_order(c: &mut PgConnection, m: &crate::db::Match) -> Result<()> {
    let o = get_order(c, &m.order_id).await?;
    if o.status == "open" || o.status == "filled" {
        sqlx::query("update orders set qty_left = qty_left + $2, status = 'open' where id = $1").bind(&o.id).bind(m.quantity).execute(c).await?;
    } else {
        release(c, &o, &o.account, m.quantity).await?;
    }
    Ok(())
}

async fn close_match(c: &mut PgConnection, id: &[u8], status: &str, reason: &str, seq: u64) -> Result<()> {
    sqlx::query("update matches set status = $2, closed_reason = $3, closed_seq = $4 where id = $1").bind(id).bind(status).bind(reason).bind(seq as i64).execute(c).await?;
    Ok(())
}

async fn record_transfer(c: &mut PgConnection, e: &ParsedEntry, from: &[u8], to: &[u8], asset: &str, amount: i64, delivered_asset: &str, delivered: i64) -> Result<()> {
    sqlx::query("insert into transfers (seq, ts, sender, receiver, asset, amount, delivered_asset, delivered) values ($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(e.seq as i64)
        .bind(e.time as i64)
        .bind(from)
        .bind(to)
        .bind(asset)
        .bind(amount)
        .bind(delivered_asset)
        .bind(delivered)
        .execute(c)
        .await?;
    Ok(())
}

fn ev(name: &str, v: Value) -> Value {
    let mut v = v;
    v["event"] = json!(name);
    v
}

/// A ledger-settled fill: `qty` of the order's base changes hands right now.
/// Returns the match id recorded for history.
async fn fill_ledger(c: &mut PgConnection, e: &ParsedEntry, o: &crate::db::Order, taker: &[u8], qty: i64, nth: u32, deliver_to: &[u8]) -> Result<(Vec<u8>, i64, i64)> {
    let quote_amount = quote_of(qty, o.price)?;
    if quote_amount <= 0 {
        bail!("too small to price");
    }
    let (taker_pays_asset, taker_pays, taker_gets_asset, taker_gets) = if o.side == 1 { (o.quote.clone(), quote_amount, o.base.clone(), qty) } else { (o.base.clone(), qty, o.quote.clone(), quote_amount) };
    // Taker pays the maker; the maker's side comes out of escrow.
    pay(c, taker, &o.account, &taker_pays_asset, taker_pays).await?;
    release(c, o, deliver_to, taker_gets).await?;
    let left = o.qty_left - qty;
    let status = if left == 0 { "filled" } else { "open" };
    sqlx::query("update orders set qty_left = $2, status = $3, claims = claims + 1 where id = $1").bind(&o.id).bind(left).bind(status).execute(&mut *c).await?;
    // Match ids must be unique even when one entry fills several orders.
    let mut idb = e.hash.to_vec();
    idb.extend_from_slice(&nth.to_be_bytes());
    let id = crate::crypto::sha256(&idb).to_vec();
    sqlx::query("insert into matches (id, seq, order_id, maker, taker, market, side, quantity, price, bond, claim_bond, fill_policy, taker_chain, taker_asset, taker_amount, taker_pay_to, maker_chain, maker_asset, maker_amount, maker_pay_to, status, claimed_ts, claimed_height, taker_txid, taker_included_height, maker_txid, maker_included_height, closed_seq, closed_reason, kind) values ($1,$2,$3,$4,$5,$6,$7,$8,$9,0,0,$10,'ledger',$11,$12,'','ledger',$13,$14,'','closed',$15,$16,'ledger',$16,'ledger',$16,$2,'settled on the ledger','trade')")
        .bind(&id)
        .bind(e.seq as i64)
        .bind(&o.id)
        .bind(&o.account)
        .bind(taker)
        .bind(&o.market)
        .bind(o.side)
        .bind(qty)
        .bind(o.price)
        .bind(o.fill_policy)
        .bind(&taker_pays_asset)
        .bind(taker_pays)
        .bind(&taker_gets_asset)
        .bind(taker_gets)
        .bind(e.time as i64)
        .bind(e.height as i32)
        .execute(&mut *c)
        .await?;
    Ok((id, taker_pays, taker_gets))
}

/// Convert `amount` of `from_asset` held by `payer` into `to_asset`, delivered
/// to `to`, by taking the best resting ledger orders. Fails unless the whole
/// amount can be converted.
async fn swap_ledger(c: &mut PgConnection, e: &ParsedEntry, payer: &[u8], from_asset: &str, to_asset: &str, amount: i64, to: &[u8]) -> Result<i64> {
    // Asks on to/from: the maker sells what we want for what we have. Bids on
    // from/to: the maker buys what we have with what we want.
    let asks = sqlx::query_as::<_, crate::db::Order>("select * from orders where market = $1 and status = 'open' and side = 1 and settle = 0 and expiry_height > $2 order by price asc, seq asc")
        .bind(format!("{to_asset}/{from_asset}"))
        .bind(e.height as i32)
        .fetch_all(&mut *c)
        .await?;
    let bids = sqlx::query_as::<_, crate::db::Order>("select * from orders where market = $1 and status = 'open' and side = 0 and settle = 0 and expiry_height > $2 order by price desc, seq asc")
        .bind(format!("{from_asset}/{to_asset}"))
        .bind(e.height as i32)
        .fetch_all(&mut *c)
        .await?;
    // Merge by rate (to-asset received per from-asset paid), best first.
    let mut all: Vec<(f64, crate::db::Order)> = Vec::new();
    for o in asks {
        all.push((PRICE_SCALE as f64 / o.price as f64, o));
    }
    for o in bids {
        all.push((o.price as f64 / PRICE_SCALE as f64, o));
    }
    all.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut left = amount;
    let mut got = 0i64;
    let mut nth = 1u32;
    for (_, o) in all {
        if left <= 0 {
            break;
        }
        let o = get_order(c, &o.id).await?;
        let qty = if o.side == 1 {
            // we pay quote (from_asset); base qty we can afford
            let affordable = i64::try_from(left as u128 * PRICE_SCALE / o.price as u128).unwrap_or(0);
            affordable.min(o.qty_left)
        } else {
            left.min(o.qty_left)
        };
        if qty <= 0 {
            continue;
        }
        // A failed statement aborts the whole Postgres transaction, so there is no skipping past a bad order.
        let (_, paid, received) = fill_ledger(c, e, &o, payer, qty, nth, to).await?;
        left -= paid;
        got += received;
        nth += 1;
    }
    if left > 0 {
        bail!("not enough {to_asset} for sale to convert {amount} of {from_asset}; {left} left over");
    }
    Ok(got)
}

/// Apply one entry to the state. Returns events for live listeners.
pub async fn apply(c: &mut PgConnection, e: &ParsedEntry) -> Result<Vec<Value>> {
    let seq = e.seq;
    let mut events = Vec::new();
    // Orders cancel themselves at their expiry height, whatever entry brings that height.
    let expired: Vec<Vec<u8>> = sqlx::query_scalar("select id from orders where status = 'open' and expiry_height <= $1").bind(e.height as i32).fetch_all(&mut *c).await?;
    for id in expired {
        let o = get_order(c, &id).await?;
        release(c, &o, &o.account, o.escrow_left).await?;
        sqlx::query("update orders set status = 'expired', qty_left = 0 where id = $1").bind(&id).execute(&mut *c).await?;
        events.push(ev("OrderExpired", json!({"order": hex::encode(&id), "market": o.market})));
    }
    match &e.body {
        Body::EpochStart { height, entry_txid } => {
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
            sqlx::query("update epochs set last_seq = $2 where height < $1 and last_seq is null and first_seq is not null").bind(*height as i32).bind(seq as i64 - 1).execute(&mut *c).await?;
            events.push(ev("EpochStart", json!({"height": height, "leader": hex::encode(e.leader), "reward": reward})));
        }

        Body::Tx(tx) => {
            let acct = tx.account.clone();
            bump_nonce(c, &acct, tx.nonce).await?;
            match &tx.body {
                TxBody::Transfer { to, amount } => {
                    if !crate::crypto::valid_account(to) {
                        bail!("recipient must be a public key");
                    }
                    let total = i(amount.checked_add(TRANSFER_FEE).ok_or_else(|| anyhow!("overflow"))?)?;
                    debit(c, &acct, total).await?;
                    credit(c, to, i(*amount)?).await?;
                    credit(c, &e.leader, i(TRANSFER_FEE)?).await?;
                    record_transfer(c, e, &acct, to, BLRD, i(*amount)?, BLRD, i(*amount)?).await?;
                    events.push(ev("Transfer", json!({"from": hex::encode(&acct), "to": hex::encode(to), "asset": BLRD, "amount": amount})));
                }

                TxBody::SetCurrency { asset } => {
                    ledger::ensure_asset(asset)?;
                    account(c, &acct).await?;
                    sqlx::query("update accounts set preferred = $2 where pubkey = $1").bind(&acct).bind(asset).execute(&mut *c).await?;
                    events.push(ev("SetCurrency", json!({"account": hex::encode(&acct), "asset": asset})));
                }

                TxBody::Mint { asset, quantity, bond } => {
                    let a = ledger::ensure_asset(asset)?;
                    if a.chain == "blrd" {
                        bail!("BLRD cannot be minted");
                    }
                    if *quantity == 0 {
                        bail!("quantity must be positive");
                    }
                    lock(c, &acct, i(*bond)?).await?;
                    let (issued, cover) = ledger::issuer_row(c, &acct, asset).await?;
                    ledger::set_issuer(c, &acct, asset, issued + i(*quantity)?, cover + i(*bond)?).await?;
                    ledger::credit(c, &acct, asset, &acct, i(*quantity)?).await?;
                    events.push(ev("Mint", json!({"issuer": hex::encode(&acct), "asset": asset, "quantity": quantity, "bond": bond})));
                }

                TxBody::Burn { asset, quantity } => {
                    ledger::ensure_asset(asset)?;
                    let q = i(*quantity)?;
                    let own: Option<i64> = sqlx::query_scalar("select amount from balances where account = $1 and asset = $2 and issuer = $1").bind(&acct).bind(asset).fetch_optional(&mut *c).await?;
                    if own.unwrap_or(0) < q {
                        bail!("you hold only {} of your own {asset} options", own.unwrap_or(0));
                    }
                    sqlx::query("update balances set amount = amount - $3 where account = $1 and asset = $2 and issuer = $1").bind(&acct).bind(asset).bind(q).execute(&mut *c).await?;
                    sqlx::query("delete from balances where account = $1 and asset = $2 and amount = 0").bind(&acct).bind(asset).execute(&mut *c).await?;
                    let (issued, cover) = ledger::issuer_row(c, &acct, asset).await?;
                    let freed = ledger::cover_for(issued, cover, q);
                    ledger::set_issuer(c, &acct, asset, issued - q, cover - freed).await?;
                    unlock_to(c, &acct, &acct, freed).await?;
                    events.push(ev("Burn", json!({"issuer": hex::encode(&acct), "asset": asset, "quantity": quantity, "cover_freed": freed})));
                }

                TxBody::Send { to, asset, amount } => {
                    if !crate::crypto::valid_account(to) {
                        bail!("recipient must be a public key");
                    }
                    ledger::ensure_asset(asset)?;
                    let amt = i(*amount)?;
                    if amt <= 0 {
                        bail!("amount must be positive");
                    }
                    account(c, to).await?;
                    let preferred: Option<String> = sqlx::query_scalar("select preferred from accounts where pubkey = $1").bind(to).fetch_one(&mut *c).await?;
                    match preferred {
                        Some(p) if p != *asset => {
                            let got = swap_ledger(c, e, &acct, asset, &p, amt, to).await?;
                            record_transfer(c, e, &acct, to, asset, amt, &p, got).await?;
                            events.push(ev("Send", json!({"from": hex::encode(&acct), "to": hex::encode(to), "asset": asset, "amount": amount, "delivered_asset": p, "delivered": got})));
                        }
                        _ => {
                            pay(c, &acct, to, asset, amt).await?;
                            record_transfer(c, e, &acct, to, asset, amt, asset, amt).await?;
                            events.push(ev("Send", json!({"from": hex::encode(&acct), "to": hex::encode(to), "asset": asset, "amount": amount, "delivered_asset": asset, "delivered": amount})));
                        }
                    }
                }

                TxBody::Exercise { asset, quantity, receive_address } => {
                    let a = ledger::ensure_asset(asset)?;
                    if a.chain == "blrd" {
                        bail!("BLRD is not an option");
                    }
                    if *quantity < a.min_quantity {
                        bail!("quantity below the minimum of {} {}", a.min_quantity, a.id);
                    }
                    if receive_address.is_empty() {
                        bail!("receive_address is required");
                    }
                    let lots = ledger::debit(c, &acct, asset, i(*quantity)?).await?;
                    for (n, (issuer, q)) in lots.iter().enumerate() {
                        let (issued, cover) = ledger::issuer_row(c, issuer, asset).await?;
                        let bond = ledger::cover_for(issued, cover, *q);
                        ledger::set_issuer(c, issuer, asset, issued - q, cover - bond).await?;
                        let mut idb = e.hash.to_vec();
                        idb.extend_from_slice(&(n as u32).to_be_bytes());
                        let id = crate::crypto::sha256(&idb).to_vec();
                        sqlx::query("insert into matches (id, seq, order_id, maker, taker, market, side, quantity, price, bond, claim_bond, fill_policy, taker_chain, taker_asset, taker_amount, taker_pay_to, maker_chain, maker_asset, maker_amount, maker_pay_to, status, claimed_ts, claimed_height, taker_txid, taker_included_height, kind) values ($1,$2,$1,$3,$4,$5,1,$6,0,$7,0,1,'ledger',$5,$6,'',$8,$5,$6,$9,'open',$10,$11,'exercised',$11,'exercise')")
                            .bind(&id)
                            .bind(seq as i64)
                            .bind(issuer)
                            .bind(&acct)
                            .bind(asset)
                            .bind(*q)
                            .bind(bond)
                            .bind(a.chain)
                            .bind(receive_address)
                            .bind(e.time as i64)
                            .bind(e.height as i32)
                            .execute(&mut *c)
                            .await?;
                        events.push(ev("Exercise", json!({"match": hex::encode(&id), "account": hex::encode(&acct), "issuer": hex::encode(issuer), "asset": asset, "quantity": q, "bond": bond, "to": receive_address})));
                    }
                }

                TxBody::PlaceOrder { market, side, quantity, price, bond: _, claim_bond, fill_policy, receive_address, expiry_height, settle } => {
                    let (base, quote) = chain::parse_market(market)?;
                    if *side > 1 {
                        bail!("side must be 0 (buy) or 1 (sell)");
                    }
                    if *settle > 1 {
                        bail!("settle must be 0 (ledger) or 1 (taker pays on chain)");
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
                    let qty = i(*quantity)?;
                    let quote_amount = quote_of(qty, i(*price)?)?;
                    if quote_amount < quote.min_quantity as i64 {
                        bail!("quote amount below the minimum of {} {}", quote.min_quantity, quote.id);
                    }
                    if *settle == 0 && base.id == quote.id {
                        bail!("base and quote must differ on a ledger market");
                    }
                    if *settle == 1 {
                        if *side != 1 {
                            bail!("an order paid on chain must sell its base");
                        }
                        if quote.chain == "blrd" {
                            bail!("BLRD cannot be paid on chain");
                        }
                        if receive_address.is_empty() {
                            bail!("receive_address is required: where the taker's on-chain payment goes");
                        }
                        chain::receive_address(receive_address, quote.chain, 0)?;
                    }
                    // What the maker sets aside: the base it sells, or the quote it buys with.
                    let (escrow_asset, escrow_amount) = if *side == 1 { (base.id, qty) } else { (quote.id, quote_amount) };
                    escrow(c, &acct, &e.hash, escrow_asset, escrow_amount).await?;
                    sqlx::query("insert into orders (id, seq, account, market, base, quote, side, quantity, qty_left, price, bond, bond_left, claim_bond, fill_policy, recv_addr, expiry_height, status, settle, escrow_asset, escrow_left) values ($1,$2,$3,$4,$5,$6,$7,$8,$8,$9,0,0,$10,$11,$12,$13,'open',$14,$15,$16)")
                        .bind(e.hash.to_vec())
                        .bind(seq as i64)
                        .bind(&acct)
                        .bind(market)
                        .bind(base.id)
                        .bind(quote.id)
                        .bind(*side as i16)
                        .bind(qty)
                        .bind(i(*price)?)
                        .bind(i(*claim_bond)?)
                        .bind(*fill_policy as i16)
                        .bind(receive_address)
                        .bind(*expiry_height as i32)
                        .bind(*settle as i16)
                        .bind(escrow_asset)
                        .bind(escrow_amount)
                        .execute(&mut *c)
                        .await?;
                    events.push(ev("OrderPlaced", json!({"order": hex::encode(e.hash), "market": market, "side": side, "quantity": quantity, "price": price, "settle": settle})));
                }

                TxBody::CancelOrder { order_id } => {
                    let o = get_order(c, order_id).await?;
                    if o.account != acct {
                        bail!("not your order");
                    }
                    if o.status != "open" {
                        bail!("order is {}", o.status);
                    }
                    // Give back everything not tied up in an open deposit claim.
                    let held: Option<i64> = sqlx::query_scalar("select sum(quantity)::bigint from matches where order_id = $1 and status = 'open'").bind(&o.id).fetch_one(&mut *c).await?;
                    let held = held.unwrap_or(0);
                    let keep = if o.side == 1 { held } else { quote_of(held, o.price)? };
                    release(c, &o, &o.account, o.escrow_left - keep).await?;
                    sqlx::query("update orders set status = 'cancelled', qty_left = 0 where id = $1").bind(&o.id).execute(&mut *c).await?;
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
                    if o.settle == 0 {
                        let (id, paid, got) = fill_ledger(c, e, &o, &acct, qty, 0, &acct).await?;
                        events.push(ev("Traded", json!({"match": hex::encode(&id), "order": hex::encode(&o.id), "market": o.market, "quantity": quantity, "taker": hex::encode(&acct), "paid": paid, "received": got})));
                    } else {
                        // Deposit: the taker pays the quote on chain; the base waits in escrow.
                        let quote_amount = quote_of(qty, o.price)?;
                        if quote_amount < quote.min_quantity as i64 {
                            bail!("quote amount below the minimum of {} {}", quote.min_quantity, quote.id);
                        }
                        if o.claim_bond > 0 {
                            lock(c, &acct, o.claim_bond).await?;
                        }
                        let taker_pay_to = chain::receive_address(&o.recv_addr, quote.chain, o.claims as u32)?;
                        let left = o.qty_left - qty;
                        let status = if left == 0 { "filled" } else { "open" };
                        sqlx::query("update orders set qty_left = $2, status = $3, claims = claims + 1 where id = $1").bind(&o.id).bind(left).bind(status).execute(&mut *c).await?;
                        sqlx::query("insert into matches (id, seq, order_id, maker, taker, market, side, quantity, price, bond, claim_bond, fill_policy, taker_chain, taker_asset, taker_amount, taker_pay_to, maker_chain, maker_asset, maker_amount, maker_pay_to, status, claimed_ts, claimed_height, kind) values ($1,$2,$3,$4,$5,$6,1,$7,$8,0,$9,$10,$11,$12,$13,$14,'ledger',$15,$7,$16,'open',$17,$18,'deposit')")
                            .bind(e.hash.to_vec())
                            .bind(seq as i64)
                            .bind(&o.id)
                            .bind(&o.account)
                            .bind(&acct)
                            .bind(&o.market)
                            .bind(qty)
                            .bind(o.price)
                            .bind(o.claim_bond)
                            .bind(o.fill_policy)
                            .bind(quote.chain)
                            .bind(quote.id)
                            .bind(quote_amount)
                            .bind(&taker_pay_to)
                            .bind(base.id)
                            .bind(receive_address)
                            .bind(e.time as i64)
                            .bind(e.height as i32)
                            .execute(&mut *c)
                            .await?;
                        events.push(ev("Claimed", json!({"match": hex::encode(e.hash), "order": hex::encode(&o.id), "market": o.market, "quantity": quantity, "taker": hex::encode(&acct), "pay": quote_amount, "pay_asset": quote.id, "pay_to": taker_pay_to})));
                    }
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
                            if m.kind != "deposit" {
                                bail!("this match has no on-chain taker leg");
                            }
                            if m.taker != acct {
                                bail!("only the taker reports the taker's leg");
                            }
                            if m.taker_txid.is_some() {
                                bail!("taker payment already reported");
                            }
                            sqlx::query("update matches set taker_txid = $2 where id = $1").bind(&m.id).bind(txid).execute(&mut *c).await?;
                        }
                        1 => {
                            if m.kind != "exercise" {
                                bail!("this match has no on-chain maker leg");
                            }
                            if m.maker_txid.is_some() {
                                bail!("delivery already reported");
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
            if m.status != "open" || m.kind != "deposit" || m.taker_txid.is_some() {
                bail!("claim cannot expire");
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
                    if m.kind != "deposit" || m.taker_txid.as_deref() != Some(txid.as_str()) {
                        bail!("txid does not match the reported taker payment");
                    }
                    if m.taker_included_height.is_some() {
                        bail!("taker leg already included");
                    }
                    sqlx::query("update matches set taker_block_hash = $2, taker_block_height = $3, taker_included_height = $4, maker_txid = 'ledger', maker_included_height = $4 where id = $1")
                        .bind(&m.id)
                        .bind(block_hash)
                        .bind(*block_height as i64)
                        .bind(e.height as i32)
                        .execute(&mut *c)
                        .await?;
                    if m.claim_bond > 0 {
                        unlock_to(c, &m.taker, &m.taker, m.claim_bond).await?;
                    }
                    // The payment is in a block: the options leave escrow for the taker.
                    let o = get_order(c, &m.order_id).await?;
                    release(c, &o, &m.taker, m.quantity).await?;
                    close_match(c, &m.id, "closed", "deposit credited", seq).await?;
                    events.push(ev("MatchClosed", json!({"match": hex::encode(&m.id), "taker": hex::encode(&m.taker), "asset": m.maker_asset, "quantity": m.quantity})));
                }
                1 => {
                    if m.kind != "exercise" || m.maker_txid.as_deref() != Some(txid.as_str()) {
                        bail!("txid does not match the reported delivery");
                    }
                    if m.maker_included_height.is_some() {
                        bail!("delivery already included");
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
            if m.status != "open" || m.kind != "deposit" || m.taker_included_height.is_some() {
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
            let why = if *reason == 1 { "payment not in a block by the deadline" } else { "payment reorged out" };
            close_match(c, &m.id, "void", why, seq).await?;
            events.push(ev("ClaimVoid", json!({"match": hex::encode(&m.id), "order": hex::encode(&m.order_id), "reason": why})));
        }

        Body::LegMissed { match_id, leg } => {
            let m = get_match(c, match_id).await?;
            if *leg != 1 || m.kind != "exercise" {
                bail!("only an exercise delivery can be missed");
            }
            if m.status != "open" {
                bail!("match is {}", m.status);
            }
            let Some(inc) = m.taker_included_height else { bail!("not exercised") };
            let deadline = inc as u32 + chain_params(&m.maker_chain).maker_deadline + GRACE_BLOCKS;
            if e.height < deadline {
                bail!("delivery deadline {deadline} has not passed");
            }
            // The issuer's bond for these options goes to the exerciser.
            unlock_to(c, &m.maker, &m.taker, m.bond).await?;
            close_match(c, &m.id, "missed", "issuer did not deliver", seq).await?;
            events.push(ev("LegMissed", json!({"match": hex::encode(&m.id), "bond_to_exerciser": m.bond})));
        }

        Body::MatchClosed { match_id } => {
            let m = get_match(c, match_id).await?;
            if m.status != "open" || m.kind != "exercise" {
                bail!("match is {}", m.status);
            }
            if m.maker_included_height.is_none() {
                bail!("delivery not included");
            }
            let payer = m.maker_payer.clone().unwrap_or_else(|| m.maker.clone());
            let fee = i(MATCH_FEE)?.min(m.bond);
            unlock_to(c, &m.maker, &payer, m.bond - fee).await?;
            unlock_to(c, &m.maker, &e.leader, fee).await?;
            close_match(c, &m.id, "closed", "delivered", seq).await?;
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
    sqlx::query("truncate accounts, orders, matches, balances, issuers, transfers").execute(&mut *c).await?;
    sqlx::query("update epochs set leader_key = null, first_seq = null, last_seq = null").execute(c).await?;
    Ok(())
}
