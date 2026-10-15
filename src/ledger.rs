//! Option balances: who holds how many delivery promises on which asset from
//! which issuer. BLRD lives in `accounts`; everything else lives here. Lots
//! are kept per issuer so an exercise knows whom to call on.

use anyhow::{anyhow, bail, Result};
use sqlx::PgConnection;

/// Credit `amount` of `asset` issued by `issuer` to `account`.
pub async fn credit(c: &mut PgConnection, account: &[u8], asset: &str, issuer: &[u8], amount: i64) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    sqlx::query("insert into balances (account, asset, issuer, amount) values ($1, $2, $3, $4) on conflict (account, asset, issuer) do update set amount = balances.amount + $4")
        .bind(account)
        .bind(asset)
        .bind(issuer)
        .bind(amount)
        .execute(c)
        .await?;
    Ok(())
}

/// Total of `asset` held by `account` across issuers.
pub async fn total(c: &mut PgConnection, account: &[u8], asset: &str) -> Result<i64> {
    let t: Option<i64> = sqlx::query_scalar("select sum(amount)::bigint from balances where account = $1 and asset = $2").bind(account).bind(asset).fetch_one(c).await?;
    Ok(t.unwrap_or(0))
}

/// Take `amount` of `asset` from `account`, oldest issuer lots first, and
/// return the (issuer, amount) lots taken.
pub async fn debit(c: &mut PgConnection, account: &[u8], asset: &str, amount: i64) -> Result<Vec<(Vec<u8>, i64)>> {
    if amount <= 0 {
        bail!("amount must be positive");
    }
    let have = total(c, account, asset).await?;
    if have < amount {
        bail!("insufficient {asset}: have {have}, need {amount}");
    }
    let lots: Vec<(Vec<u8>, i64)> = sqlx::query_as("select issuer, amount from balances where account = $1 and asset = $2 and amount > 0 order by issuer").bind(account).bind(asset).fetch_all(&mut *c).await?;
    let mut left = amount;
    let mut taken = Vec::new();
    for (issuer, lot) in lots {
        if left == 0 {
            break;
        }
        let take = lot.min(left);
        sqlx::query("update balances set amount = amount - $4 where account = $1 and asset = $2 and issuer = $3").bind(account).bind(asset).bind(&issuer).bind(take).execute(&mut *c).await?;
        taken.push((issuer, take));
        left -= take;
    }
    sqlx::query("delete from balances where account = $1 and asset = $2 and amount = 0").bind(account).bind(asset).execute(c).await?;
    Ok(taken)
}

/// Move lots from one account to another, preserving issuers.
pub async fn transfer(c: &mut PgConnection, from: &[u8], to: &[u8], asset: &str, amount: i64) -> Result<()> {
    for (issuer, n) in debit(c, from, asset, amount).await? {
        credit(c, to, asset, &issuer, n).await?;
    }
    Ok(())
}

pub async fn issuer_row(c: &mut PgConnection, account: &[u8], asset: &str) -> Result<(i64, i64)> {
    let row: Option<(i64, i64)> = sqlx::query_as("select issued, cover from issuers where account = $1 and asset = $2").bind(account).bind(asset).fetch_optional(c).await?;
    Ok(row.unwrap_or((0, 0)))
}

pub async fn set_issuer(c: &mut PgConnection, account: &[u8], asset: &str, issued: i64, cover: i64) -> Result<()> {
    sqlx::query("insert into issuers (account, asset, issued, cover) values ($1, $2, $3, $4) on conflict (account, asset) do update set issued = $3, cover = $4")
        .bind(account)
        .bind(asset)
        .bind(issued)
        .bind(cover)
        .execute(c)
        .await?;
    Ok(())
}

/// The share of an issuer's cover that backs `quantity` of its outstanding options.
pub fn cover_for(issued: i64, cover: i64, quantity: i64) -> i64 {
    if issued <= 0 {
        return 0;
    }
    ((cover as i128 * quantity as i128) / issued as i128) as i64
}

pub fn no_issuer() -> Vec<u8> {
    Vec::new()
}

pub fn ensure_asset(asset: &str) -> Result<&'static crate::chain::AssetInfo> {
    crate::chain::asset(asset).ok_or_else(|| anyhow!("unknown asset {asset}"))
}
