//! Lottery entries on Bitcoin: a 75-byte OP_RETURN payload, weighted by fee,
//! ranked by a draw that every node computes the same way from the block.

use crate::crypto;
use serde_json::Value;
use std::net::{IpAddr, Ipv6Addr};

pub const MAGIC: &[u8; 4] = b"BLRD";
pub const VERSION: u8 = 1;
pub const PAYLOAD_LEN: usize = 75;

#[derive(Debug, Clone)]
pub struct Payload {
    pub key_hash: [u8; 20],
    pub ip: IpAddr,
    pub port: u16,
    pub tip: [u8; 32],
}

impl Payload {
    pub fn encode(&self) -> [u8; PAYLOAD_LEN] {
        let mut out = [0u8; PAYLOAD_LEN];
        out[0..4].copy_from_slice(MAGIC);
        out[4] = VERSION;
        out[5..25].copy_from_slice(&self.key_hash);
        let v6 = match self.ip {
            IpAddr::V4(a) => a.to_ipv6_mapped(),
            IpAddr::V6(a) => a,
        };
        out[25..41].copy_from_slice(&v6.octets());
        out[41..43].copy_from_slice(&self.port.to_be_bytes());
        out[43..75].copy_from_slice(&self.tip);
        out
    }

    pub fn decode(b: &[u8]) -> Option<Payload> {
        if b.len() != PAYLOAD_LEN || &b[0..4] != MAGIC || b[4] != VERSION {
            return None;
        }
        let v6 = Ipv6Addr::from(<[u8; 16]>::try_from(&b[25..41]).ok()?);
        let ip = v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6));
        Some(Payload {
            key_hash: b[5..25].try_into().ok()?,
            ip,
            port: u16::from_be_bytes([b[41], b[42]]),
            tip: b[43..75].try_into().ok()?,
        })
    }

    pub fn url(&self) -> String {
        match self.ip {
            IpAddr::V4(a) => format!("http://{a}:{}", self.port),
            IpAddr::V6(a) => format!("http://[{a}]:{}", self.port),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RawEntry {
    pub txid: String,
    pub payload: Payload,
    pub fee_sats: u64,
}

/// Entries in a block, from `getblock <hash> 2`: transactions with exactly one
/// OP_RETURN output whose data is a valid payload. Weight is the fee.
pub fn entries_in_block(block: &Value) -> Vec<RawEntry> {
    let mut out = Vec::new();
    let Some(txs) = block.get("tx").and_then(|t| t.as_array()) else { return out };
    for tx in txs {
        let Some(vouts) = tx.get("vout").and_then(|v| v.as_array()) else { continue };
        let mut nulldata = vouts.iter().filter(|o| o.pointer("/scriptPubKey/type").and_then(|t| t.as_str()) == Some("nulldata"));
        let (Some(o), None) = (nulldata.next(), nulldata.next()) else { continue };
        let Some(hex_script) = o.pointer("/scriptPubKey/hex").and_then(|h| h.as_str()) else { continue };
        let Ok(script) = hex::decode(hex_script) else { continue };
        // OP_RETURN (0x6a), direct push of 75 bytes (0x4b), payload.
        if script.len() != 2 + PAYLOAD_LEN || script[0] != 0x6a || script[1] != PAYLOAD_LEN as u8 {
            continue;
        }
        let Some(payload) = Payload::decode(&script[2..]) else { continue };
        let fee_btc = tx.get("fee").and_then(|f| f.as_f64()).unwrap_or(0.0);
        let fee_sats = (fee_btc * 1e8).round() as u64;
        let Some(txid) = tx.get("txid").and_then(|t| t.as_str()) else { continue };
        out.push(RawEntry { txid: txid.to_string(), payload, fee_sats });
    }
    out
}

/// Rank entries by drawing one at a time, weighted by fee, never twice.
/// Draw i: r = SHA256(seed, i) mod the undrawn total; walk the undrawn entries
/// in txid order adding fees; the first entry whose running total exceeds r
/// takes rank i. seed = SHA256d(block hash || "bollard/v1").
pub fn rank(block_hash: &str, entries: &[(String, u64)]) -> Vec<String> {
    let mut seed_in = hex::decode(block_hash).unwrap_or_default();
    seed_in.extend_from_slice(b"bollard/v1");
    let seed = crypto::sha256d(&seed_in);
    let mut pool: Vec<(String, u64)> = entries.iter().filter(|(_, w)| *w > 0).cloned().collect();
    pool.sort();
    let mut out = Vec::with_capacity(pool.len());
    let mut i: u64 = 0;
    while !pool.is_empty() {
        let total: u128 = pool.iter().map(|(_, w)| *w as u128).sum();
        let mut buf = seed.to_vec();
        buf.extend_from_slice(&i.to_le_bytes());
        let h = crypto::sha256(&buf);
        let r = u128::from_be_bytes(h[0..16].try_into().unwrap()) % total;
        let mut acc: u128 = 0;
        let mut pick = pool.len() - 1;
        for (idx, (_, w)) in pool.iter().enumerate() {
            acc += *w as u128;
            if acc > r {
                pick = idx;
                break;
            }
        }
        out.push(pool.remove(pick).0);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_roundtrip() {
        let p = Payload { key_hash: [7u8; 20], ip: "45.77.101.95".parse().unwrap(), port: 8080, tip: [9u8; 32] };
        let d = Payload::decode(&p.encode()).unwrap();
        assert_eq!(d.ip, p.ip);
        assert_eq!(d.port, 8080);
        assert_eq!(d.tip, p.tip);
        assert_eq!(d.key_hash, p.key_hash);
    }

    #[test]
    fn rank_is_a_permutation_and_deterministic() {
        let e = vec![("a".into(), 10u64), ("b".into(), 1), ("c".into(), 100)];
        let r1 = rank("00ab", &e);
        let r2 = rank("00ab", &e);
        assert_eq!(r1, r2);
        assert_eq!(r1.len(), 3);
        let mut s = r1.clone();
        s.sort();
        assert_eq!(s, vec!["a", "b", "c"]);
    }
}
