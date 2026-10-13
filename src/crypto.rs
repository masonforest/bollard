use anyhow::Result;
use bitcoin::hashes::{hash160, sha256, Hash};
use bitcoin::secp256k1::{ecdsa::Signature, All, Message, PublicKey, Secp256k1, SecretKey};
use std::sync::LazyLock;

pub static SECP: LazyLock<Secp256k1<All>> = LazyLock::new(Secp256k1::new);

pub fn sha256(b: &[u8]) -> [u8; 32] {
    sha256::Hash::hash(b).to_byte_array()
}

pub fn sha256d(b: &[u8]) -> [u8; 32] {
    sha256(&sha256(b))
}

pub fn hash160(b: &[u8]) -> [u8; 20] {
    hash160::Hash::hash(b).to_byte_array()
}

#[derive(Clone)]
pub struct Keypair {
    pub sk: SecretKey,
    pub pk: PublicKey,
}

impl Keypair {
    pub fn generate() -> Self {
        let (sk, pk) = SECP.generate_keypair(&mut rand::thread_rng());
        Self { sk, pk }
    }
    pub fn from_hex(h: &str) -> Result<Self> {
        let b = hex::decode(h.trim().trim_start_matches("0x"))?;
        let sk = SecretKey::from_slice(&b)?;
        let pk = PublicKey::from_secret_key(&SECP, &sk);
        Ok(Self { sk, pk })
    }
    pub fn secret_hex(&self) -> String {
        hex::encode(self.sk.secret_bytes())
    }
    pub fn pubkey(&self) -> [u8; 33] {
        self.pk.serialize()
    }
    pub fn key_hash(&self) -> [u8; 20] {
        hash160(&self.pubkey())
    }
    pub fn sign(&self, digest: &[u8; 32]) -> [u8; 64] {
        let m = Message::from_digest(*digest);
        SECP.sign_ecdsa(&m, &self.sk).serialize_compact()
    }
}

pub fn verify(pubkey: &[u8], digest: &[u8; 32], sig: &[u8]) -> bool {
    let (Ok(pk), Ok(sig)) = (PublicKey::from_slice(pubkey), Signature::from_compact(sig)) else {
        return false;
    };
    SECP.verify_ecdsa(&Message::from_digest(*digest), &sig, &pk).is_ok()
}

pub fn pubkey_from_hex(h: &str) -> Result<[u8; 33]> {
    let b = hex::decode(h.trim().trim_start_matches("0x"))?;
    Ok(PublicKey::from_slice(&b)?.serialize())
}
