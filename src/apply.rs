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
/// Blocks between asking to unstake and getting the BLRD back; the stake stays slashable meanwhile.
pub const UNSTAKE_DELAY: u32 = 144;
/// The oracle: trade-weighted average prices from the ledger's own books.
pub const USDC: &str = "eth:USDC";
pub const ORACLE_SHORT: u32 = 144; // about a day
pub const ORACLE_LONG: u32 = 4320; // about a month
/// Dollars per BLRD before the BLRD/USDC market has traded: USDC atoms per 10^8 rill, so $1.
pub const GENESIS_BLRD_PRICE: i64 = 1_000_000;
/// Opening prices for the other assets, used only until each has traded against USDC:
/// USDC atoms per 10^8 atoms. BTC $80,000 per coin; ETH $3,000 per coin (atoms are gwei); USDT $1.
pub fn genesis_price(asset: &str) -> Option<i64> {
    match asset {
        BLRD => Some(GENESIS_BLRD_PRICE),
        "btc:BTC" => Some(80_000_000_000),
        "eth:ETH" => Some(300_000_000),
        "eth:USDT" => Some(100_000_000),
        "base:USDC" => Some(100_000_000),
        // A vault share was worth about $1.046 when listed; 10^8 atoms is 100 shares.
        "base:gtUSDCp" => Some(104_600_000),
        _ => None,
    }
}
/// Stake value must cover issued value by this much, scaled up when BLRD runs above its monthly average.
pub const BASE_RATIO_BPS: i128 = 15_000;
/// Under the required ratio, holders may redeem the issuer's options from its stake at this premium.
pub const LIQUIDATION_PREMIUM_BPS: i128 = 11_000;
/// The hard floor: under this, the leader liquidates the issuer at the next block.
pub const LIQUIDATION_FLOOR_BPS: i128 = 11_000;
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

async fn account_full(c: &mut PgConnection, pk: &[u8]) -> Result<(i64, i64, i64, i64, i64, i32)> {
    account(c, pk).await?;
    let r: (i64, i64, i64, i64, i64, i32) = sqlx::query_as("select nonce, balance, locked, staked, unstaking, unstake_height from accounts where pubkey = $1").bind(pk).fetch_one(c).await?;
    Ok(r)
}

/// Who delivers an exercise of `quantity` of `asset`: the thinnest-collateralized
/// issuer that can cover the whole thing on its own, so delivery doubles as a
/// margin call while the holder still gets one payment; else the largest issuer.
/// Returns it and the (issuer, quantity) shares it takes over from the others.
async fn assign_deliverer(c: &mut PgConnection, asset: &str, quantity: i64, height: u32) -> Result<(Vec<u8>, Vec<(Vec<u8>, i64)>)> {
    let rows: Vec<(Vec<u8>, i64)> = sqlx::query_as("select account, issued from issuers where asset = $1 and issued > 0").bind(asset).fetch_all(&mut *c).await?;
    let total: i64 = rows.iter().map(|r| r.1).sum();
    if total < quantity {
        bail!("only {total} {asset} is outstanding across all issuers");
    }
    let mut ranked = Vec::new();
    for (acct, issued) in rows {
        let iv = issued_value(c, &acct, height).await.unwrap_or(1).max(1);
        let sv = stake_value(c, &acct, height).await?;
        ranked.push((sv * 1_000_000 / iv, acct, issued));
    }
    ranked.sort();
    let deliverer = match ranked.iter().find(|r| r.2 >= quantity) {
        Some(r) => r.1.clone(),
        None => ranked.iter().max_by_key(|r| r.2).map(|r| r.1.clone()).expect("issuers"),
    };
    let own = ranked.iter().find(|r| r.1 == deliverer).map(|r| r.2).unwrap_or(0);
    let mut left = quantity - own;
    let mut taken = Vec::new();
    for (_, acct, issued) in ranked {
        if left <= 0 {
            break;
        }
        if acct == deliverer {
            continue;
        }
        let take = left.min(issued);
        taken.push((acct, take));
        left -= take;
    }
    Ok((deliverer, taken))
}

/// When a maker's reserved stake did not cover what it owed, the other issuers
/// of the same asset make up the difference in proportion to what they issued.
/// This is what makes one maker's option as good as another's.
async fn socialize_shortfall(c: &mut PgConnection, asset: &str, except: &[u8], mut value: i128, to: &[u8], height: u32) -> Result<i128> {
    let rows: Vec<(Vec<u8>, i64)> = sqlx::query_as("select account, issued from issuers where asset = $1 and issued > 0 and account <> $2").bind(asset).bind(except).fetch_all(&mut *c).await?;
    let total: i128 = rows.iter().map(|r| r.1 as i128).sum();
    if total == 0 || value <= 0 {
        return Ok(value.max(0));
    }
    let p_blrd = oracle_price(c, BLRD, height).await?.unwrap_or(GENESIS_BLRD_PRICE) as i128;
    let want = value;
    for (acct, issued) in rows {
        let share = want * issued as i128 / total;
        if share <= 0 {
            continue;
        }
        let (_, _, _, staked, unstaking, _) = account_full(c, &acct).await?;
        let blrd = (share * PRICE_SCALE as i128 / p_blrd.max(1)).min((staked + unstaking) as i128) as i64;
        let from_staked = blrd.min(staked);
        sqlx::query("update accounts set staked = staked - $2, unstaking = unstaking - $3 where pubkey = $1").bind(&acct).bind(from_staked).bind(blrd - from_staked).execute(&mut *c).await?;
        credit(c, to, blrd).await?;
        let covered = blrd as i128 * p_blrd / PRICE_SCALE as i128;
        let rest = pay_from_staked_lots(c, &acct, to, share - covered, height).await?;
        value -= share - rest;
    }
    Ok(value.max(0))
}

/// Move every lot held under `from` (a match or stake account) to `to`.
async fn move_all_lots(c: &mut PgConnection, from: &[u8], to: &[u8]) -> Result<()> {
    let rows: Vec<(String, i64)> = sqlx::query_as("select asset, sum(amount)::bigint from balances where account = $1 group by asset").bind(from).fetch_all(&mut *c).await?;
    for (asset, amount) in rows {
        if amount > 0 {
            ledger::transfer(c, from, to, &asset, amount).await?;
        }
    }
    Ok(())
}

/// Where a maker's non-BLRD stake lives: a balances account nobody can sign for.
pub fn stake_account(pk: &[u8]) -> Vec<u8> {
    let mut v = b"bollard/stake/".to_vec();
    v.extend_from_slice(pk);
    crate::crypto::sha256(&v).to_vec()
}

/// Dollar value (USDC atoms) of the options a maker has staked, at oracle prices.
async fn staked_lots_value(c: &mut PgConnection, pk: &[u8], height: u32) -> Result<i128> {
    let sa = stake_account(pk);
    let rows: Vec<(String, i64)> = sqlx::query_as("select asset, sum(amount)::bigint from balances where account = $1 group by asset").bind(&sa).fetch_all(&mut *c).await?;
    let mut total: i128 = 0;
    for (asset, amount) in rows {
        let p = oracle_price(c, &asset, height).await?.unwrap_or(0);
        total += amount as i128 * p as i128 / PRICE_SCALE as i128;
    }
    Ok(total)
}

/// Pay `value` USDC atoms' worth out of a maker's staked options to `to`, BLRD having run out. Returns what could not be covered.
async fn pay_from_staked_lots(c: &mut PgConnection, pk: &[u8], to: &[u8], mut value: i128, height: u32) -> Result<i128> {
    let sa = stake_account(pk);
    let rows: Vec<(String, i64)> = sqlx::query_as("select asset, sum(amount)::bigint from balances where account = $1 group by asset order by asset").bind(&sa).fetch_all(&mut *c).await?;
    for (asset, amount) in rows {
        if value <= 0 {
            break;
        }
        let p = oracle_price(c, &asset, height).await?.unwrap_or(0) as i128;
        if p <= 0 {
            continue;
        }
        let want = (value * PRICE_SCALE as i128 / p) as i64;
        let take = want.min(amount);
        if take > 0 {
            ledger::transfer(c, &sa, to, &asset, take).await?;
            value -= take as i128 * p / PRICE_SCALE as i128;
        }
    }
    Ok(value.max(0))
}

/// Stake that still backs the maker's options: what is staked plus what is waiting to unstake.
async fn stake_total(c: &mut PgConnection, pk: &[u8]) -> Result<i64> {
    let (_, _, _, staked, unstaking, _) = account_full(c, pk).await?;
    Ok(staked + unstaking)
}

/// Trade-weighted average price of `market` over the last `window` blocks, in
/// quote atoms per 10^8 base atoms. Only fills count, never resting orders.
async fn twap(c: &mut PgConnection, market: &str, height: u32, window: u32) -> Result<Option<i64>> {
    let since = height.saturating_sub(window) as i32;
    let row: Option<(Option<i64>, Option<i64>)> = sqlx::query_as("select sum(price * quantity)::bigint, sum(quantity)::bigint from matches where market = $1 and kind = 'trade' and claimed_height > $2")
        .bind(market)
        .bind(since)
        .fetch_optional(c)
        .await?;
    Ok(match row {
        Some((Some(pq), Some(q))) if q > 0 => Some(pq / q),
        _ => None,
    })
}

/// The dollar price of an asset: USDC atoms per 10^8 atoms of it. A day's
/// average, else a month's, else the genesis price for BLRD, else nothing.
pub async fn oracle_price(c: &mut PgConnection, asset: &str, height: u32) -> Result<Option<i64>> {
    if asset == USDC {
        return Ok(Some(PRICE_SCALE as i64));
    }
    let market = format!("{asset}/{USDC}");
    if let Some(p) = twap(c, &market, height, ORACLE_SHORT).await? {
        return Ok(Some(p));
    }
    if let Some(p) = twap(c, &market, height, ORACLE_LONG).await? {
        return Ok(Some(p));
    }
    Ok(genesis_price(asset))
}

/// The required stake ratio in basis points: 150%, times how far BLRD's daily
/// average sits above its monthly average. A run-up never loosens collateral.
pub async fn required_ratio_bps(c: &mut PgConnection, height: u32) -> Result<i128> {
    let market = format!("{BLRD}/{USDC}");
    let now = twap(c, &market, height, ORACLE_SHORT).await?;
    let long = twap(c, &market, height, ORACLE_LONG).await?;
    Ok(match (now, long) {
        (Some(n), Some(l)) if l > 0 && n > l => BASE_RATIO_BPS * n as i128 / l as i128,
        _ => BASE_RATIO_BPS,
    })
}

/// Dollar value (USDC atoms) of everything an issuer has issued, at oracle prices.
pub async fn issued_value(c: &mut PgConnection, issuer: &[u8], height: u32) -> Result<i128> {
    let rows: Vec<(String, i64)> = sqlx::query_as("select asset, issued from issuers where account = $1 and issued > 0").bind(issuer).fetch_all(&mut *c).await?;
    let mut total: i128 = 0;
    for (asset, issued) in rows {
        let p = oracle_price(c, &asset, height).await?.ok_or_else(|| anyhow!("no price for {asset} yet: it has not traded against USDC"))?;
        total += issued as i128 * p as i128 / PRICE_SCALE as i128;
    }
    Ok(total)
}

/// Dollar value (USDC atoms) of an issuer's stake, including what is waiting to unstake.
pub async fn stake_value(c: &mut PgConnection, issuer: &[u8], height: u32) -> Result<i128> {
    let total = stake_total(c, issuer).await?;
    let p = oracle_price(c, BLRD, height).await?.unwrap_or(GENESIS_BLRD_PRICE);
    Ok(total as i128 * p as i128 / PRICE_SCALE as i128 + staked_lots_value(c, issuer, height).await?)
}

/// How much more, in USDC atoms, a maker could issue right now if it also staked every free BLRD it holds.
pub async fn mint_capacity(c: &mut PgConnection, issuer: &[u8], height: u32) -> Result<i128> {
    let (_, free, _) = account(c, issuer).await?;
    let p = oracle_price(c, BLRD, height).await?.unwrap_or(GENESIS_BLRD_PRICE);
    let stake = stake_value(c, issuer, height).await? + free as i128 * p as i128 / PRICE_SCALE as i128;
    let issued = issued_value(c, issuer, height).await.unwrap_or(0);
    let need = required_ratio_bps(c, height).await?;
    Ok((stake * 10_000 / need - issued).max(0))
}

/// Stake free BLRD so that the maker covers its issuance; fails if even that is not enough.
async fn auto_stake(c: &mut PgConnection, issuer: &[u8], height: u32) -> Result<i64> {
    let issued = issued_value(c, issuer, height).await?;
    let stake = stake_value(c, issuer, height).await?;
    let need = required_ratio_bps(c, height).await?;
    let short = issued * need / 10_000 - stake;
    if short <= 0 {
        return Ok(0);
    }
    let p = oracle_price(c, BLRD, height).await?.unwrap_or(GENESIS_BLRD_PRICE) as i128;
    let blrd = (short * PRICE_SCALE as i128 + p - 1) / p; // round up
    let (_, free, _) = account(c, issuer).await?;
    if (free as i128) < blrd {
        bail!("the maker's stake and free BLRD together do not cover this");
    }
    debit(c, issuer, blrd as i64).await?;
    sqlx::query("update accounts set staked = staked + $2 where pubkey = $1").bind(issuer).bind(blrd as i64).execute(&mut *c).await?;
    Ok(blrd as i64)
}

/// Fail unless the issuer's stake covers its issuance at the required ratio.
async fn check_collateral(c: &mut PgConnection, issuer: &[u8], height: u32) -> Result<()> {
    let issued = issued_value(c, issuer, height).await?;
    if issued == 0 {
        return Ok(());
    }
    let stake = stake_value(c, issuer, height).await?;
    let need = required_ratio_bps(c, height).await?;
    if stake * 10_000 < issued * need {
        bail!("stake worth {} USDC atoms covers only {}% of {} USDC atoms issued; {}% is required", stake, stake * 100 / issued, issued, need / 100);
    }
    Ok(())
}

/// Take BLRD worth `quantity` of `asset` (at oracle prices) out of the issuer's
/// stake, capped at what it has; it is held on the match until delivery or default.
async fn reserve_stake_worth(c: &mut PgConnection, issuer: &[u8], match_id: &[u8], asset: &str, quantity: i64, height: u32) -> Result<i64> {
    let (_, _, _, staked, unstaking, _) = account_full(c, issuer).await?;
    let total = staked + unstaking;
    let p_asset = oracle_price(c, asset, height).await?.unwrap_or(0) as i128;
    let p_blrd = oracle_price(c, BLRD, height).await?.unwrap_or(GENESIS_BLRD_PRICE) as i128;
    let value = quantity as i128 * p_asset / PRICE_SCALE as i128; // USDC atoms owed
    let bond = (value * PRICE_SCALE as i128 / p_blrd.max(1)).min(total as i128) as i64;
    let from_staked = bond.min(staked);
    let from_unstaking = bond - from_staked;
    sqlx::query("update accounts set staked = staked - $2, unstaking = unstaking - $3 where pubkey = $1").bind(issuer).bind(from_staked).bind(from_unstaking).execute(&mut *c).await?;
    // Whatever BLRD did not cover comes from the maker's staked options, held under the match until it settles.
    let rest = value - bond as i128 * p_blrd / PRICE_SCALE as i128;
    if rest > 0 {
        pay_from_staked_lots(c, issuer, match_id, rest, height).await?;
    }
    Ok(bond)
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
    if amount == 0 {
        return Ok(());
    }
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
    if o.mint {
        // The options minted for this claim are destroyed again; the maker owes nothing for them.
        ledger::debit(c, &o.id, &o.escrow_asset, m.quantity).await?;
        sqlx::query("update orders set escrow_left = escrow_left - $2 where id = $1").bind(&o.id).bind(m.quantity).execute(&mut *c).await?;
        let issued = ledger::issued(c, &o.account, &o.escrow_asset).await?;
        ledger::set_issued(c, &o.account, &o.escrow_asset, issued - m.quantity).await?;
        if o.status == "open" || o.status == "filled" {
            sqlx::query("update orders set qty_left = qty_left + $2, status = 'open' where id = $1").bind(&o.id).bind(m.quantity).execute(&mut *c).await?;
        }
        return Ok(());
    }
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
/// to `to`, by taking the best resting ledger orders. Every ledger market is
/// quoted in USDC, so a conversion between two other assets is two hops through
/// USDC in the same entry. Fails unless the whole amount can be converted.
async fn swap_ledger(c: &mut PgConnection, e: &ParsedEntry, payer: &[u8], from_asset: &str, to_asset: &str, amount: i64, to: &[u8]) -> Result<i64> {
    if from_asset == chain::QUOTE || to_asset == chain::QUOTE {
        return swap_pair(c, e, payer, from_asset, to_asset, amount, to, 1).await;
    }
    let usdc = swap_pair(c, e, payer, from_asset, chain::QUOTE, amount, payer, 1).await?;
    swap_pair(c, e, payer, chain::QUOTE, to_asset, usdc, to, 1000).await
}

/// One hop: take resting orders on the pair until `amount` of `from_asset` is spent.
/// `first_nth` keeps match ids unique across the hops of one entry.
async fn swap_pair(c: &mut PgConnection, e: &ParsedEntry, payer: &[u8], from_asset: &str, to_asset: &str, amount: i64, to: &[u8], first_nth: u32) -> Result<i64> {
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
    let mut nth = first_nth;
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
    // Unstakes mature at their height, whatever entry brings it.
    sqlx::query("update accounts set balance = balance + unstaking, unstaking = 0 where unstaking > 0 and unstake_height <= $1").bind(e.height as i32).execute(&mut *c).await?;
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

                TxBody::Mint { asset, quantity, source } => {
                    let a = ledger::ensure_asset(asset)?;
                    // Every issuer names where its coins sit; deliveries must come from there, and anyone can watch it.
                    if source.is_empty() {
                        bail!("name the on-chain address the coins behind these options sit at");
                    }
                    chain::receive_address(source, a.chain, 0)?;
                    let had = ledger::source(c, &acct, asset).await?;
                    if !had.is_empty() && had != *source && ledger::issued(c, &acct, asset).await? > 0 {
                        bail!("your outstanding {asset} is backed from {had}; burn it before moving to another address");
                    }
                    ledger::set_source(c, &acct, asset, source).await?;
                    if a.chain == "blrd" {
                        bail!("BLRD cannot be minted");
                    }
                    if *quantity == 0 {
                        bail!("quantity must be positive");
                    }
                    if ledger::total(c, &stake_account(&acct), asset).await? > 0 {
                        bail!("you have {asset} staked; a maker cannot issue an asset it uses as collateral");
                    }
                    let issued = ledger::issued(c, &acct, asset).await?;
                    ledger::set_issued(c, &acct, asset, issued + i(*quantity)?).await?;
                    ledger::credit(c, &acct, asset, ledger::NO_ISSUER, i(*quantity)?).await?;
                    check_collateral(c, &acct, e.height).await?;
                    events.push(ev("Mint", json!({"issuer": hex::encode(&acct), "asset": asset, "quantity": quantity})));
                }

                TxBody::Stake { amount, asset } => {
                    let amt = i(*amount)?;
                    if amt <= 0 {
                        bail!("amount must be positive");
                    }
                    if asset == BLRD {
                        debit(c, &acct, amt).await?;
                        sqlx::query("update accounts set staked = staked + $2 where pubkey = $1").bind(&acct).bind(amt).execute(&mut *c).await?;
                    } else {
                        // Options can back a maker's promises, but not options on an asset the maker itself issues.
                        ledger::ensure_asset(asset)?;
                        if ledger::issued(c, &acct, asset).await? > 0 {
                            bail!("you issue {asset}; it cannot also be your collateral");
                        }
                        ledger::transfer(c, &acct, &stake_account(&acct), asset, amt).await?;
                    }
                    events.push(ev("Stake", json!({"account": hex::encode(&acct), "asset": asset, "amount": amount})));
                }

                TxBody::Unstake { amount, asset } => {
                    let amt = i(*amount)?;
                    if amt <= 0 {
                        bail!("amount must be positive");
                    }
                    if asset == BLRD {
                        let (_, _, _, staked, _, _) = account_full(c, &acct).await?;
                        if amt > staked {
                            bail!("you have {staked} rill staked");
                        }
                        // Joins any amount already waiting; the clock restarts for all of it.
                        sqlx::query("update accounts set staked = staked - $2, unstaking = unstaking + $2, unstake_height = $3 where pubkey = $1")
                            .bind(&acct)
                            .bind(amt)
                            .bind((e.height + UNSTAKE_DELAY) as i32)
                            .execute(&mut *c)
                            .await?;
                    } else {
                        ledger::transfer(c, &stake_account(&acct), &acct, asset, amt).await?;
                    }
                    // Unstaking BLRD still backs the options until it is released; what matters is what remains afterwards.
                    let (_, _, _, staked_after, _, _) = account_full(c, &acct).await?;
                    let p = oracle_price(c, BLRD, e.height).await?.unwrap_or(GENESIS_BLRD_PRICE);
                    let issued = issued_value(c, &acct, e.height).await?;
                    let need = required_ratio_bps(c, e.height).await?;
                    let remaining = staked_after as i128 * p as i128 / PRICE_SCALE as i128 + staked_lots_value(c, &acct, e.height).await?;
                    if issued > 0 && remaining * 10_000 < issued * need {
                        bail!("that would leave the stake below {}% of what you have issued", need / 100);
                    }
                    events.push(ev("Unstake", json!({"account": hex::encode(&acct), "asset": asset, "amount": amount})));
                }

                TxBody::SetReceiveAddress { chain: ch, address } => {
                    if ch != "btc" && ch != "eth" && ch != "base" {
                        bail!("chain must be btc, eth or base");
                    }
                    if !address.is_empty() {
                        chain::receive_address(address, ch, 0)?;
                    }
                    // Replace this maker's standing offers on the chain.
                    sqlx::query("update orders set status = 'cancelled' where account = $1 and standing and status = 'open' and base in (select unnest($2::text[]))")
                        .bind(&acct)
                        .bind(chain::ASSETS.iter().filter(|a| a.chain == *ch).map(|a| a.id.to_string()).collect::<Vec<_>>())
                        .execute(&mut *c)
                        .await?;
                    if !address.is_empty() {
                        for a in chain::ASSETS.iter().filter(|a| a.chain == *ch) {
                            let had = ledger::source(c, &acct, a.id).await?;
                            if had.is_empty() || ledger::issued(c, &acct, a.id).await? == 0 {
                                ledger::set_source(c, &acct, a.id, address).await?;
                            }
                            let mut idb = e.hash.to_vec();
                            idb.extend_from_slice(a.id.as_bytes());
                            let id = crate::crypto::sha256(&idb).to_vec();
                            // One for one, minted against the stake as people claim, never expiring.
                            sqlx::query("insert into orders (id, seq, account, market, base, quote, side, quantity, qty_left, price, bond, bond_left, claim_bond, fill_policy, recv_addr, expiry_height, status, settle, escrow_asset, escrow_left, mint, standing) values ($1,$2,$3,$4,$5,$5,1,$6,$6,$7,0,0,0,1,$8,$9,'open',1,$5,0,true,true)")
                                .bind(&id)
                                .bind(seq as i64)
                                .bind(&acct)
                                .bind(format!("{}/{}", a.id, a.id))
                                .bind(a.id)
                                .bind(i64::MAX / 4)
                                .bind(PRICE_SCALE as i64)
                                .bind(address)
                                .bind(i32::MAX)
                                .execute(&mut *c)
                                .await?;
                        }
                    }
                    events.push(ev("SetReceiveAddress", json!({"account": hex::encode(&acct), "chain": ch, "address": address})));
                }

                TxBody::Liquidate { issuer, asset, quantity } => {
                    let a = ledger::ensure_asset(asset)?;
                    if a.chain == "blrd" || *quantity == 0 {
                        bail!("nothing to liquidate");
                    }
                    let issued_v = issued_value(c, issuer, e.height).await?;
                    let stake_v = stake_value(c, issuer, e.height).await?;
                    let need = required_ratio_bps(c, e.height).await?;
                    if issued_v == 0 || stake_v * 10_000 >= issued_v * need {
                        bail!("this issuer is collateralized at the required {}%", need / 100);
                    }
                    // Options are fungible: any holder's options are destroyed and this issuer owes less.
                    let q = i(*quantity)?;
                    let issued = ledger::issued(c, issuer, asset).await?;
                    if issued < q {
                        bail!("this issuer has only {issued} {asset} outstanding");
                    }
                    ledger::debit(c, &acct, asset, q).await?;
                    ledger::set_issued(c, issuer, asset, issued - q).await?;
                    // Paid in BLRD from the stake at a premium over the oracle value.
                    let p_asset = oracle_price(c, asset, e.height).await?.ok_or_else(|| anyhow!("no price for {asset}"))? as i128;
                    let p_blrd = oracle_price(c, BLRD, e.height).await?.unwrap_or(GENESIS_BLRD_PRICE) as i128;
                    let value = q as i128 * p_asset / PRICE_SCALE as i128;
                    let (_, _, _, staked, unstaking, _) = account_full(c, issuer).await?;
                    let owed = value * LIQUIDATION_PREMIUM_BPS / 10_000;
                    let out = (owed * PRICE_SCALE as i128 / p_blrd.max(1)).min((staked + unstaking) as i128) as i64;
                    let from_staked = out.min(staked);
                    sqlx::query("update accounts set staked = staked - $2, unstaking = unstaking - $3 where pubkey = $1").bind(issuer).bind(from_staked).bind(out - from_staked).execute(&mut *c).await?;
                    credit(c, &acct, out).await?;
                    let rest = owed - out as i128 * p_blrd / PRICE_SCALE as i128;
                    if rest > 0 {
                        pay_from_staked_lots(c, issuer, &acct, rest, e.height).await?;
                    }
                    events.push(ev("Liquidate", json!({"issuer": hex::encode(issuer), "by": hex::encode(&acct), "asset": asset, "quantity": quantity, "blrd": out})));
                }

                TxBody::Burn { asset, quantity } => {
                    ledger::ensure_asset(asset)?;
                    let q = i(*quantity)?;
                    let issued = ledger::issued(c, &acct, asset).await?;
                    if issued < q {
                        bail!("you have issued only {issued} {asset}");
                    }
                    ledger::debit(c, &acct, asset, q).await?;
                    ledger::set_issued(c, &acct, asset, issued - q).await?;
                    events.push(ev("Burn", json!({"issuer": hex::encode(&acct), "asset": asset, "quantity": quantity})));
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

                TxBody::Exercise { asset, quantity, receive_address, issuer: wanted } => {
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
                    ledger::debit(c, &acct, asset, i(*quantity)?).await?;
                    // Options are fungible, so the ledger decides who delivers, and one maker delivers the lot:
                    // the thinnest issuer that has enough outstanding, else the largest. Whatever the deliverer
                    // did not issue itself it takes over from the other issuers, who pay it for that in BLRD
                    // from their stakes at the oracle price. The holder gets a single payment.
                    let q = i(*quantity)?;
                    let (deliverer, taken_over) = if wanted.is_empty() {
                        assign_deliverer(c, asset, q, e.height).await?
                    } else {
                        // The holder called a particular maker: it must be able to cover the whole thing itself.
                        let outstanding = ledger::issued(c, wanted, asset).await?;
                        if outstanding < q {
                            bail!("that issuer has only {outstanding} {asset} outstanding");
                        }
                        (wanted.clone(), Vec::new())
                    };
                    let maker_source = ledger::source(c, &deliverer, asset).await?;
                    let p_asset = oracle_price(c, asset, e.height).await?.unwrap_or(0) as i128;
                    let p_blrd = oracle_price(c, BLRD, e.height).await?.unwrap_or(GENESIS_BLRD_PRICE) as i128;
                    let id = e.hash.to_vec();
                    for (other, part) in &taken_over {
                        // The obligation moves now, so the deliverer's stake backs the whole exercise at once.
                        // The payment for it waits until the holder has been paid and the delivery is deep.
                        let oi = ledger::issued(c, other, asset).await?;
                        ledger::set_issued(c, other, asset, oi - part).await?;
                        let di = ledger::issued(c, &deliverer, asset).await?;
                        ledger::set_issued(c, &deliverer, asset, di + part).await?;
                        let value = *part as i128 * p_asset / PRICE_SCALE as i128;
                        sqlx::query("insert into exercise_shares (match_id, issuer, quantity, value) values ($1, $2, $3, $4)").bind(&id).bind(other).bind(*part).bind(value as i64).execute(&mut *c).await?;
                        events.push(ev("ObligationMoved", json!({"asset": asset, "from": hex::encode(other), "to": hex::encode(&deliverer), "quantity": part, "value_usdc": value as i64})));
                    }
                    let _ = p_blrd;
                    let issued = ledger::issued(c, &deliverer, asset).await?;
                    ledger::set_issued(c, &deliverer, asset, issued - q).await?;
                    let bond = reserve_stake_worth(c, &deliverer, &id, asset, q, e.height).await?;
                    sqlx::query("insert into matches (id, seq, order_id, maker, taker, market, side, quantity, price, bond, claim_bond, fill_policy, taker_chain, taker_asset, taker_amount, taker_pay_to, maker_chain, maker_asset, maker_amount, maker_pay_to, status, claimed_ts, claimed_height, taker_txid, taker_included_height, kind, maker_source) values ($1,$2,$1,$3,$4,$5,1,$6,0,$7,0,1,'ledger',$5,$6,'',$8,$5,$6,$9,'open',$10,$11,'exercised',$11,'exercise',$12)")
                        .bind(&id)
                        .bind(seq as i64)
                        .bind(&deliverer)
                        .bind(&acct)
                        .bind(asset)
                        .bind(q)
                        .bind(bond)
                        .bind(a.chain)
                        .bind(receive_address)
                        .bind(e.time as i64)
                        .bind(e.height as i32)
                        .bind(&maker_source)
                        .execute(&mut *c)
                        .await?;
                    events.push(ev("Exercise", json!({"match": hex::encode(&id), "account": hex::encode(&acct), "issuer": hex::encode(&deliverer), "asset": asset, "quantity": q, "bond": bond, "to": receive_address})));
                }

                TxBody::PlaceOrder { market, side, quantity, price, bond: _, claim_bond, fill_policy, receive_address, expiry_height, settle, mint } => {
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
                    if *settle == 0 && base.id == chain::QUOTE {
                        bail!("{} is the quote of every ledger market; the base must be another asset", chain::QUOTE);
                    }
                    if *settle == 0 && quote.id != chain::QUOTE {
                        bail!("ledger markets are quoted in {}: use {}/{}", chain::QUOTE, base.id, chain::QUOTE);
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
                    if *mint {
                        if *settle != 1 || *side != 1 {
                            bail!("a minting order sells its base for on-chain payment");
                        }
                        if base.chain == "blrd" {
                            bail!("BLRD cannot be minted");
                        }
                    }
                    // What the maker sets aside: the base it sells, or the quote it buys with. A minting order sets nothing aside yet.
                    let (escrow_asset, escrow_amount) = if *side == 1 { (base.id, qty) } else { (quote.id, quote_amount) };
                    let escrow_amount = if *mint { 0 } else { escrow_amount };
                    escrow(c, &acct, &e.hash, escrow_asset, escrow_amount).await?;
                    sqlx::query("insert into orders (id, seq, account, market, base, quote, side, quantity, qty_left, price, bond, bond_left, claim_bond, fill_policy, recv_addr, expiry_height, status, settle, escrow_asset, escrow_left, mint) values ($1,$2,$3,$4,$5,$6,$7,$8,$8,$9,0,0,$10,$11,$12,$13,'open',$14,$15,$16,$17)")
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
                        .bind(*mint)
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
                        if o.mint {
                            // Minted now, against the maker's stake, free BLRD being staked as needed.
                            let issued = ledger::issued(c, &o.account, base.id).await?;
                            if ledger::source(c, &o.account, base.id).await?.is_empty() {
                                ledger::set_source(c, &o.account, base.id, &o.recv_addr).await?;
                            }
                            ledger::set_issued(c, &o.account, base.id, issued + qty).await?;
                            ledger::credit(c, &o.id, base.id, ledger::NO_ISSUER, qty).await?;
                            sqlx::query("update orders set escrow_left = escrow_left + $2 where id = $1").bind(&o.id).bind(qty).execute(&mut *c).await?;
                            auto_stake(c, &o.account, e.height).await?;
                            check_collateral(c, &o.account, e.height).await?;
                        }
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
            // The reserved stake, BLRD and any staked options, goes to the exerciser; the other issuers cover any gap.
            let p_asset = oracle_price(c, &m.maker_asset, e.height).await?.unwrap_or(0) as i128;
            let p_blrd = oracle_price(c, BLRD, e.height).await?.unwrap_or(GENESIS_BLRD_PRICE) as i128;
            let owed = m.quantity as i128 * p_asset / PRICE_SCALE as i128;
            let held: Option<i64> = sqlx::query_scalar("select sum(amount)::bigint from balances where account = $1").bind(&m.id).fetch_one(&mut *c).await?;
            let lots_value = { let rows: Vec<(String, i64)> = sqlx::query_as("select asset, sum(amount)::bigint from balances where account = $1 group by asset").bind(&m.id).fetch_all(&mut *c).await?; let mut v: i128 = 0; for (asset, amt) in rows { v += amt as i128 * oracle_price(c, &asset, e.height).await?.unwrap_or(0) as i128 / PRICE_SCALE as i128; } v };
            let _ = held;
            credit(c, &m.taker, m.bond).await?;
            move_all_lots(c, &m.id, &m.taker).await?;
            let shortfall = owed - m.bond as i128 * p_blrd / PRICE_SCALE as i128 - lots_value;
            if shortfall > 0 {
                socialize_shortfall(c, &m.maker_asset, &m.maker, shortfall, &m.taker, e.height).await?;
            }
            // A deliverer that failed is not paid for the shares it took over.
            sqlx::query("delete from exercise_shares where match_id = $1").bind(&m.id).execute(&mut *c).await?;
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
            if payer == m.maker {
                // Delivered: the stake goes back into the pool.
                sqlx::query("update accounts set staked = staked + $2 where pubkey = $1").bind(&m.maker).bind(m.bond - fee).execute(&mut *c).await?;
                move_all_lots(c, &m.id, &stake_account(&m.maker)).await?;
            } else {
                credit(c, &payer, m.bond - fee).await?;
                move_all_lots(c, &m.id, &payer).await?;
            }
            credit(c, &e.leader, fee).await?;
            // Now that the holder has been paid, the other issuers pay the deliverer for the shares it took over.
            let shares: Vec<(Vec<u8>, i64, i64)> = sqlx::query_as("select issuer, quantity, value from exercise_shares where match_id = $1").bind(&m.id).fetch_all(&mut *c).await?;
            let p_blrd = oracle_price(c, BLRD, e.height).await?.unwrap_or(GENESIS_BLRD_PRICE) as i128;
            for (issuer, _qty, value) in shares {
                let want = (value as i128 * PRICE_SCALE as i128 / p_blrd.max(1)) as i64;
                let (_, _, _, st, un, _) = account_full(c, &issuer).await?;
                let blrd = want.min(st + un);
                let from_st = blrd.min(st);
                sqlx::query("update accounts set staked = staked - $2, unstaking = unstaking - $3 where pubkey = $1").bind(&issuer).bind(from_st).bind(blrd - from_st).execute(&mut *c).await?;
                if payer == m.maker {
                    sqlx::query("update accounts set staked = staked + $2 where pubkey = $1").bind(&payer).bind(blrd).execute(&mut *c).await?;
                } else {
                    credit(c, &payer, blrd).await?;
                }
                let rest = value as i128 - blrd as i128 * p_blrd / PRICE_SCALE as i128;
                if rest > 0 {
                    let to = if payer == m.maker { stake_account(&payer) } else { payer.clone() };
                    pay_from_staked_lots(c, &issuer, &to, rest, e.height).await?;
                }
            }
            sqlx::query("delete from exercise_shares where match_id = $1").bind(&m.id).execute(&mut *c).await?;
            close_match(c, &m.id, "closed", "delivered", seq).await?;
            events.push(ev("MatchClosed", json!({"match": hex::encode(&m.id), "bond_to": hex::encode(&payer), "fee": fee})));
        }

        Body::Liquidated { issuer } => {
            // Checkable by every node from state alone: the issuer must be under the floor.
            let issued = issued_value(c, issuer, e.height).await?;
            let stake = stake_value(c, issuer, e.height).await?;
            if issued == 0 || stake * 10_000 >= issued * LIQUIDATION_FLOOR_BPS {
                bail!("issuer is not under the liquidation floor");
            }
            let height = e.height;
            let need = required_ratio_bps(c, height).await?;
            let p_blrd = oracle_price(c, BLRD, height).await?.unwrap_or(GENESIS_BLRD_PRICE) as i128;
            let (_, _, _, staked, unstaking, _) = account_full(c, issuer).await?;
            let stake_blrd = staked + unstaking;
            let mut moved = Vec::new();
            // Each asset's outstanding options pass to that asset's other issuers, up to their spare capacity,
            // and the defaulter's stake follows in the same proportion. Anything nobody can absorb stays with
            // the defaulter, to be settled by exercise and shortfall sharing.
            let assets: Vec<(String, i64)> = sqlx::query_as("select asset, issued from issuers where account = $1 and issued > 0").bind(issuer).fetch_all(&mut *c).await?;
            let total_value = issued.max(1);
            for (asset, outstanding) in assets {
                let p_asset = oracle_price(c, &asset, height).await?.unwrap_or(0) as i128;
                let others: Vec<(Vec<u8>, i64)> = sqlx::query_as("select account, issued from issuers where asset = $1 and issued > 0 and account <> $2").bind(&asset).bind(issuer).fetch_all(&mut *c).await?;
                let mut left = outstanding;
                for (other, _) in others {
                    if left <= 0 {
                        break;
                    }
                    let ov = issued_value(c, &other, height).await.unwrap_or(0);
                    let sv = stake_value(c, &other, height).await?;
                    let cap_value = (sv * 10_000 / need - ov).max(0);
                    let cap_qty = if p_asset > 0 { (cap_value * PRICE_SCALE as i128 / p_asset) as i64 } else { 0 };
                    let take = left.min(cap_qty);
                    if take <= 0 {
                        continue;
                    }
                    let oi = ledger::issued(c, &other, &asset).await?;
                    ledger::set_issued(c, &other, &asset, oi + take).await?;
                    left -= take;
                    // The defaulter's stake travels with the debt, in proportion to its value.
                    let share_value = take as i128 * p_asset / PRICE_SCALE as i128;
                    let blrd = (stake_blrd as i128 * share_value / total_value).min(stake_blrd as i128) as i64;
                    if blrd > 0 {
                        let (_, _, _, st, un, _) = account_full(c, issuer).await?;
                        let b = blrd.min(st + un);
                        let from_st = b.min(st);
                        sqlx::query("update accounts set staked = staked - $2, unstaking = unstaking - $3 where pubkey = $1").bind(issuer).bind(from_st).bind(b - from_st).execute(&mut *c).await?;
                        sqlx::query("update accounts set staked = staked + $2 where pubkey = $1").bind(&other).bind(b).execute(&mut *c).await?;
                    }
                    let lots_value = staked_lots_value(c, issuer, height).await?;
                    if lots_value > 0 {
                        pay_from_staked_lots(c, issuer, &stake_account(&other), lots_value * share_value / total_value, height).await?;
                    }
                    moved.push(json!({"asset": asset, "to": hex::encode(&other), "quantity": take, "blrd": blrd}));
                }
                let si = ledger::issued(c, issuer, &asset).await?;
                ledger::set_issued(c, issuer, &asset, si - (outstanding - left)).await?;
            }
            events.push(ev("Liquidated", json!({"issuer": hex::encode(issuer), "moved": moved, "price_blrd": p_blrd as i64})));
        }

        Body::Handoff { reason, replaced } => {
            events.push(ev("Handoff", json!({"reason": reason, "replaced": hex::encode(replaced), "leader": hex::encode(e.leader)})));
        }
    }
    Ok(events)
}

/// Wipe derived state so the log can be replayed.
pub async fn reset_state(c: &mut PgConnection) -> Result<()> {
    sqlx::query("truncate accounts, orders, matches, balances, issuers, transfers, exercise_shares").execute(&mut *c).await?;
    sqlx::query("update epochs set leader_key = null, first_seq = null, last_seq = null").execute(c).await?;
    Ok(())
}
