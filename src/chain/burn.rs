//! Keyless burn addresses, one per account per chain. Coins sent to them are
//! gone from their chain for good and show up on Bollard one for one. Nobody
//! holds a key: the address is a hash of the account's public key with a
//! vanity prefix, and the checksum is computed, not found.
//!
//! Bitcoin: a legacy P2PKH address starting with `1BoLLARD`. Legacy costs the
//! sender three vbytes more than bech32 and nobody ever spends from it, so the
//! readable prefix is nearly free. Ethereum: an address starting with
//! `b0110a4d`, which is "bollard" in hex.

use crate::crypto::{sha256, sha256d};

const B58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
pub const BTC_PREFIX: &str = "1BoLLARD";
pub const ETH_PREFIX: &str = "b0110a4d";

fn b58_encode(bytes: &[u8]) -> String {
    let zeros = bytes.iter().take_while(|b| **b == 0).count();
    let mut digits: Vec<u8> = Vec::new(); // little-endian base58 digits
    for &byte in bytes {
        let mut carry = byte as u32;
        for d in digits.iter_mut() {
            carry += (*d as u32) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let mut s: Vec<u8> = vec![b'1'; zeros];
    s.extend(digits.iter().rev().map(|d| B58[*d as usize]));
    String::from_utf8(s).expect("ascii")
}

/// Decode to exactly `len` bytes, or None if the value does not fit.
fn b58_decode(s: &str, len: usize) -> Option<Vec<u8>> {
    let mut bytes: Vec<u8> = Vec::new(); // little-endian
    for c in s.bytes() {
        let mut carry = B58.iter().position(|b| *b == c)? as u32;
        for b in bytes.iter_mut() {
            carry += (*b as u32) * 58;
            *b = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            bytes.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    let zeros = s.bytes().take_while(|b| *b == b'1').count();
    bytes.extend(std::iter::repeat(0).take(zeros));
    if bytes.len() > len {
        return None;
    }
    bytes.resize(len, 0);
    bytes.reverse();
    Some(bytes)
}

/// The account's Bitcoin burn address: `1BoLLARD` then characters drawn from a
/// hash of the key, with a valid checksum, decoding to version 0 and 20 bytes.
pub fn btc(pubkey: &[u8]) -> String {
    let mut seed = b"bollard/burn/btc".to_vec();
    seed.extend_from_slice(pubkey);
    let mut filler = b58_encode(&sha256(&seed));
    while filler.len() < 40 {
        filler.push('x');
    }
    for len in [34usize, 33] {
        let s: String = format!("{BTC_PREFIX}{filler}").chars().take(len).collect();
        let Some(raw) = b58_decode(&s, 25) else { continue };
        if raw[0] != 0 {
            continue;
        }
        let payload = &raw[..21];
        let chk = &sha256d(payload)[..4];
        let mut full = payload.to_vec();
        full.extend_from_slice(chk);
        let addr = b58_encode(&full);
        if addr.starts_with(BTC_PREFIX) {
            return addr;
        }
    }
    unreachable!("a 33-character body always fits")
}

/// The account's Ethereum burn address: `0xb0110a4d` then 32 hex characters from a keccak of the key.
pub fn eth(pubkey: &[u8]) -> String {
    use sha3::{Digest, Keccak256};
    let mut seed = b"bollard/burn/eth".to_vec();
    seed.extend_from_slice(pubkey);
    let h = Keccak256::digest(&seed);
    format!("0x{ETH_PREFIX}{}", &hex::encode(h)[..32])
}

pub fn for_chain(chain: &str, pubkey: &[u8]) -> Option<String> {
    match chain {
        "btc" => Some(btc(pubkey)),
        "eth" => Some(eth(pubkey)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn bitcoin_burn_addresses_parse_and_carry_the_prefix() {
        for i in 0u8..40 {
            let pk = [i; 33];
            let a = btc(&pk);
            assert!(a.starts_with(BTC_PREFIX), "{a}");
            let parsed = bitcoin::Address::from_str(&a).expect("checksum").require_network(bitcoin::Network::Bitcoin).expect("mainnet");
            assert!(matches!(parsed.address_type(), Some(bitcoin::AddressType::P2pkh)));
            assert_eq!(a, btc(&pk), "deterministic");
        }
        assert_ne!(btc(&[1u8; 33]), btc(&[2u8; 33]));
    }

    #[test]
    fn ethereum_burn_addresses_are_hex_with_the_prefix() {
        let a = eth(&[7u8; 65]);
        assert_eq!(a.len(), 42);
        assert!(a.starts_with("0xb0110a4d"));
        assert!(a[2..].bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, eth(&[8u8; 65]));
    }

    #[test]
    fn base58_round_trips() {
        let v = vec![0, 0, 1, 2, 3, 250, 255];
        assert_eq!(b58_decode(&b58_encode(&v), 7).unwrap(), v);
    }
}
