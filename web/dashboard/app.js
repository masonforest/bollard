import { TxType, hex, signers, submit, createPasskey, signInPasskey } from './wire.js';

// ------------------------------------------------------------------ setup
const BASE_DOMAIN = location.hostname.replace(/^(app|www)\./, '');
const API = (location.port || location.hostname.startsWith('mainnet.') || location.hostname === 'localhost') ? '' : 'https://mainnet.' + BASE_DOMAIN;
let api = API;
const $ = (id) => document.getElementById(id);
const short = (h, n = 10) => (h ? h.slice(0, n) + '…' : '');
const j = async (path) => { const r = await fetch(api + path); return r.json(); };
const policy = ['seen', 'included', 'final'];
const assets = {};
let marketsInfo = null;
let status = null;

// ------------------------------------------------------------------ units
function toAtoms(human, assetId) {
  const a = assets[assetId]; if (!a) throw new Error('unknown asset');
  const s = String(human).trim(); if (!/^\d*\.?\d*$/.test(s) || s === '' || s === '.') throw new Error('bad amount');
  const [i, f = ''] = s.split('.');
  if (f.length > a.decimals) throw new Error(`at most ${a.decimals} decimals for ${a.symbol}`);
  return BigInt(i || '0') * 10n ** BigInt(a.decimals) + BigInt((f + '0'.repeat(a.decimals)).slice(0, a.decimals) || '0');
}
function fromAtoms(atoms, assetId, withSymbol = true) {
  const a = assets[assetId]; if (!a) return String(atoms);
  const s = BigInt(atoms).toString().padStart(a.decimals + 1, '0');
  const t = (s.slice(0, -a.decimals) + '.' + s.slice(-a.decimals)).replace(/\.?0+$/, '');
  return withSymbol ? `${t} ${a.symbol}` : t;
}
// price: quote atoms per 1e8 base atoms  <->  quote units per base unit
function priceToHuman(price, base, quote) {
  return Number(price) / 10 ** assets[quote].decimals * 10 ** assets[base].decimals / 1e8;
}
function priceFromHuman(human, base, quote) {
  const v = Number(human); if (!(v > 0)) throw new Error('bad price');
  return BigInt(Math.round(v * 10 ** assets[quote].decimals / 10 ** assets[base].decimals * 1e8));
}
const fmt = (n, d = 8) => Number(n).toLocaleString(undefined, { maximumSignificantDigits: d });
const quoteAmount = (qty, price) => (BigInt(qty) * BigInt(price)) / 100000000n;

// ------------------------------------------------------------------ account (passkey, shared with the wallet)
const KEY = 'bollard.passkey';
let key = null;
try { key = JSON.parse(localStorage.getItem(KEY) || 'null'); } catch {}
const me = () => key?.pubkey || null;
function setKey(k) { key = k; try { if (k) localStorage.setItem(KEY, JSON.stringify(k)); else localStorage.removeItem(KEY); } catch {} renderAccount(); }
const signer = () => { if (!key) throw new Error('Sign in first (top right).'); return signers.passkey(key); };
const send = (api, _unused, type, body) => submit(api, signer(), type, body);
async function renderAccount() {
  const pk = me();
  $('account-btn').textContent = pk ? short(pk, 12) : '';
  $('gate').classList.toggle('hidden', !!pk);
  $('app').classList.toggle('hidden', !pk);
  $('a-pub').textContent = pk || '(not signed in)';
  if (pk) {
    const a = await j(`/account/${pk}`).catch(() => null);
    const bal = a ? fromAtoms(a.balance, 'BLRD') : '?';
    $('a-balance').textContent = a ? `${bal} (nonce ${a.nonce})` : '?';
    const b = await j(`/balances/${pk}`).catch(() => null);
    $('a-assets').innerHTML = b ? [...(b.assets || []).filter((x) => x.amount > 0).map((x) => `<div class="kv"><span>${assets[x.asset]?.symbol || x.asset}</span><span>${fromAtoms(x.amount, x.asset)}</span></div>`)].join('') : '';
  } else { $('a-balance').textContent = '–'; $('a-assets').innerHTML = ''; }
}
// Signed out: the page is just two buttons. Create makes and registers a passkey; Sign in asks for an existing one.
function authError(e) { if (e?.name === 'NotAllowedError') return; $('g-error').textContent = e.message || String(e); }
$('g-create').onclick = async () => {
  $('g-error').textContent = '';
  try {
    const k = await createPasskey('Bollard');
    const r = await fetch(api + '/keys', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ cred_id: k.id, pubkey: k.pubkey }) });
    if (!r.ok) throw new Error((await r.json().catch(() => ({}))).error || 'could not register the passkey');
    setKey(k);
  } catch (e) { authError(e); }
};
$('g-signin').onclick = async () => {
  $('g-error').textContent = '';
  try {
    const id = await signInPasskey();
    const r = await fetch(`${api}/keys/${encodeURIComponent(id)}`);
    if (!r.ok) throw new Error('That passkey is not registered here. Create an account instead.');
    setKey({ id, pubkey: (await r.json()).pubkey });
  } catch (e) { authError(e); }
};
function ensureKey() { if (!key) throw new Error('Sign in first.'); return key; }
$('account-btn').onclick = () => { renderAccount(); $('account').showModal(); };
$('a-close').onclick = () => $('account').close();
$('a-copy').onclick = () => navigator.clipboard.writeText(me() || '');
$('a-signout').onclick = () => { setKey(null); $('account').close(); };

// ------------------------------------------------------------------ status
async function loadStatus() {
  status = await j('/status');
  const r = status.role; const el = $('role');
  el.textContent = r.role === 'Leader' ? 'node is the leader' : r.role === 'Follower' ? 'following ' + r.url : r.role.toLowerCase() + (r.reason ? ': ' + r.reason : '');
  el.className = 'pill' + (r.role === 'Leader' ? ' leader' : '');
}

async function loadMarkets() {
  marketsInfo = await j('/markets');
  for (const a of marketsInfo.assets) assets[a.id] = a;
  buildPickers();
}

// ------------------------------------------------------------------ coins and the token picker
const CHAIN_NAMES = { btc: 'Bitcoin', eth: 'Ethereum', base: 'Base', blrd: 'Bollard' };
const ASSET_NAMES = { 'btc:BTC': 'Bitcoin', 'eth:ETH': 'Ether', 'eth:USDC': 'USD Coin', 'eth:USDT': 'Tether', 'base:USDC': 'USD Coin', 'base:gtUSDCp': 'USDC Prime (Morpho vault)', 'BLRD': 'Bollard' };
// The Morpho vault behind USDC Prime, for the rate and the share price the wallet shows.
const VAULT = { asset: 'base:gtUSDCp', address: '0x050cE30b927Da55177A4914EC73480238BAD56f0', chainId: 8453, url: 'https://app.morpho.org/base/vault/0x050cE30b927Da55177A4914EC73480238BAD56f0/gauntlet-usdc-prime' };
let vaultInfo = null, vaultFetched = 0;
async function loadVault() {
  if (Date.now() - vaultFetched < 60000) return vaultInfo;
  vaultFetched = Date.now();
  try {
    const r = await fetch('https://blue-api.morpho.org/graphql', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ query: `{ vaultV2ByAddress(address: "${VAULT.address}", chainId: ${VAULT.chainId}) { netApy apy sharePrice } }` }) });
    vaultInfo = (await r.json()).data?.vaultV2ByAddress || null;
  } catch { vaultInfo = null; }
  return vaultInfo;
}
const coinClass = (a) => 'c-' + (a.symbol.toLowerCase() === 'usdc' || a.symbol.toLowerCase() === 'usdt' ? a.symbol.toLowerCase() : a.chain);
const LOGOS = {
  'eth:USDC': `<svg viewBox="0 0 32 32"><circle cx="16" cy="16" r="16" fill="#2775CA"/><path fill="#fff" d="M20.5 18.6c0-2.3-1.4-3.1-4.2-3.5-2-.3-2.4-.8-2.4-1.7s.7-1.5 2-1.5c1.2 0 1.9.4 2.2 1.4.1.2.3.3.5.3h1.1c.3 0 .5-.2.4-.5-.3-1.5-1.4-2.6-3-2.9V8.8c0-.3-.2-.5-.5-.5h-1c-.3 0-.5.2-.5.5v1.4c-2 .3-3.3 1.6-3.3 3.3 0 2.2 1.3 3 4.1 3.4 1.9.3 2.5.7 2.5 1.8s-.9 1.7-2.2 1.7c-1.6 0-2.2-.7-2.4-1.6-.1-.2-.3-.4-.5-.4h-1.2c-.3 0-.5.2-.4.5.3 1.7 1.4 2.9 3.4 3.2v1.4c0 .3.2.5.5.5h1c.3 0 .5-.2.5-.5v-1.4c2-.3 3.4-1.7 3.4-3.5z"/><path fill="#fff" d="M12.6 25.6c-5.2-1.9-7.9-7.7-6-12.9 1-2.8 3.2-4.9 6-5.9.3-.1.4-.3.4-.6v-.9c0-.3-.1-.5-.4-.5h-.2C6 6.8 2.5 13.6 4.5 19.9c1.2 3.7 4.1 6.6 7.8 7.8.3.1.5 0 .6-.3v-1.2c0-.2-.1-.4-.3-.6zm7-19.9c-.3-.1-.5 0-.6.3v1.1c0 .2.1.4.3.6 5.2 1.9 7.9 7.7 6 12.9-1 2.8-3.2 4.9-6 5.9-.3.1-.4.3-.4.6v.9c0 .3.1.5.4.5h.2c6.4-2 9.9-8.8 7.9-15.1-1.2-3.8-4.1-6.7-7.8-7.7z"/></svg>`,
  'btc:BTC': `<svg viewBox="0 0 32 32"><circle cx="16" cy="16" r="16" fill="#F7931A"/><text x="16" y="22" text-anchor="middle" font-size="17" font-weight="700" fill="#fff" font-family="Arial, Helvetica, sans-serif" transform="rotate(-12 16 16)">₿</text></svg>`,
  'eth:ETH': `<svg viewBox="0 0 32 32"><circle cx="16" cy="16" r="16" fill="#627EEA"/><path fill="#fff" fill-opacity=".6" d="M16 5v8.1l6.9 3.1z"/><path fill="#fff" d="M16 5l-6.9 11.2 6.9-3.1z"/><path fill="#fff" fill-opacity=".6" d="M16 21.7V27l6.9-9.6z"/><path fill="#fff" d="M16 27v-5.3l-6.9-4.3z"/><path fill="#fff" fill-opacity=".2" d="M16 20.4l6.9-4.2-6.9-3.1z"/><path fill="#fff" fill-opacity=".6" d="M9.1 16.2l6.9 4.2v-7.3z"/></svg>`,
  'eth:USDT': `<svg viewBox="0 0 32 32"><circle cx="16" cy="16" r="16" fill="#26A17B"/><path fill="#fff" d="M17.9 17.9v-.1c-.1 0-.7.1-1.9.1-1 0-1.7 0-1.9-.1v.1c-3.7-.2-6.5-.8-6.5-1.6s2.8-1.4 6.5-1.6v2.6c.3 0 1 .1 1.9.1 1.1 0 1.7-.1 1.9-.1v-2.6c3.7.2 6.5.8 6.5 1.6s-2.8 1.4-6.5 1.6zm0-3.5v-2.3h5.2V8.6H8.9v3.5h5.2v2.3c-4.2.2-7.4 1-7.4 2s3.2 1.8 7.4 2v7.5h3.8v-7.5c4.2-.2 7.4-1 7.4-2s-3.2-1.8-7.4-2z"/></svg>`,
  'BLRD': `<svg viewBox="0 0 32 32"><circle cx="16" cy="16" r="16" fill="#1F5FBF"/><text x="16" y="22" text-anchor="middle" font-size="17" font-weight="700" fill="#fff" font-family="Arial, Helvetica, sans-serif">B</text></svg>`,
  'base:gtUSDCp': `<svg viewBox="0 0 32 32"><circle cx="16" cy="16" r="16" fill="#2775CA"/><circle cx="16" cy="16" r="10" fill="none" stroke="#fff" stroke-width="2.2" stroke-dasharray="44 20" transform="rotate(-60 16 16)"/><text x="16" y="20.5" text-anchor="middle" font-size="12" font-weight="700" fill="#fff" font-family="Arial, Helvetica, sans-serif">%</text></svg>`,
  base: `<svg viewBox="0 0 32 32"><circle cx="16" cy="16" r="16" fill="#0052FF"/><circle cx="16" cy="16" r="9" fill="none" stroke="#fff" stroke-width="4" stroke-dasharray="44 13" transform="rotate(180 16 16)"/></svg>`,
};
LOGOS['base:USDC'] = LOGOS['eth:USDC'];
function coinIcon(a, large = false) {
  return `<span class="coin ${large ? 'lg' : ''}">${LOGOS[a.id] || ''}</span>`;
}
// Two autocompletes with logos: the network first, then the token on it. Writes the
// chosen asset id into the hidden input named by data-pick and fires its input event.
const chainIcon = (c) => `<span class="coin">${LOGOS[c === 'btc' ? 'btc:BTC' : c === 'eth' ? 'eth:ETH' : c === 'base' ? 'base' : 'BLRD'] || ''}</span>`;
function buildPickers() {
  for (const root of document.querySelectorAll('.pickers')) {
    const target = $(root.dataset.pick);
    // Dollars are the quote everywhere except in the simple card, where you can buy them by burning USDC on Ethereum.
    const all = () => Object.values(assets).filter((a) => a.id !== 'eth:USDC');
    const chains = () => [...new Set(all().map((a) => a.chain))];
    root.innerHTML = `
      <div class="ac" data-ac="chain"><span class="lbl">Network</span><div class="ac-field"><span class="ic"></span><input autocomplete="off" spellcheck="false" placeholder="Network"></div><ul class="ac-list hidden"></ul></div>
      <div class="ac" data-ac="token"><span class="lbl">Token</span><div class="ac-field"><span class="ic"></span><input autocomplete="off" spellcheck="false" placeholder="Token"></div><ul class="ac-list hidden"></ul></div>`;
    const [cAc, tAc] = root.querySelectorAll('.ac');
    const chainOf = () => assets[target.value]?.chain || chains()[0];
    const show = () => {
      const c = chainOf(); const a = assets[target.value];
      const single = all().filter((x) => x.chain === c).length < 2;
      tAc.classList.toggle('hidden', single); root.classList.toggle('one', single);
      cAc.querySelector('.ic').innerHTML = chainIcon(c); cAc.querySelector('input').value = CHAIN_NAMES[c] || c;
      tAc.querySelector('.ic').innerHTML = a ? coinIcon(a) : ''; tAc.querySelector('input').value = a ? a.symbol : '';
    };
    const pick = (id) => { target.value = id; show(); target.dispatchEvent(new Event('input', { bubbles: true })); };
    const wire = (ac, items, label, icon, onPick) => {
      const inp = ac.querySelector('input'); const list = ac.querySelector('.ac-list'); let hl = 0;
      const open = (filter) => {
        const q = (filter ?? '').trim().toLowerCase();
        const rows = items().filter((it) => !q || label(it).toLowerCase().includes(q) || (it.symbol || '').toLowerCase().includes(q) || (it.id || '').toLowerCase().includes(q));
        hl = 0;
        list.innerHTML = rows.length ? rows.map((it, i) => `<li data-i="${i}" class="${i === hl ? 'hl' : ''}">${icon(it)}<span>${label(it)}</span>${it.symbol ? `<span class="sym">${it.symbol}</span>` : ''}</li>`).join('') : '<li class="none">Nothing matches.</li>';
        list.classList.remove('hidden');
        list.onmousedown = (e) => { const li = e.target.closest('li[data-i]'); if (!li) return; e.preventDefault(); onPick(rows[Number(li.dataset.i)]); close(); };
        list.rows = rows;
      };
      const close = () => { list.classList.add('hidden'); show(); };
      inp.onfocus = () => { inp.select(); open(''); };
      inp.oninput = () => open(inp.value);
      inp.onblur = () => setTimeout(close, 120);
      inp.onkeydown = (e) => {
        const rows = list.rows || [];
        if (e.key === 'ArrowDown' || e.key === 'ArrowUp') { e.preventDefault(); hl = (hl + (e.key === 'ArrowDown' ? 1 : rows.length - 1)) % Math.max(rows.length, 1); [...list.children].forEach((li, i) => li.classList.toggle('hl', i === hl)); }
        else if (e.key === 'Enter') { e.preventDefault(); if (rows[hl]) { onPick(rows[hl]); inp.blur(); } }
        else if (e.key === 'Escape') inp.blur();
      };
    };
    wire(cAc, () => chains().map((c) => ({ id: c, name: CHAIN_NAMES[c] || c })), (c) => c.name, (c) => chainIcon(c.id), (c) => {
      if (assets[target.value]?.chain !== c.id) { const first = all().find((a) => a.chain === c.id); if (first) pick(first.id); }
    });
    wire(tAc, () => all().filter((a) => a.chain === chainOf()), (a) => ASSET_NAMES[a.id] || a.symbol, (a) => coinIcon(a), (a) => pick(a.id));
    root.show = show; show();
  }
}
function renderTokenButtons() { for (const root of document.querySelectorAll('.pickers')) root.show?.(); }

function currentMarket() { return `${$('m-base-asset').value}/eth:USDC`; }
// ------------------------------------------------------------------ simple: buy or sell for dollars
let side = 'buy'; let simpleTimer = null; let plan = null; // plan: { fills: [{order, qty}], quote, short } or { deposit: order, qty }
const USDC = 'eth:USDC';
function setSide(v) { side = v; $('s-buy').classList.toggle('on', v === 'buy'); $('s-sell').classList.toggle('on', v === 'sell'); scheduleSimple(); }
$('s-buy').onclick = () => setSide('buy'); $('s-sell').onclick = () => setSide('sell');
$('s-amount').addEventListener('input', scheduleSimple);
// Type the amount in dollars or in the coin; the book converts either way.
let unitUsd = true;
$('s-swap').onclick = () => { unitUsd = !unitUsd; computeSimple(); };
$('s-asset').addEventListener('input', scheduleSimple);
function scheduleSimple() { clearTimeout(simpleTimer); simpleTimer = setTimeout(computeSimple, 200); }
let computeSeq = 0;
async function computeSimple() {
  const run = ++computeSeq; // a slower, older run must not overwrite a newer one
  plan = null; $('s-error').textContent = '';
  const asset = $('s-asset').value; const a = assets[asset]; if (!a) return;
  $('s-go').textContent = `${side === 'buy' ? 'Buy' : 'Sell'} ${a.symbol}`;
  $('s-prefix').classList.toggle('hidden', !unitUsd); $('s-unit').textContent = unitUsd ? '' : a.symbol; $('s-amount').placeholder = unitUsd ? '0.00' : '0.0';
  // The switcher says what it will switch to.
  $('s-swap').innerHTML = unitUsd ? `<span class="sw-ic">⇅</span>${a.symbol}` : `<span class="sw-ic">⇅</span>USD`;
  $('s-swap').title = unitUsd ? `Enter the amount in ${a.symbol} instead` : 'Enter the amount in dollars instead';
  let qty = 0n, dollars = 0n;
  try { if (unitUsd) dollars = toAtoms($('s-amount').value || '0', USDC); else qty = toAtoms($('s-amount').value || '0', asset); } catch (e) { $('s-quote').textContent = e.message; $('s-go').disabled = true; return; }
  if (qty <= 0n && dollars <= 0n) { $('s-quote').textContent = 'Enter an amount.'; $('s-go').disabled = true; return; }
  const b = await j(`/book?market=${encodeURIComponent(asset + '/' + USDC)}`);
  if (run !== computeSeq) return;
  // Buying takes asks, cheapest first; selling takes bids, highest first.
  const orders = side === 'buy' ? [...(b.asks || [])].sort((x, y) => x.price - y.price) : [...(b.bids || [])].sort((x, y) => y.price - x.price);
  if (unitUsd) {
    // Walk the book until the dollars are used up, and that is the quantity.
    let budget = dollars;
    for (const o of orders) { if (budget <= 0n) break; const affordable = (budget * 100000000n) / BigInt(o.price); const take = affordable < BigInt(o.qty_left) ? affordable : BigInt(o.qty_left); qty += take; budget -= quoteAmount(take, o.price); }
    if (qty <= 0n) { $('s-quote').textContent = `Nobody is ${side === 'buy' ? 'selling' : 'buying'} ${a.symbol} right now.`; $('s-go').disabled = true; return; }
  }
  if (qty > 0n && qty < BigInt(a.min_quantity)) {
    const minUsd = orders.length ? quoteAmount(BigInt(a.min_quantity), orders[0].price) : null;
    $('s-quote').textContent = `The smallest ${a.symbol} trade is ${fromAtoms(a.min_quantity, asset)}${minUsd ? ` (about ${fromAtoms(minUsd, USDC)})` : ''}.`; $('s-go').disabled = true; return;
  }
  let left = qty, quote = 0n; const fills = [];
  for (const o of orders) { if (left <= 0n) break; const take = left < BigInt(o.qty_left) ? left : BigInt(o.qty_left); fills.push({ order: o, qty: take }); quote += quoteAmount(take, o.price); left -= take; }
  if (!fills.length) { $('s-quote').textContent = `Nobody is ${side === 'buy' ? 'selling' : 'buying'} ${a.symbol} right now.`; $('s-go').disabled = true; return; }
  plan = { fills, quote, short: left };
  const have = side === 'buy' ? quote : qty; const haveAsset = side === 'buy' ? USDC : asset;
  const mine = myBalance(haveAsset);
  const avg = fills.length ? Number(quote) / Number(qty - left) * 1e8 : 0;
  let yieldNote = '';
  if (asset === VAULT.asset) {
    const v = await loadVault(); if (run !== computeSeq) return;
    if (v && v.sharePrice) {
      const worth = Number(qty - left) / 1e6 * v.sharePrice; const paid = Number(quote) / 1e6;
      const diff = side === 'buy' ? paid - worth : worth - paid;
      const fmtUsd = (x) => '$' + Math.abs(x).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 });
      yieldNote = `<br>Earning <b>${(100 * (v.netApy ?? v.apy)).toFixed(2)}%</b> now · ${diff >= 0 ? fmtUsd(diff) + (side === 'buy' ? ' to get in' : ' to get out') : fmtUsd(diff) + ' bonus'} · worth ${fmtUsd(worth)} <a href="${VAULT.url}" target="_blank" rel="noreferrer">Morpho</a>`;
    }
  }
  $('s-quote').innerHTML = (unitUsd ? `${side === 'buy' ? 'You get' : 'You sell'} <b>${fromAtoms(qty - left, asset)}</b> for ${fromAtoms(quote, USDC)}` : `${side === 'buy' ? 'You pay' : 'You get'} <b>${fromAtoms(quote, USDC)}</b>`) + (left > 0n ? ` for ${fromAtoms(qty - left, asset)}; only that much is on the book` : '') + ` · ${fmt(priceToHuman(BigInt(Math.round(avg)), asset, USDC), 6)} USDC/${a.symbol}` + (mine !== null && mine < have ? ` · you have ${fromAtoms(mine, haveAsset)}` : '');
  $('s-quote').innerHTML += yieldNote;
  $('s-go').disabled = !me() || (mine !== null && mine < have);
}
let balances = null;
function myBalance(asset) { if (!balances) return null; return asset === 'BLRD' ? BigInt(balances.blrd || 0) : BigInt(balances.assets?.find((x) => x.asset === asset)?.amount || 0); }
$('s-go').onclick = async () => {
  if (!plan) return; $('s-error').textContent = ''; $('s-result').textContent = ''; $('s-go').disabled = true;
  const asset = $('s-asset').value; const a = assets[asset];
  try {
    ensureKey();
    let got = 0n, paid = 0n;
    for (const f of plan.fills) {
      await send(api, null, TxType.Claim, { order_id: hex.dec(f.order.id), quantity: f.qty, receive_address: '' });
      got += f.qty; paid += quoteAmount(f.qty, f.order.price);
    }
    $('s-result').textContent = side === 'buy' ? `Bought ${fromAtoms(got, asset)} for ${fromAtoms(paid, USDC)}.` : `Sold ${fromAtoms(got, asset)} for ${fromAtoms(paid, USDC)}.`;
    $('s-amount').value = ''; plan = null; $('s-quote').textContent = 'Enter an amount.';
    renderAccount(); 
  } catch (e) { $('s-error').textContent = e.message; $('s-go').disabled = false; }
};
async function loadSimple() { if (me()) balances = await j(`/balances/${me()}`).catch(() => null); renderTokenButtons(); computeSimple(); }

// ------------------------------------------------------------------ pro mode
let chart = null, candles = null, chartMarket = null;
function chartColors() {
  const cs = getComputedStyle(document.documentElement);
  return { text: cs.getPropertyValue('--muted').trim() || '#666', line: cs.getPropertyValue('--line').trim() || '#e4e4e4' };
}
function ensureChart() {
  if (chart || !window.LightweightCharts) return;
  const c = chartColors();
  chart = LightweightCharts.createChart($('chart'), {
    layout: { background: { type: 'solid', color: 'transparent' }, textColor: c.text },
    grid: { vertLines: { color: c.line }, horzLines: { color: c.line } },
    rightPriceScale: { borderColor: c.line }, timeScale: { borderColor: c.line, timeVisible: true },
    autoSize: true,
  });
  candles = chart.addCandlestickSeries({ upColor: '#1a7f37', downColor: '#b42318', borderVisible: false, wickUpColor: '#1a7f37', wickDownColor: '#b42318' });
}
function fitChart() { if (chart) chart.timeScale().fitContent(); }
const advanced = () => { try { return localStorage.getItem('bollard.advanced') === '1'; } catch { return false; } };
async function loadChart() {
  $('chart-wrap').classList.toggle('hidden', !advanced());
  if (!advanced()) return;
  ensureChart(); if (!chart) return;
  const market = currentMarket(); if (!market || marketProblem()) return;
  const [base, q] = market.split('/');
  const trades = await j(`/trades?market=${encodeURIComponent(market)}`).catch(() => []);
  const bucket = 15 * 60; const byT = new Map();
  for (const t of trades) {
    const time = Math.floor(t.time / 1000 / bucket) * bucket;
    const p = priceToHuman(t.price, base, q);
    const c = byT.get(time);
    if (!c) byT.set(time, { time, open: p, high: p, low: p, close: p });
    else { c.high = Math.max(c.high, p); c.low = Math.min(c.low, p); c.close = p; }
  }
  const data = [...byT.values()].sort((a, b) => a.time - b.time);
  candles.setData(data);
  chartMarket = market;
  $('chart-empty').textContent = data.length ? '' : 'No trades on this market yet; the chart fills as claims happen. 15-minute candles.';
  $('last-price').textContent = trades.length ? `last ${fmt(priceToHuman(trades[trades.length - 1].price, base, q))} ${assets[q].symbol}/${assets[base].symbol}` : '';
  fitChart();
}

function orderRow(o, cls) {
  const tr = document.createElement('tr');
  const [base, q] = o.market.split('/');
  tr.style.cursor = 'pointer'; tr.title = 'take this order'; tr.onclick = () => pickOrder(o);
  tr.innerHTML = `<td class="mono ${cls}">${short(o.id)}</td><td class="num">${fmt(priceToHuman(o.price, base, q))}</td><td class="num">${fromAtoms(o.qty_left, base)}</td><td class="num">${o.settle === 0 ? 'escrowed' : fromAtoms(o.bond_left, 'BLRD') + ' BLRD'}</td><td>${policy[o.fill_policy]}</td>`;
  return tr;
}
function marketProblem() {
  const [b] = currentMarket().split('/');
  if (b === 'eth:USDC') return 'USDC is the quote for every market; pick what to trade against it';
  return '';
}
async function loadBook() {
  const market = currentMarket(); if (!market) return;
  const problem = marketProblem(); if (problem) { $('market-note').textContent = problem; $('bids').innerHTML = $('asks').innerHTML = ''; return; }
  const [base, q] = market.split('/');
  $('market-note').textContent = `price in ${assets[q]?.symbol} per ${assets[base]?.symbol}`;
  const b = await j('/book?market=' + encodeURIComponent(market));
  if (b.error) { $('market-note').textContent = b.error; return; }
  $('bids').replaceChildren(...b.bids.map((o) => orderRow(o, 'bid')));
  $('asks').replaceChildren(...b.asks.map((o) => orderRow(o, 'ask')));
  if (!b.bids.length && !b.asks.length) { const tr = document.createElement('tr'); tr.innerHTML = '<td colspan="5" class="muted">no resting orders</td>'; $('bids').appendChild(tr); }
}
// ---- taking: click a book row, enter a quantity, claim it
let picked = null;
function pickOrder(o) {
  picked = o; const [base, q] = o.market.split('/');
  $('t-desc').textContent = `${o.side ? 'buy' : 'sell'} ${assets[base]?.symbol} at ${fmt(priceToHuman(o.price, base, q))} ${assets[q]?.symbol}/${assets[base]?.symbol} (${fromAtoms(o.qty_left, base)} left${o.settle === 1 ? ', paid on chain' : ''})`;
  $('t-unit').textContent = assets[base]?.symbol || ''; $('t-qty').value = fromAtoms(o.qty_left, base, false); $('t-result').textContent = ''; $('t-error').textContent = '';
  $('t-qty').dispatchEvent(new Event('input')); $('t-qty').focus();
}
$('t-qty').addEventListener('input', () => {
  try { if (!picked) throw 0; const [base, q] = picked.market.split('/'); const qa = quoteAmount(toAtoms($('t-qty').value, base), picked.price); $('t-total').textContent = `${picked.side ? 'you pay' : 'you get'} ${fromAtoms(qa, q)}`; } catch { $('t-total').textContent = ''; }
});
$('take-form').onsubmit = async (ev) => {
  ev.preventDefault(); $('t-error').textContent = ''; $('t-result').textContent = '';
  try {
    if (!picked) throw new Error('Pick an order in the book first.');
    const [base] = picked.market.split('/');
    const quantity = toAtoms($('t-qty').value, base);
    const out = await send(api, ensureKey(), TxType.Claim, { order_id: hex.dec(picked.id), quantity, receive_address: '' });
    if (picked.settle === 1) {
      const ms = await j(`/matches?account=${me()}&kind=deposit&limit=5`); const m = ms.find((x) => x.id === out.hash);
      $('t-result').innerHTML = m ? `Pay <b>${fromAtoms(m.taker_amount, m.taker_asset)}</b> to <code class="addr">${m.taker_pay_to}</code>. The options are yours once it is in a block; the wallet watches for it.` : `claimed ${short(out.hash)}`;
    } else {
      $('t-result').textContent = `Done: ${fromAtoms(quantity, base)} settled on the ledger (${short(out.hash)}).`;
    }
    picked = null; $('t-desc').textContent = 'none picked'; $('t-qty').value = ''; $('t-total').textContent = '';
    loadPro();
  } catch (e) { $('t-error').textContent = e.message; }
};

function legText(txid, included) { return txid ? `<span class="mono">${short(txid, 12)}</span> ${included ? 'in block' : 'reported'}` : '<span class="muted">waiting</span>'; }
async function loadMine() {
  const pk = me();
  if (!pk) { $('my-matches').innerHTML = '<tr><td colspan="7" class="muted">no key yet</td></tr>'; return; }
  const ms = (await j(`/matches?account=${pk}&limit=50`).catch(() => [])).filter((m) => m.taker === pk);
  $('my-matches').replaceChildren(...ms.map((m) => {
    const tr = document.createElement('tr'); const base = m.market.split('/')[0];
    tr.innerHTML = `<td class="mono">${short(m.id)}</td><td>${m.market}</td><td class="num">${fromAtoms(m.quantity, base)}</td><td>${m.taker_chain === 'ledger' ? 'ledger' : legText(m.taker_txid, m.taker_included_height)}</td><td>${m.maker_chain === 'ledger' || m.maker_chain === 'blrd' ? 'ledger' : legText(m.maker_txid, m.maker_included_height)}</td><td>${m.status}</td><td></td>`;
    if (m.status === 'open' && !m.taker_txid && m.taker_chain !== 'ledger') {
      const cell = tr.lastElementChild; const inp = document.createElement('input'); inp.placeholder = `txid: pay ${fromAtoms(m.taker_amount, m.taker_asset)} to ${short(m.taker_pay_to, 10)}`; inp.style.width = '22em';
      const b = document.createElement('button'); b.className = 'small'; b.textContent = 'Report';
      b.onclick = async () => { try { await send(api, null, TxType.ReportPayment, { match_id: hex.dec(m.id), leg: 0, txid: inp.value.trim() }); loadPro(); } catch (e) { $('t-error').textContent = e.message; } };
      cell.append(inp, ' ', b);
    }
    return tr;
  }));
  if (!ms.length) $('my-matches').innerHTML = '<tr><td colspan="7" class="muted">none</td></tr>';
}
async function loadMatches() {
  const ms = await j('/matches?status=open&limit=50');
  $('matches').replaceChildren(...ms.map((m) => {
    const tr = document.createElement('tr'); const [base, q] = m.market.split('/');
    tr.innerHTML = `<td class="mono">${short(m.id)}</td><td>${m.market}</td><td class="num">${fromAtoms(m.quantity, base)}</td><td class="num">${q ? fmt(priceToHuman(m.price, base, q)) : m.kind}</td><td>${legText(m.taker_txid, m.taker_included_height)}</td><td>${m.maker_chain === 'blrd' ? 'ledger' : legText(m.maker_txid, m.maker_included_height)}</td><td>${new Date(m.claimed_ts).toLocaleString()}</td>`;
    return tr;
  }));
  if (!ms.length) $('matches').innerHTML = '<tr><td colspan="7" class="muted">none</td></tr>';
}
function logLine(ev) {
  const li = document.createElement('li'); const t = new Date().toLocaleTimeString(); let text = ev.event;
  if (ev.event === 'Entry') text = `#${ev.seq} ${ev.kind}`;
  else if (ev.event === 'OrderPlaced') text = `order ${short(ev.order)} ${ev.side ? 'sell' : 'buy'} ${ev.market} qty ${ev.quantity} @ ${ev.price}`;
  else if (ev.event === 'Claimed') text = `claim ${short(ev.match)} on ${short(ev.order)} qty ${ev.quantity}`;
  else if (ev.event === 'EpochStart') text = `epoch ${ev.height} led by ${short(ev.leader, 12)}, reward ${ev.reward} rill`;
  else text = ev.event + ' ' + Object.entries(ev).filter(([k]) => k !== 'event').map(([k, v]) => `${k}=${typeof v === 'string' ? short(v, 14) : v}`).join(' ');
  li.innerHTML = `<span class="muted mono">${t}</span> &nbsp; ${text}`;
  const log = $('log'); log.prepend(li); while (log.children.length > 60) log.lastChild.remove();
}
async function loadPro() { await Promise.all([loadStatus(), loadBook(), loadChart(), loadMine(), loadMatches(), renderAccount()]); }
for (const id of ['m-base-asset', 'm-quote-asset']) $(id).addEventListener('input', () => { loadBook(); loadChart(); });

// ------------------------------------------------------------------ boot
(async () => {
  // Inside the wallet's Trade tab the page has its own menubar, so hide this one.
  if (window.self !== window.top) document.body.classList.add('embedded');
  if (API) { try { const r = await fetch(API + '/status', { signal: AbortSignal.timeout(4000) }); if (!r.ok) api = ''; } catch { api = ''; } }
  await loadMarkets();
  await loadStatus();
  renderAccount();
  $('simple').classList.toggle('hidden', advanced()); $('pro').classList.toggle('hidden', !advanced());
  const tick = () => (advanced() ? loadPro() : loadSimple());
  tick(); if (advanced()) setTimeout(fitChart, 0);
  const es = new EventSource(api + '/events');
  es.onmessage = (m) => { try { const ev = JSON.parse(m.data); logLine(ev); if (ev.event === 'Entry') { loadStatus(); return; } tick(); } catch {} };
  setInterval(() => { loadStatus(); tick(); }, 20000);
})();
