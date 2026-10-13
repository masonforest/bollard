//! `bollard enter`: put a lottery entry on Bitcoin. One transaction from the
//! node's own Bitcoin address with a 75-byte OP_RETURN: our key hash, where we
//! can be reached, and the newest log entry we hold. The fee is the weight.

use crate::crypto::{Keypair, SECP};
use crate::lottery::Payload;
use anyhow::{anyhow, bail, Result};
use bitcoin::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::script::PushBytesBuf;
use bitcoin::secp256k1::Message;
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::transaction::Version;
use bitcoin::{Address, Amount, CompressedPublicKey, Network, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};
use clap::Args;
use serde::Deserialize;

#[derive(Args, Debug)]
pub struct EnterArgs {
    /// Node secret key, 32 bytes hex
    #[arg(long, env = "BOLLARD_KEY")]
    pub key: String,
    /// Fee in satoshis. This is the entry's weight in the draw.
    #[arg(long)]
    pub fee: u64,
    /// Public IP other nodes reach this node at
    #[arg(long, env = "PUBLIC_IP")]
    pub public_ip: String,
    /// Public port
    #[arg(long, env = "PUBLIC_PORT", default_value_t = 8080)]
    pub public_port: u16,
    /// Postgres connection string, to read the log tip to commit to
    #[arg(long, env = "DATABASE_URL")]
    pub database_url: Option<String>,
    /// Commit to this tip instead of reading the database (hex, 32 bytes; zeros at genesis)
    #[arg(long)]
    pub tip: Option<String>,
    #[arg(long, env = "BITCOIN_RPC", default_value = "https://bitcoin.publicnode.com")]
    pub bitcoin_rpc: String,
    /// Esplora-style API for listing this address's coins
    #[arg(long, env = "ESPLORA_API", default_value = "https://mempool.space/api")]
    pub esplora: String,
    /// Build and print the transaction without broadcasting it
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Deserialize)]
struct Utxo {
    txid: String,
    vout: u32,
    value: u64,
    status: UtxoStatus,
}

#[derive(Deserialize)]
struct UtxoStatus {
    confirmed: bool,
}

pub fn address_of(kp: &Keypair) -> Address {
    Address::p2wpkh(&CompressedPublicKey(kp.pk), Network::Bitcoin)
}

pub async fn enter(a: EnterArgs) -> Result<()> {
    let kp = Keypair::from_hex(&a.key)?;
    let addr = address_of(&kp);
    println!("node key hash: {}", hex::encode(kp.key_hash()));
    println!("bitcoin address: {addr}");

    let tip: [u8; 32] = match (&a.tip, &a.database_url) {
        (Some(t), _) => hex::decode(t)?.try_into().map_err(|_| anyhow!("tip must be 32 bytes"))?,
        (None, Some(url)) => {
            let pool = sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(url).await?;
            crate::db::head(&pool).await?.map(|(_, h, _)| h).unwrap_or([0u8; 32])
        }
        (None, None) => [0u8; 32],
    };
    println!("committing to tip: {}", hex::encode(tip));

    let utxos: Vec<Utxo> = reqwest::get(format!("{}/address/{addr}/utxo", a.esplora.trim_end_matches('/'))).await?.error_for_status()?.json().await?;
    let utxos: Vec<Utxo> = utxos.into_iter().filter(|u| u.status.confirmed).collect();
    let total: u64 = utxos.iter().map(|u| u.value).sum();
    if utxos.is_empty() {
        bail!("no confirmed coins at {addr}. Send some bitcoin there (the fee plus a little change), wait a confirmation, and run this again.");
    }
    if total < a.fee + 546 {
        bail!("{addr} holds {total} sats; the fee of {} plus 546 sats of change needs more", a.fee);
    }

    let payload = Payload { key_hash: kp.key_hash(), ip: a.public_ip.parse()?, port: a.public_port, tip };
    let data = PushBytesBuf::try_from(payload.encode().to_vec()).map_err(|_| anyhow!("payload too long"))?;
    let spk = addr.script_pubkey();
    let mut tx = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: utxos
            .iter()
            .map(|u| TxIn {
                previous_output: OutPoint { txid: u.txid.parse::<Txid>().expect("txid"), vout: u.vout },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            })
            .collect(),
        output: vec![
            TxOut { value: Amount::ZERO, script_pubkey: ScriptBuf::new_op_return(&data) },
            TxOut { value: Amount::from_sat(total - a.fee), script_pubkey: spk.clone() },
        ],
    };
    let mut witnesses = Vec::with_capacity(tx.input.len());
    {
        let mut cache = SighashCache::new(&tx);
        for (i, u) in utxos.iter().enumerate() {
            let sighash = cache.p2wpkh_signature_hash(i, &spk, Amount::from_sat(u.value), EcdsaSighashType::All)?;
            let sig = SECP.sign_ecdsa(&Message::from_digest(sighash.to_byte_array()), &kp.sk);
            let sig = bitcoin::ecdsa::Signature { signature: sig, sighash_type: EcdsaSighashType::All };
            witnesses.push(Witness::p2wpkh(&sig, &kp.pk));
        }
    }
    for (i, w) in witnesses.into_iter().enumerate() {
        tx.input[i].witness = w;
    }
    let raw = bitcoin::consensus::encode::serialize_hex(&tx);
    println!("entry txid: {}", tx.compute_txid());
    println!("fee (weight): {} sats, change: {} sats", a.fee, total - a.fee);
    if a.dry_run {
        println!("raw: {raw}");
        return Ok(());
    }
    let rpc = crate::chain::btc::BtcRpc::new(&a.bitcoin_rpc);
    let txid = rpc.send_raw_transaction(&raw).await?;
    println!("broadcast: {txid}");
    println!("the draw happens in the block that includes it; watch /entries and /epochs on your node");
    Ok(())
}
