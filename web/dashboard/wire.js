// Bollard wire format in the browser: keys, FlatBuffers transactions, signing.
// Mirrors schema/bollard.fbs field by field. Works in browsers and Node.
import * as flatbuffers from 'flatbuffers';
import * as secp from '@noble/secp256k1';

export const TxType = { Transfer: 1, PlaceOrder: 2, CancelOrder: 3, Claim: 4, ReportPayment: 5 };

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
      b.startObject(9);
      b.addFieldOffset(0, market, 0);
      b.addFieldInt8(1, body.side, 0);
      b.addFieldInt64(2, big(body.quantity), 0n);
      b.addFieldInt64(3, big(body.price), 0n);
      b.addFieldInt64(4, big(body.bond ?? 0), 0n);
      b.addFieldInt64(5, big(body.claim_bond ?? 0), 0n);
      b.addFieldInt8(6, body.fill_policy ?? 1, 0);
      b.addFieldOffset(7, ra, 0);
      b.addFieldInt32(8, body.expiry_height, 0);
      off = b.endObject(); break;
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

/** Sign Tx bytes and wrap them in a SignedTx: SHA256(tx), compact low-S ECDSA. */
export async function signTx(txBytes, secretHex) {
  const digest = await sha256(txBytes);
  const sig = await secp.signAsync(digest, hex.dec(secretHex), { lowS: true });
  const sigBytes = sig.toCompactRawBytes();
  const b = new flatbuffers.Builder(txBytes.length + 128);
  const t = b.createByteVector(txBytes);
  const s = b.createByteVector(sigBytes);
  b.startObject(2); b.addFieldOffset(0, t, 0); b.addFieldOffset(1, s, 0);
  const st = b.endObject();
  b.finish(st);
  return b.asUint8Array();
}

/** Build, sign and submit one transaction. Fetches the nonce from the node. */
export async function send(api, secretHex, type, body) {
  const pk = pubkeyOf(secretHex);
  const acct = await (await fetch(`${api}/account/${hex.enc(pk)}`)).json();
  const tx = buildTx(pk, acct.nonce ?? 0, type, body);
  const signed = await signTx(tx, secretHex);
  const res = await fetch(`${api}/tx`, { method: 'POST', headers: { 'content-type': 'application/octet-stream' }, body: signed });
  const out = await res.json().catch(() => ({ error: 'no JSON from node' }));
  if (!res.ok) throw new Error(out.error || 'rejected');
  return out;
}
