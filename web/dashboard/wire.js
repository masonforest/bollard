// Bollard wire format in the browser: keys, FlatBuffers transactions, signing.
// Mirrors schema/bollard.fbs field by field. Works in browsers and Node.
import * as flatbuffers from 'flatbuffers';
import * as secp from '@noble/secp256k1';

export const TxType = { Transfer: 1, PlaceOrder: 2, CancelOrder: 3, Claim: 4, ReportPayment: 5, Mint: 6, Burn: 7, Send: 8, Exercise: 9, SetCurrency: 10, Stake: 11, Unstake: 12, Liquidate: 13, SetReceiveAddress: 14 };

export const hex = {
  enc: (u8) => Array.from(u8, (b) => b.toString(16).padStart(2, '0')).join(''),
  dec: (s) => Uint8Array.from((s.trim().replace(/^0x/, '').match(/../g) ?? []), (h) => parseInt(h, 16)),
};

export async function sha256(u8) {
  return new Uint8Array(await crypto.subtle.digest('SHA-256', u8));
}

export function randomSecret() { return hex.enc(secp.utils.randomPrivateKey()); }
export function pubkeyOf(secretHex) { return secp.getPublicKey(hex.dec(secretHex), true); }
export function isSecret(s) { try { return secp.utils.isValidPrivateKey(hex.dec(s)); } catch { return false; } }

const big = (v) => BigInt(v);

/** Build the Tx table bytes (the bytes that get signed). */
export function buildTx(account, nonce, type, body) {
  const b = new flatbuffers.Builder(512);
  const acct = b.createByteVector(account);
  let off;
  switch (type) {
    case TxType.Transfer: {
      const to = b.createByteVector(body.to);
      b.startObject(2); b.addFieldOffset(0, to, 0); b.addFieldInt64(1, big(body.amount), 0n); off = b.endObject(); break;
    }
    case TxType.PlaceOrder: {
      const market = b.createString(body.market);
      const ra = b.createString(body.receive_address ?? '');
      b.startObject(11);
      b.addFieldOffset(0, market, 0);
      b.addFieldInt8(1, body.side, 0);
      b.addFieldInt64(2, big(body.quantity), 0n);
      b.addFieldInt64(3, big(body.price), 0n);
      b.addFieldInt64(4, big(body.bond ?? 0), 0n);
      b.addFieldInt64(5, big(body.claim_bond ?? 0), 0n);
      b.addFieldInt8(6, body.fill_policy ?? 1, 0);
      b.addFieldOffset(7, ra, 0);
      b.addFieldInt32(8, body.expiry_height, 0);
      b.addFieldInt8(9, body.settle ?? 0, 0);
      b.addFieldInt8(10, body.mint ? 1 : 0, 0);
      off = b.endObject(); break;
    }
    case TxType.Mint: {
      const a = b.createString(body.asset); const src = b.createString(body.source ?? '');
      b.startObject(3); b.addFieldOffset(0, a, 0); b.addFieldInt64(1, big(body.quantity), 0n); b.addFieldOffset(2, src, 0); off = b.endObject(); break;
    }
    case TxType.SetReceiveAddress: {
      const c = b.createString(body.chain); const a = b.createString(body.address ?? '');
      b.startObject(2); b.addFieldOffset(0, c, 0); b.addFieldOffset(1, a, 0); off = b.endObject(); break;
    }
    case TxType.Liquidate: {
      const i = b.createByteVector(body.issuer); const a = b.createString(body.asset);
      b.startObject(3); b.addFieldOffset(0, i, 0); b.addFieldOffset(1, a, 0); b.addFieldInt64(2, big(body.quantity), 0n); off = b.endObject(); break;
    }
    case TxType.Stake: case TxType.Unstake: {
      const a = b.createString(body.asset ?? 'BLRD');
      b.startObject(2); b.addFieldInt64(0, big(body.amount), 0n); b.addFieldOffset(1, a, 0); off = b.endObject(); break;
    }
    case TxType.Burn: {
      const a = b.createString(body.asset);
      b.startObject(2); b.addFieldOffset(0, a, 0); b.addFieldInt64(1, big(body.quantity), 0n); off = b.endObject(); break;
    }
    case TxType.Send: {
      const to = b.createByteVector(body.to); const a = b.createString(body.asset);
      b.startObject(3); b.addFieldOffset(0, to, 0); b.addFieldOffset(1, a, 0); b.addFieldInt64(2, big(body.amount), 0n); off = b.endObject(); break;
    }
    case TxType.Exercise: {
      const a = b.createString(body.asset); const ra = b.createString(body.receive_address ?? ''); const iss = b.createByteVector(body.issuer ?? new Uint8Array(0));
      b.startObject(4); b.addFieldOffset(0, a, 0); b.addFieldInt64(1, big(body.quantity), 0n); b.addFieldOffset(2, ra, 0); b.addFieldOffset(3, iss, 0); off = b.endObject(); break;
    }
    case TxType.SetCurrency: {
      const a = b.createString(body.asset);
      b.startObject(1); b.addFieldOffset(0, a, 0); off = b.endObject(); break;
    }
    case TxType.CancelOrder: {
      const o = b.createByteVector(body.order_id);
      b.startObject(1); b.addFieldOffset(0, o, 0); off = b.endObject(); break;
    }
    case TxType.Claim: {
      const o = b.createByteVector(body.order_id);
      const ra = b.createString(body.receive_address ?? '');
      b.startObject(3); b.addFieldOffset(0, o, 0); b.addFieldInt64(1, big(body.quantity), 0n); b.addFieldOffset(2, ra, 0); off = b.endObject(); break;
    }
    case TxType.ReportPayment: {
      const m = b.createByteVector(body.match_id);
      const t = b.createString(body.txid);
      b.startObject(3); b.addFieldOffset(0, m, 0); b.addFieldInt8(1, body.leg, 0); b.addFieldOffset(2, t, 0); off = b.endObject(); break;
    }
    default: throw new Error('unknown tx type');
  }
  b.startObject(4);
  b.addFieldOffset(0, acct, 0);
  b.addFieldInt64(1, big(nonce), 0n);
  b.addFieldInt8(2, type, 0);
  b.addFieldOffset(3, off, 0);
  const tx = b.endObject();
  b.finish(tx);
  return b.asUint8Array();
}

function wrap(txBytes, sigBytes, authData, clientData) {
  const b = new flatbuffers.Builder(txBytes.length + 512);
  const t = b.createByteVector(txBytes);
  const s = b.createByteVector(sigBytes);
  const ad = authData ? b.createByteVector(authData) : 0;
  const cd = clientData ? b.createByteVector(clientData) : 0;
  b.startObject(4); b.addFieldOffset(0, t, 0); b.addFieldOffset(1, s, 0);
  if (ad) b.addFieldOffset(2, ad, 0);
  if (cd) b.addFieldOffset(3, cd, 0);
  const st = b.endObject();
  b.finish(st);
  return b.asUint8Array();
}

/** Sign Tx bytes with a secp256k1 secret: SHA256(tx), compact low-S ECDSA. */
export async function signTx(txBytes, secretHex) {
  const digest = await sha256(txBytes);
  const sig = await secp.signAsync(digest, hex.dec(secretHex), { lowS: true });
  return wrap(txBytes, sig.toCompactRawBytes());
}

// ---- passkeys (WebAuthn, P-256). The account is the 65-byte uncompressed key.
export const b64url = {
  enc: (u8) => btoa(String.fromCharCode(...u8)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, ''),
  dec: (s) => Uint8Array.from(atob(s.replace(/-/g, '+').replace(/_/g, '/')), (c) => c.charCodeAt(0)),
};
const P256_N = 0xffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551n;
// Passkeys return DER; the node wants r||s with a low s.
function derToRaw(der) {
  let i = 2;
  const int = () => { i++; const len = der[i++]; let v = der.slice(i, i + len); i += len; while (v.length > 32 && v[0] === 0) v = v.slice(1); const out = new Uint8Array(32); out.set(v, 32 - v.length); return out; };
  const r = int(); const s = int();
  let S = BigInt('0x' + hex.enc(s)); if (S > P256_N / 2n) S = P256_N - S;
  const out = new Uint8Array(64); out.set(r, 0); out.set(hex.dec(S.toString(16).padStart(64, '0')), 32);
  return out;
}
export async function createPasskey(name = 'Bollard') {
  const cred = await navigator.credentials.create({ publicKey: {
    challenge: crypto.getRandomValues(new Uint8Array(32)),
    rp: { name },
    user: { id: crypto.getRandomValues(new Uint8Array(16)), name, displayName: name },
    pubKeyCredParams: [{ type: 'public-key', alg: -7 }],
    authenticatorSelection: { residentKey: 'preferred', userVerification: 'required' },
  } });
  if (cred.response.getPublicKeyAlgorithm() !== -7) throw new Error('This passkey is not P-256.');
  const spki = new Uint8Array(cred.response.getPublicKey());
  return { id: b64url.enc(new Uint8Array(cred.rawId)), pubkey: hex.enc(spki.slice(-65)) };
}
export async function signInPasskey(knownId) {
  const cred = await navigator.credentials.get({ publicKey: {
    challenge: crypto.getRandomValues(new Uint8Array(32)), userVerification: 'required',
    allowCredentials: knownId ? [{ type: 'public-key', id: b64url.dec(knownId) }] : [],
  } });
  return b64url.enc(new Uint8Array(cred.rawId));
}
/** Sign Tx bytes with a passkey: the challenge is the base64url of SHA256(tx). */
export async function signTxPasskey(txBytes, credId) {
  const digest = await sha256(txBytes);
  const cred = await navigator.credentials.get({ publicKey: { challenge: digest, allowCredentials: [{ type: 'public-key', id: b64url.dec(credId) }], userVerification: 'required' } });
  const r = cred.response;
  return wrap(txBytes, derToRaw(new Uint8Array(r.signature)), new Uint8Array(r.authenticatorData), new Uint8Array(r.clientDataJSON));
}

/** A signer: { account: Uint8Array, sign(txBytes) -> SignedTx bytes }. */
export const signers = {
  secret: (secretHex) => ({ account: pubkeyOf(secretHex), sign: (tx) => signTx(tx, secretHex) }),
  passkey: ({ id, pubkey }) => ({ account: hex.dec(pubkey), sign: (tx) => signTxPasskey(tx, id) }),
};

export async function submit(api, signer, type, body) {
  const acct = await (await fetch(`${api}/account/${hex.enc(signer.account)}`)).json();
  const tx = buildTx(signer.account, acct.nonce ?? 0, type, body);
  const signed = await signer.sign(tx);
  const res = await fetch(`${api}/tx`, { method: 'POST', headers: { 'content-type': 'application/octet-stream' }, body: signed });
  const out = await res.json().catch(() => ({ error: 'no JSON from node' }));
  if (!res.ok) throw new Error(out.error || 'rejected');
  return out;
}

/** Build, sign with a secret and submit one transaction. */
export const send = (api, secretHex, type, body) => submit(api, signers.secret(secretHex), type, body);
