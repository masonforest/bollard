//! Wire format helpers. The bytes a user or leader signed are canonical: they
//! are hashed, stored and relayed as-is. Parsing verifies both the buffer and
//! the signature.

use crate::crypto::{self, Keypair};
use crate::fb;
use anyhow::{anyhow, bail, Result};
use flatbuffers::FlatBufferBuilder;
use serde::Serialize;

pub const ZERO32: [u8; 32] = [0u8; 32];

pub fn hex_bytes<T: AsRef<[u8]>, S: serde::Serializer>(v: &T, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&hex::encode(v))
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type")]
pub enum TxBody {
    Transfer {
        #[serde(serialize_with = "hex_bytes")]
        to: Vec<u8>,
        amount: u64,
    },
    PlaceOrder {
        market: String,
        side: u8,
        quantity: u64,
        price: u64,
        bond: u64,
        claim_bond: u64,
        fill_policy: u8,
        receive_address: String,
        expiry_height: u32,
    },
    CancelOrder {
        #[serde(serialize_with = "hex_bytes")]
        order_id: Vec<u8>,
    },
    Claim {
        #[serde(serialize_with = "hex_bytes")]
        order_id: Vec<u8>,
        quantity: u64,
        receive_address: String,
    },
    ReportPayment {
        #[serde(serialize_with = "hex_bytes")]
        match_id: Vec<u8>,
        leg: u8,
        txid: String,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct ParsedTx {
    #[serde(skip)]
    pub tx_bytes: Vec<u8>,
    #[serde(skip)]
    pub sig: Vec<u8>,
    #[serde(serialize_with = "hex_bytes")]
    pub hash: [u8; 32],
    #[serde(serialize_with = "hex_bytes")]
    pub account: [u8; 33],
    pub nonce: u64,
    pub body: TxBody,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind")]
pub enum Body {
    Tx(ParsedTx),
    EpochStart { height: u32, entry_txid: String },
    ClaimExpired { #[serde(serialize_with = "hex_bytes")] match_id: [u8; 32] },
    LegIncluded { #[serde(serialize_with = "hex_bytes")] match_id: [u8; 32], leg: u8, txid: String, block_hash: String, block_height: u64 },
    ClaimVoid { #[serde(serialize_with = "hex_bytes")] match_id: [u8; 32], reason: u8 },
    LegMissed { #[serde(serialize_with = "hex_bytes")] match_id: [u8; 32], leg: u8 },
    MatchClosed { #[serde(serialize_with = "hex_bytes")] match_id: [u8; 32] },
    Handoff { reason: String, #[serde(serialize_with = "hex_bytes")] replaced: Vec<u8> },
}

#[derive(Clone, Debug, Serialize)]
pub struct ParsedEntry {
    #[serde(skip)]
    pub bytes: Vec<u8>,
    #[serde(skip)]
    pub content: Vec<u8>,
    #[serde(serialize_with = "hex_bytes")]
    pub hash: [u8; 32],
    pub seq: u64,
    #[serde(serialize_with = "hex_bytes")]
    pub prev: [u8; 32],
    pub height: u32,
    pub time: u64,
    #[serde(serialize_with = "hex_bytes")]
    pub leader: [u8; 33],
    pub body: Body,
}

impl ParsedEntry {
    pub fn kind(&self) -> &'static str {
        match &self.body {
            Body::Tx(t) => match t.body {
                TxBody::Transfer { .. } => "Transfer",
                TxBody::PlaceOrder { .. } => "PlaceOrder",
                TxBody::CancelOrder { .. } => "CancelOrder",
                TxBody::Claim { .. } => "Claim",
                TxBody::ReportPayment { .. } => "ReportPayment",
            },
            Body::EpochStart { .. } => "EpochStart",
            Body::ClaimExpired { .. } => "ClaimExpired",
            Body::LegIncluded { .. } => "LegIncluded",
            Body::ClaimVoid { .. } => "ClaimVoid",
            Body::LegMissed { .. } => "LegMissed",
            Body::MatchClosed { .. } => "MatchClosed",
            Body::Handoff { .. } => "Handoff",
        }
    }
}

fn bytes<'a>(v: Option<flatbuffers::Vector<'a, u8>>) -> &'a [u8] {
    v.map(|v| v.bytes()).unwrap_or(&[])
}

fn a32(b: &[u8]) -> Result<[u8; 32]> {
    b.try_into().map_err(|_| anyhow!("expected 32 bytes, got {}", b.len()))
}

fn a33(b: &[u8]) -> Result<[u8; 33]> {
    b.try_into().map_err(|_| anyhow!("expected a 33-byte public key, got {} bytes", b.len()))
}

pub fn build_tx(account: &[u8; 33], nonce: u64, body: &TxBody) -> Vec<u8> {
    let mut b = FlatBufferBuilder::with_capacity(256);
    let acct = b.create_vector(account);
    let (bt, bo) = match body {
        TxBody::Transfer { to, amount } => {
            let to = b.create_vector(to);
            (fb::TxBody::Transfer, fb::Transfer::create(&mut b, &fb::TransferArgs { to: Some(to), amount: *amount }).as_union_value())
        }
        TxBody::PlaceOrder { market, side, quantity, price, bond, claim_bond, fill_policy, receive_address, expiry_height } => {
            let market = b.create_string(market);
            let ra = b.create_string(receive_address);
            let args = fb::PlaceOrderArgs {
                market: Some(market),
                side: *side,
                quantity: *quantity,
                price: *price,
                bond: *bond,
                claim_bond: *claim_bond,
                fill_policy: *fill_policy,
                receive_address: Some(ra),
                expiry_height: *expiry_height,
            };
            (fb::TxBody::PlaceOrder, fb::PlaceOrder::create(&mut b, &args).as_union_value())
        }
        TxBody::CancelOrder { order_id } => {
            let o = b.create_vector(order_id);
            (fb::TxBody::CancelOrder, fb::CancelOrder::create(&mut b, &fb::CancelOrderArgs { order_id: Some(o) }).as_union_value())
        }
        TxBody::Claim { order_id, quantity, receive_address } => {
            let o = b.create_vector(order_id);
            let ra = b.create_string(receive_address);
            let args = fb::ClaimArgs { order_id: Some(o), quantity: *quantity, receive_address: Some(ra) };
            (fb::TxBody::Claim, fb::Claim::create(&mut b, &args).as_union_value())
        }
        TxBody::ReportPayment { match_id, leg, txid } => {
            let m = b.create_vector(match_id);
            let t = b.create_string(txid);
            let args = fb::ReportPaymentArgs { match_id: Some(m), leg: *leg, txid: Some(t) };
            (fb::TxBody::ReportPayment, fb::ReportPayment::create(&mut b, &args).as_union_value())
        }
    };
    let tx = fb::Tx::create(&mut b, &fb::TxArgs { account: Some(acct), nonce, body_type: bt, body: Some(bo) });
    b.finish(tx, None);
    b.finished_data().to_vec()
}

pub fn sign_tx(tx_bytes: &[u8], kp: &Keypair) -> Vec<u8> {
    let sig = kp.sign(&crypto::sha256(tx_bytes));
    let mut b = FlatBufferBuilder::with_capacity(tx_bytes.len() + 128);
    let t = b.create_vector(tx_bytes);
    let sg = b.create_vector(&sig);
    let st = fb::SignedTx::create(&mut b, &fb::SignedTxArgs { tx: Some(t), sig: Some(sg) });
    b.finish(st, None);
    b.finished_data().to_vec()
}

fn parse_tx_body(tx: &fb::Tx) -> Result<TxBody> {
    Ok(match tx.body_type() {
        fb::TxBody::Transfer => {
            let t = tx.body_as_transfer().ok_or_else(|| anyhow!("bad Transfer"))?;
            TxBody::Transfer { to: bytes(t.to()).to_vec(), amount: t.amount() }
        }
        fb::TxBody::PlaceOrder => {
            let t = tx.body_as_place_order().ok_or_else(|| anyhow!("bad PlaceOrder"))?;
            TxBody::PlaceOrder {
                market: t.market().unwrap_or("").to_string(),
                side: t.side(),
                quantity: t.quantity(),
                price: t.price(),
                bond: t.bond(),
                claim_bond: t.claim_bond(),
                fill_policy: t.fill_policy(),
                receive_address: t.receive_address().unwrap_or("").to_string(),
                expiry_height: t.expiry_height(),
            }
        }
        fb::TxBody::CancelOrder => {
            let t = tx.body_as_cancel_order().ok_or_else(|| anyhow!("bad CancelOrder"))?;
            TxBody::CancelOrder { order_id: bytes(t.order_id()).to_vec() }
        }
        fb::TxBody::Claim => {
            let t = tx.body_as_claim().ok_or_else(|| anyhow!("bad Claim"))?;
            TxBody::Claim { order_id: bytes(t.order_id()).to_vec(), quantity: t.quantity(), receive_address: t.receive_address().unwrap_or("").to_string() }
        }
        fb::TxBody::ReportPayment => {
            let t = tx.body_as_report_payment().ok_or_else(|| anyhow!("bad ReportPayment"))?;
            TxBody::ReportPayment { match_id: bytes(t.match_id()).to_vec(), leg: t.leg(), txid: t.txid().unwrap_or("").to_string() }
        }
        _ => bail!("unknown transaction body"),
    })
}

fn parse_tx_parts(tx_bytes: Vec<u8>, sig: Vec<u8>) -> Result<ParsedTx> {
    let (account, nonce, body) = {
        let tx = flatbuffers::root::<fb::Tx>(&tx_bytes).map_err(|e| anyhow!("bad Tx: {e}"))?;
        (a33(bytes(tx.account()))?, tx.nonce(), parse_tx_body(&tx)?)
    };
    let hash = crypto::sha256(&tx_bytes);
    if !crypto::verify(&account, &hash, &sig) {
        bail!("bad transaction signature");
    }
    Ok(ParsedTx { tx_bytes, sig, hash, account, nonce, body })
}

/// Parse and verify a SignedTx buffer as submitted by a user.
pub fn parse_signed_tx(signed: &[u8]) -> Result<ParsedTx> {
    let st = flatbuffers::root::<fb::SignedTx>(signed).map_err(|e| anyhow!("bad SignedTx: {e}"))?;
    parse_tx_parts(bytes(st.tx()).to_vec(), bytes(st.sig()).to_vec())
}

pub fn build_entry_content(seq: u64, prev: &[u8; 32], height: u32, time: u64, leader: &[u8; 33], body: &Body) -> Vec<u8> {
    let mut b = FlatBufferBuilder::with_capacity(512);
    let prev = b.create_vector(prev);
    let leader_v = b.create_vector(leader);
    let (bt, bo) = match body {
        Body::Tx(t) => {
            let tx = b.create_vector(&t.tx_bytes);
            let sig = b.create_vector(&t.sig);
            (fb::EntryBody::SignedTx, fb::SignedTx::create(&mut b, &fb::SignedTxArgs { tx: Some(tx), sig: Some(sig) }).as_union_value())
        }
        Body::EpochStart { height, entry_txid } => {
            let t = b.create_string(entry_txid);
            (fb::EntryBody::EpochStart, fb::EpochStart::create(&mut b, &fb::EpochStartArgs { height: *height, entry_txid: Some(t) }).as_union_value())
        }
        Body::ClaimExpired { match_id } => {
            let m = b.create_vector(match_id);
            (fb::EntryBody::ClaimExpired, fb::ClaimExpired::create(&mut b, &fb::ClaimExpiredArgs { match_id: Some(m) }).as_union_value())
        }
        Body::LegIncluded { match_id, leg, txid, block_hash, block_height } => {
            let m = b.create_vector(match_id);
            let t = b.create_string(txid);
            let bh = b.create_string(block_hash);
            let args = fb::LegIncludedArgs { match_id: Some(m), leg: *leg, txid: Some(t), block_hash: Some(bh), block_height: *block_height };
            (fb::EntryBody::LegIncluded, fb::LegIncluded::create(&mut b, &args).as_union_value())
        }
        Body::ClaimVoid { match_id, reason } => {
            let m = b.create_vector(match_id);
            (fb::EntryBody::ClaimVoid, fb::ClaimVoid::create(&mut b, &fb::ClaimVoidArgs { match_id: Some(m), reason: *reason }).as_union_value())
        }
        Body::LegMissed { match_id, leg } => {
            let m = b.create_vector(match_id);
            (fb::EntryBody::LegMissed, fb::LegMissed::create(&mut b, &fb::LegMissedArgs { match_id: Some(m), leg: *leg }).as_union_value())
        }
        Body::MatchClosed { match_id } => {
            let m = b.create_vector(match_id);
            (fb::EntryBody::MatchClosed, fb::MatchClosed::create(&mut b, &fb::MatchClosedArgs { match_id: Some(m) }).as_union_value())
        }
        Body::Handoff { reason, replaced } => {
            let r = b.create_string(reason);
            let rp = b.create_vector(replaced);
            (fb::EntryBody::Handoff, fb::Handoff::create(&mut b, &fb::HandoffArgs { reason: Some(r), replaced: Some(rp) }).as_union_value())
        }
    };
    let args = fb::EntryContentArgs { seq, prev: Some(prev), height, time, body_type: bt, body: Some(bo), leader: Some(leader_v) };
    let c = fb::EntryContent::create(&mut b, &args);
    b.finish(c, None);
    b.finished_data().to_vec()
}

pub fn sign_entry(content: &[u8], kp: &Keypair) -> Vec<u8> {
    let sig = kp.sign(&crypto::sha256(content));
    let mut b = FlatBufferBuilder::with_capacity(content.len() + 128);
    let c = b.create_vector(content);
    let sg = b.create_vector(&sig);
    let e = fb::Entry::create(&mut b, &fb::EntryArgs { content: Some(c), sig: Some(sg) });
    b.finish(e, None);
    b.finished_data().to_vec()
}

/// Parse and verify an Entry buffer. The caller checks that `leader` is the
/// leader it expects; this only checks that the signature matches that key.
pub fn parse_entry(buf: &[u8]) -> Result<ParsedEntry> {
    let e = flatbuffers::root::<fb::Entry>(buf).map_err(|e| anyhow!("bad Entry: {e}"))?;
    let content = bytes(e.content()).to_vec();
    let sig = bytes(e.sig()).to_vec();
    let hash = crypto::sha256(&content);
    let c = flatbuffers::root::<fb::EntryContent>(&content).map_err(|e| anyhow!("bad EntryContent: {e}"))?;
    let leader = a33(bytes(c.leader()))?;
    if !crypto::verify(&leader, &hash, &sig) {
        bail!("bad entry signature");
    }
    let body = match c.body_type() {
        fb::EntryBody::SignedTx => {
            let st = c.body_as_signed_tx().ok_or_else(|| anyhow!("bad SignedTx"))?;
            Body::Tx(parse_tx_parts(bytes(st.tx()).to_vec(), bytes(st.sig()).to_vec())?)
        }
        fb::EntryBody::EpochStart => {
            let t = c.body_as_epoch_start().ok_or_else(|| anyhow!("bad EpochStart"))?;
            Body::EpochStart { height: t.height(), entry_txid: t.entry_txid().unwrap_or("").to_string() }
        }
        fb::EntryBody::ClaimExpired => {
            let t = c.body_as_claim_expired().ok_or_else(|| anyhow!("bad ClaimExpired"))?;
            Body::ClaimExpired { match_id: a32(bytes(t.match_id()))? }
        }
        fb::EntryBody::LegIncluded => {
            let t = c.body_as_leg_included().ok_or_else(|| anyhow!("bad LegIncluded"))?;
            Body::LegIncluded {
                match_id: a32(bytes(t.match_id()))?,
                leg: t.leg(),
                txid: t.txid().unwrap_or("").to_string(),
                block_hash: t.block_hash().unwrap_or("").to_string(),
                block_height: t.block_height(),
            }
        }
        fb::EntryBody::ClaimVoid => {
            let t = c.body_as_claim_void().ok_or_else(|| anyhow!("bad ClaimVoid"))?;
            Body::ClaimVoid { match_id: a32(bytes(t.match_id()))?, reason: t.reason() }
        }
        fb::EntryBody::LegMissed => {
            let t = c.body_as_leg_missed().ok_or_else(|| anyhow!("bad LegMissed"))?;
            Body::LegMissed { match_id: a32(bytes(t.match_id()))?, leg: t.leg() }
        }
        fb::EntryBody::MatchClosed => {
            let t = c.body_as_match_closed().ok_or_else(|| anyhow!("bad MatchClosed"))?;
            Body::MatchClosed { match_id: a32(bytes(t.match_id()))? }
        }
        fb::EntryBody::Handoff => {
            let t = c.body_as_handoff().ok_or_else(|| anyhow!("bad Handoff"))?;
            Body::Handoff { reason: t.reason().unwrap_or("").to_string(), replaced: bytes(t.replaced()).to_vec() }
        }
        _ => bail!("unknown entry body"),
    };
    let (seq, prev, height, time) = (c.seq(), a32(bytes(c.prev()))?, c.height(), c.time());
    Ok(ParsedEntry { bytes: buf.to_vec(), content, hash, seq, prev, height, time, leader, body })
}
