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
function ensureKey() { if (!key) { $('account').showModal(); throw new Error('Sign in first (top right).'); } return key; }
async function renderAccount() {
  const pk = me();
  $('account-btn').textContent = pk ? short(pk, 12) : 'Sign in';
  $('a-pub').textContent = pk || '(not signed in)';
  $('a-signed').classList.toggle('hidden', !pk);
  $('a-unsigned').classList.toggle('hidden', !!pk);
  if (pk) {
    const a = await j(`/account/${pk}`).catch(() => null);
    const bal = a ? fromAtoms(a.balance, 'BLRD') : '?';
    $('a-balance').textContent = a ? `${bal} (${fromAtoms(a.locked, 'BLRD')} locked, nonce ${a.nonce})` : '?';
    $('my-blrd').textContent = bal;
    const b = await j(`/balances/${pk}`).catch(() => null);
    $('a-assets').innerHTML = b ? [...(b.assets || []).filter((x) => x.amount > 0).map((x) => `<div class="kv"><span>${assets[x.asset]?.symbol || x.asset}</span><span>${fromAtoms(x.amount, x.asset)}</span></div>`), ...(b.issuing || []).map((x) => `<div class="kv"><span>issued ${assets[x.asset]?.symbol || x.asset}</span><span>${fromAtoms(x.issued, x.asset)} (cover ${fromAtoms(x.cover, 'BLRD')})</span></div>`)].join('') : '';
  } else { $('a-balance').textContent = '–'; $('my-blrd').textContent = '–'; $('a-assets').innerHTML = ''; }
}
$('account-btn').onclick = () => { renderAccount(); $('account').showModal(); };
$('a-close').onclick = () => $('account').close();
$('a-copy').onclick = () => navigator.clipboard.writeText(me() || '');
$('a-create').onclick = async () => {
  $('a-error').textContent = '';
  try { const k = await createPasskey('Bollard'); await fetch(api + '/keys', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ cred_id: k.id, pubkey: k.pubkey }) }); setKey(k); } catch (e) { $('a-error').textContent = e.message; }
};
$('a-signin').onclick = async () => {
  $('a-error').textContent = '';
  try {
    const id = await signInPasskey();
    const r = await fetch(`${api}/keys/${encodeURIComponent(id)}`); if (!r.ok) throw new Error('That passkey is not registered with this node.');
    setKey({ id, pubkey: (await r.json()).pubkey });
  } catch (e) { $('a-error').textContent = e.message; }
};
$('a-signout').onclick = () => { setKey(null); };

// ------------------------------------------------------------------ modes
const MODE = 'bollard.mode';
function setMode(m) {
  $('simple').classList.toggle('hidden', m !== 'simple');
  $('pro').classList.toggle('hidden', m !== 'pro');
  $('mode-simple').classList.toggle('on', m === 'simple');
  $('mode-pro').classList.toggle('on', m === 'pro');
  try { localStorage.setItem(MODE, m); } catch {}
  if (m === 'pro') { loadPro(); setTimeout(fitChart, 0); }
}
$('mode-simple').onclick = () => setMode('simple');
$('mode-pro').onclick = () => setMode('pro');

// ------------------------------------------------------------------ status
async function loadStatus() {
  status = await j('/status');
  const r = status.role; const el = $('role');
  el.textContent = r.role === 'Leader' ? 'node is the leader' : r.role === 'Follower' ? 'following ' + r.url : r.role.toLowerCase() + (r.reason ? ': ' + r.reason : '');
  el.className = 'pill' + (r.role === 'Leader' ? ' leader' : '');
  $('height').textContent = status.bitcoin ? status.bitcoin.height.toLocaleString() : '–';
  $('head').textContent = status.head.seq.toLocaleString();
  $('leader').textContent = status.epoch ? status.epoch.leader_key_hash : '–';
  $('open-orders').textContent = status.counts.open_orders;
  $('open-matches').textContent = status.counts.open_matches;
}

async function loadMarkets() {
  marketsInfo = await j('/markets');
  for (const a of marketsInfo.assets) assets[a.id] = a;
  $('i-asset').innerHTML = marketsInfo.assets.filter((a) => a.chain !== 'blrd').map((a) => `<option value="${a.id}">${a.symbol} (${a.chain})</option>`).join('');
  renderTokenButtons();
  buildPicker();
}

// ------------------------------------------------------------------ simple mode
const CHAIN_NAMES = { btc: 'Bitcoin', eth: 'Ethereum', blrd: 'Bollard' };
const ASSET_NAMES = { 'btc:BTC': 'Bitcoin', 'eth:ETH': 'Ether', 'eth:USDC': 'USD Coin', 'BLRD': 'Bollard' };
const coinClass = (a) => 'c-' + (a.symbol.toLowerCase() === 'usdc' || a.symbol.toLowerCase() === 'usdt' ? a.symbol.toLowerCase() : a.chain);
function coinIcon(a, large = false) {
  const net = a.chain === 'blrd' ? '' : `<span class="net c-${a.chain}">${a.chain === 'btc' ? '₿' : 'Ξ'}</span>`;
  return `<span class="coin ${large ? 'lg ' : ''}${coinClass(a)}">${a.symbol[0]}${net}</span>`;
}
function renderTokenButtons() {
  for (const btn of document.querySelectorAll('.token-btn')) {
    const a = assets[$(btn.dataset.for).value];
    if (!a) continue;
    btn.innerHTML = `${coinIcon(a)}<span>${a.symbol}<br><span class="sub">${CHAIN_NAMES[a.chain] || a.chain}</span></span><span class="chev">▾</span>`;
  }
}
// ---- token picker: search, chain chips, popular, full list
let pickerFor = null, pickerChain = 'all';
function buildPicker() {
  const chips = $('p-chains'); chips.innerHTML = '';
  const allBtn = document.createElement('button'); allBtn.textContent = 'All'; allBtn.dataset.chain = 'all'; chips.appendChild(allBtn);
  for (const c of [...new Set(Object.values(assets).map((a) => a.chain))]) {
    const b = document.createElement('button'); b.dataset.chain = c; b.title = CHAIN_NAMES[c] || c;
    b.innerHTML = `<span class="coin c-${c}" style="width:22px;height:22px;font-size:11px">${(CHAIN_NAMES[c] || c)[0]}</span>`;
    chips.appendChild(b);
  }
  chips.onclick = (e) => { const b = e.target.closest('button'); if (!b) return; pickerChain = b.dataset.chain; renderPicker(); };
  $('p-search').oninput = renderPicker;
  $('p-close').onclick = () => $('picker').close();
  for (const btn of document.querySelectorAll('.token-btn')) btn.onclick = () => openPicker(btn.dataset.for);
  $('picker').addEventListener('click', (e) => { if (e.target === $('picker')) $('picker').close(); });
}
function openPicker(forId) {
  pickerFor = forId; pickerChain = 'all'; $('p-search').value = '';
  renderPicker(); $('picker').showModal(); $('p-search').focus();
}
function choose(id) {
  const inp = $(pickerFor); inp.value = id; $('picker').close();
  renderTokenButtons(); inp.dispatchEvent(new Event('input', { bubbles: true }));
}
function renderPicker() {
  for (const b of $('p-chains').children) b.classList.toggle('on', b.dataset.chain === pickerChain);
  const q = $('p-search').value.trim().toLowerCase();
  const current = pickerFor ? $(pickerFor).value : '';
  const list = Object.values(assets).filter((a) => (pickerChain === 'all' || a.chain === pickerChain) && (!q || a.symbol.toLowerCase().includes(q) || (ASSET_NAMES[a.id] || '').toLowerCase().includes(q) || a.id.toLowerCase().includes(q)));
  $('p-popular').replaceChildren(...list.slice(0, 4).map((a) => { const b = document.createElement('button'); b.innerHTML = `${coinIcon(a)}${a.symbol}`; b.onclick = () => choose(a.id); return b; }));
  $('p-list').replaceChildren(...list.map((a) => {
    const b = document.createElement('button'); b.className = 'token-row' + (a.id === current ? ' on' : '');
    b.innerHTML = `${coinIcon(a, true)}<span><div class="name">${ASSET_NAMES[a.id] || a.symbol}</div><div class="sym">${a.symbol}</div></span><span class="right">${CHAIN_NAMES[a.chain] || a.chain}${a.contract ? ' · ' + a.contract.slice(0, 6) + '…' + a.contract.slice(-4) : ''}</span>`;
    b.onclick = () => choose(a.id); return b;
  }));
  if (!list.length) $('p-list').innerHTML = '<div class="picker-label">Nothing matches.</div>';
}
function currentMarket() { return `${$('m-base-asset').value}/${$('m-quote-asset').value}`; }
let quote = null; // { order, qty, payAtoms, getAtoms, payAsset, getAsset, market, side }

async function bestQuote(payAsset, getAsset, payAtoms) {
  const candidates = [];
  // Asks on get/pay: the maker sells `get` for `pay`; we pay quote, receive base.
  const m1 = `${getAsset}/${payAsset}`;
  const b1 = await j(`/book?market=${encodeURIComponent(m1)}`).catch(() => null);
  if (b1 && b1.asks) for (const o of b1.asks) {
    let qty = (payAtoms * 100000000n) / BigInt(o.price);
    if (qty > BigInt(o.qty_left)) qty = BigInt(o.qty_left);
    if (qty < BigInt(assets[getAsset].min_quantity)) continue;
    const pay = quoteAmount(qty, o.price);
    if (pay < BigInt(assets[payAsset].min_quantity)) continue;
    candidates.push({ order: o, market: m1, side: 'ask', qty, payAtoms: pay, getAtoms: qty, payAsset, getAsset });
  }
  // Bids on pay/get: the maker buys `pay` with `get`; we pay base, receive quote.
  const m2 = `${payAsset}/${getAsset}`;
  const b2 = await j(`/book?market=${encodeURIComponent(m2)}`).catch(() => null);
  if (b2 && b2.bids) for (const o of b2.bids) {
    let qty = payAtoms; if (qty > BigInt(o.qty_left)) qty = BigInt(o.qty_left);
    if (qty < BigInt(assets[payAsset].min_quantity)) continue;
    const get = quoteAmount(qty, o.price);
    if (get < BigInt(assets[getAsset].min_quantity)) continue;
    candidates.push({ order: o, market: m2, side: 'bid', qty, payAtoms: qty, getAtoms: get, payAsset, getAsset });
  }
  if (!candidates.length) return null;
  // Best = most received per unit paid.
  candidates.sort((a, b) => Number(b.getAtoms * 10n ** 18n / b.payAtoms - a.getAtoms * 10n ** 18n / a.payAtoms));
  return candidates[0];
}

let quoteTimer = null;
function scheduleQuote() { clearTimeout(quoteTimer); quoteTimer = setTimeout(computeQuote, 250); }
async function computeQuote() {
  quote = null; $('s-error').textContent = '';
  const payAsset = $('s-pay-asset').value, getAsset = $('s-get-asset').value;
  const needsAddr = getAsset !== 'BLRD';
  $('s-addr-box').classList.toggle('hidden', !needsAddr);
  $('s-addr-label').textContent = needsAddr ? `Receive address on ${assets[getAsset]?.chain || ''}` : '';
  const go = $('s-go'); go.disabled = true;
  if (payAsset === getAsset) { $('s-quote').textContent = 'Pick two different tokens.'; go.textContent = 'Pick two tokens'; return; }
  let payAtoms;
  try { payAtoms = toAtoms($('s-pay-amount').value || '0', payAsset); } catch (e) { $('s-quote').textContent = e.message; go.textContent = 'Enter an amount'; return; }
  if (payAtoms === 0n) { $('s-get-amount').value = ''; $('s-quote').textContent = 'Enter an amount.'; go.textContent = 'Enter an amount'; return; }
  $('s-quote').textContent = 'Finding the best price…';
  const q = await bestQuote(payAsset, getAsset, payAtoms);
  if (!q) { $('s-get-amount').value = ''; $('s-quote').textContent = `Nobody is selling ${assets[getAsset].symbol} for ${assets[payAsset].symbol} right now.`; go.textContent = 'Nothing to buy'; return; }
  quote = q;
  $('s-get-amount').value = fromAtoms(q.getAtoms, getAsset, false);
  const rate = Number(q.getAtoms) / 10 ** assets[getAsset].decimals / (Number(q.payAtoms) / 10 ** assets[payAsset].decimals);
  const partial = q.payAtoms < payAtoms ? ` Only ${fromAtoms(q.payAtoms, payAsset)} fits this order; you pay that.` : '';
  $('s-quote').innerHTML = `1 ${assets[payAsset].symbol} = ${fmt(rate)} ${assets[getAsset].symbol} · order <span class="mono">${short(q.order.id)}</span> · maker bond ${fromAtoms(q.order.bond_left || q.order.qty_left, 'BLRD')} · fills on ${policy[q.order.fill_policy]}.${partial}`;
  go.textContent = `Claim ${fromAtoms(q.getAtoms, getAsset)}`;
  go.disabled = false;
}
for (const id of ['s-pay-amount', 's-pay-asset', 's-get-asset']) $(id).addEventListener('input', scheduleQuote);
$('s-flip').onclick = () => {
  const pa = $('s-pay-asset').value; $('s-pay-asset').value = $('s-get-asset').value; $('s-get-asset').value = pa;
  $('s-pay-amount').value = $('s-get-amount').value; renderTokenButtons(); scheduleQuote();
};

const SWAP = 'bollard.swap';
let swap = null;
try { swap = JSON.parse(localStorage.getItem(SWAP) || 'null'); } catch {}

$('s-go').onclick = async () => {
  if (!quote) return;
  const addr = $('s-addr').value.trim();
  if (quote.getAsset !== 'BLRD' && !addr) { $('s-error').textContent = 'Enter the address where you want to receive ' + assets[quote.getAsset].symbol + '.'; return; }
  $('s-go').disabled = true; $('s-error').textContent = '';
  try {
    const out = await send(api, ensureKey(), TxType.Claim, { order_id: hex.dec(quote.order.id), quantity: quote.qty, receive_address: addr });
    const ms = await j(`/matches?account=${me()}&limit=5`);
    const m = ms.find((x) => x.id === out.hash);
    swap = { match: out.hash, payAsset: quote.payAsset, getAsset: quote.getAsset, payAtoms: m ? m.taker_amount : quote.payAtoms.toString(), payTo: m ? m.taker_pay_to : quote.order.recv_addr, getAtoms: quote.getAtoms.toString(), reported: false, at: Date.now() };
    try { localStorage.setItem(SWAP, JSON.stringify(swap)); } catch {}
    showSwap();
  } catch (e) { $('s-error').textContent = e.message; $('s-go').disabled = false; }
};

function showSwap() {
  // One thing at a time: while a swap is in flight the swap card gives way to the payment panel.
  $('s-card').classList.toggle('hidden', !!swap);
  if (!swap) { $('s-swap').classList.add('hidden'); return; }
  $('s-swap').classList.remove('hidden');
  $('s-pay-exact').textContent = fromAtoms(swap.payAtoms, swap.payAsset);
  $('s-pay-chain-name').textContent = assets[swap.payAsset]?.chain === 'btc' ? 'Bitcoin' : assets[swap.payAsset]?.chain === 'eth' ? 'Ethereum' : assets[swap.payAsset]?.chain;
  $('s-pay-to').textContent = swap.payTo;
  $('s-match').textContent = short(swap.match, 16);
  try {
    const uri = assets[swap.payAsset]?.chain === 'btc' ? `bitcoin:${swap.payTo}?amount=${fromAtoms(swap.payAtoms, swap.payAsset, false)}` : swap.payTo;
    const qr = qrcode(0, 'M'); qr.addData(uri); qr.make(); $('s-qr').innerHTML = qr.createSvgTag({ cellSize: 3, margin: 2 });
  } catch { $('s-qr').innerHTML = ''; }
  $('s-manual').classList.toggle('hidden', swap.reported);
  $('s-done').classList.toggle('hidden', !swap.reported);
  $('s-watch').classList.toggle('hidden', swap.reported);
  trackSwap();
  watchPayment();
}

let watching = false;
async function watchPayment() {
  if (watching) return; watching = true;
  try {
    while (swap && !swap.reported) {
      try {
        const r = await j(`/find-payment?match=${swap.match}&leg=0`);
        if (r.txid) {
          $('s-watch').textContent = `Payment seen: ${short(r.txid, 14)}. Reporting it…`;
          if (!r.reported) await send(api, ensureKey(), TxType.ReportPayment, { match_id: hex.dec(swap.match), leg: 0, txid: r.txid });
          swap.reported = true; swap.txid = r.txid; try { localStorage.setItem(SWAP, JSON.stringify(swap)); } catch {}
          showSwap(); break;
        }
      } catch (e) { $('s-watch').textContent = 'Watching for your payment… (' + e.message + ')'; }
      await new Promise((r) => setTimeout(r, 8000));
    }
  } finally { watching = false; }
}
$('s-copy').onclick = async () => {
  try { await navigator.clipboard.writeText(swap?.payTo || ''); } catch {}
  const b = $('s-copy'); b.textContent = 'Copied'; b.classList.add('ok'); setTimeout(() => { b.textContent = 'Copy address'; b.classList.remove('ok'); }, 1500);
};
$('s-report').onclick = async () => {
  const txid = $('s-txid').value.trim();
  if (!/^(0x)?[0-9a-fA-F]{64}$/.test(txid)) { $('s-status').textContent = 'That does not look like a transaction id.'; return; }
  try {
    await send(api, ensureKey(), TxType.ReportPayment, { match_id: hex.dec(swap.match), leg: 0, txid });
    swap.reported = true; try { localStorage.setItem(SWAP, JSON.stringify(swap)); } catch {}
    showSwap();
  } catch (e) { $('s-status').textContent = e.message; }
};
$('s-done').onclick = () => { swap = null; try { localStorage.removeItem(SWAP); } catch {} showSwap(); $('s-go').disabled = false; scheduleQuote(); };

function matchStatusText(m, role) {
  if (m.status === 'expired') return 'Expired: no payment was reported in time.';
  if (m.status === 'void') return 'Void: ' + (m.closed_reason || '');
  if (m.status === 'missed') return role === 'taker' ? 'The maker did not deliver; their bond was paid to you.' : 'You did not deliver; your bond went to the taker.';
  if (m.status === 'closed') return m.closed_reason === 'settled on the ledger' ? 'Done: BLRD credited to your account.' : 'Done: both payments delivered.';
  if (!m.taker_txid) return role === 'taker' ? 'Waiting for your payment.' : 'Waiting for the taker to pay.';
  if (!m.taker_included_height) return role === 'taker' ? 'Your payment is on its way; waiting for it to land in a block.' : 'Taker payment seen; waiting for it to land in a block.';
  if (m.maker_chain === 'blrd') return 'Taker payment in a block; settling.';
  if (!m.maker_txid) return role === 'taker' ? 'Your payment is in a block; waiting for the maker to deliver.' : 'The taker has paid; your turn to deliver.';
  if (!m.maker_included_height) return 'Maker payment reported; waiting for it to land in a block.';
  return 'Maker payment in a block; bond releases when it is deep.';
}
let celebrating = false;
function celebrate() {
  if (celebrating) return; celebrating = true;
  $('s-success').classList.remove('hidden');
  setTimeout(() => {
    $('s-success').classList.add('hidden'); celebrating = false;
    swap = null; try { localStorage.removeItem(SWAP); } catch {}
    showSwap(); $('s-go').disabled = false; $('s-pay-amount').value = ''; $('s-get-amount').value = ''; scheduleQuote(); loadHistory(); renderAccount();
  }, 2800);
}
async function trackSwap() {
  if (!swap) return;
  const ms = await j(`/matches?account=${me()}&limit=20`).catch(() => []);
  const m = ms.find((x) => x.id === swap.match);
  if (!m) { $('s-status').textContent = 'Match not found on this node yet.'; return; }
  $('s-status').textContent = matchStatusText(m, 'taker');
  if (m.status === 'closed') celebrate();
}
async function loadHistory() {
  const pk = me(); if (!pk) { $('s-history').innerHTML = '<tr><td colspan="6" class="muted">no key yet</td></tr>'; return; }
  const ms = await j(`/matches?account=${pk}&limit=50`).catch(() => []);
  const mine = ms.filter((m) => m.taker === pk);
  $('s-history').replaceChildren(...mine.map((m) => {
    const tr = document.createElement('tr');
    const base = m.market.split('/')[0];
    tr.innerHTML = `<td>${new Date(m.claimed_ts).toLocaleString()}</td><td>${m.market}</td><td class="num">${fromAtoms(m.quantity, base)}</td><td>${fromAtoms(m.taker_amount, m.taker_asset)}</td><td>${fromAtoms(m.maker_amount, m.maker_asset)}</td><td>${matchStatusText(m, 'taker')}</td>`;
    return tr;
  }));
  if (!mine.length) $('s-history').innerHTML = '<tr><td colspan="6" class="muted">none yet</td></tr>';
}

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
async function loadChart() {
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
  tr.innerHTML = `<td class="mono ${cls}">${short(o.id)}</td><td class="num">${fmt(priceToHuman(o.price, base, q))}</td><td class="num">${fromAtoms(o.qty_left, base)}</td><td class="num">${o.settle === 0 ? 'escrowed' : fromAtoms(o.bond_left, 'BLRD') + ' BLRD'}</td><td>${policy[o.fill_policy]}</td>`;
  return tr;
}
function marketProblem() {
  const [b, q] = currentMarket().split('/');
  if (b === q) return 'base and quote must differ';
  if (q === 'BLRD') return 'BLRD trades as the base asset (sell BLRD for the other side)';
  return '';
}
async function loadBook() {
  const market = currentMarket(); if (!market) return;
  const problem = marketProblem(); if (problem) { $('market-note').textContent = problem; $('bids').innerHTML = $('asks').innerHTML = ''; return; }
  const [base, q] = market.split('/');
  $('market-note').textContent = `price in ${assets[q]?.symbol} per ${assets[base]?.symbol}`;
  $('o-qty-unit').textContent = assets[base]?.symbol || ''; $('o-price-unit').textContent = `${assets[q]?.symbol}/${assets[base]?.symbol}`;
  updateRecvHint();
  const b = await j('/book?market=' + encodeURIComponent(market));
  if (b.error) { $('market-note').textContent = b.error; return; }
  $('bids').replaceChildren(...b.bids.map((o) => orderRow(o, 'bid')));
  $('asks').replaceChildren(...b.asks.map((o) => orderRow(o, 'ask')));
  if (!b.bids.length && !b.asks.length) { const tr = document.createElement('tr'); tr.innerHTML = '<td colspan="5" class="muted">no resting orders</td>'; $('bids').appendChild(tr); }
}
function updateRecvHint() {
  const [base, q] = (currentMarket() || '/').split('/');
  const buying = $('o-side').value === '1' ? q : base; // sell base -> you receive quote
  $('o-recv-chain').textContent = buying ? `(${assets[buying]?.symbol} on ${assets[buying]?.chain})` : '';
  const bondRow = $('o-bond').parentElement; bondRow.style.opacity = base === 'BLRD' && $('o-side').value === '1' ? .4 : 1;
}
$('o-side').onchange = updateRecvHint;
$('o-settle').onchange = () => { const dep = $('o-settle').value === '1'; $('o-recv').parentElement.classList.toggle('hidden', !dep); $('o-bond').parentElement.classList.toggle('hidden', dep); $('o-claim-bond').parentElement.classList.toggle('hidden', !dep); if (dep) $('o-side').value = '1'; };
$('issue-form').onsubmit = async (ev) => {
  ev.preventDefault(); $('i-error').textContent = '';
  try {
    const asset = $('i-asset').value; const quantity = toAtoms($('i-qty').value, asset);
    if ($('i-kind').value === 'mint') await send(api, ensureKey(), TxType.Mint, { asset, quantity, bond: toAtoms($('i-bond').value || '0', 'BLRD') });
    else await send(api, ensureKey(), TxType.Burn, { asset, quantity });
    $('i-qty').value = ''; renderAccount(); loadPro();
  } catch (e) { $('i-error').textContent = e.message; }
};
$('order-form').onsubmit = async (ev) => {
  ev.preventDefault(); $('o-error').textContent = '';
  try {
    const market = currentMarket(); const [base, q] = market.split('/');
    const body = {
      market, side: Number($('o-side').value),
      quantity: toAtoms($('o-qty').value, base),
      price: priceFromHuman($('o-price').value, base, q),
      bond: toAtoms($('o-bond').value || '0', 'BLRD'),
      claim_bond: toAtoms($('o-claim-bond').value || '0', 'BLRD'),
      fill_policy: Number($('o-fill').value),
      receive_address: $('o-recv').value.trim(),
      expiry_height: (status?.bitcoin?.height || 0) + Number($('o-expiry').value || 144),
      settle: Number($('o-settle').value),
    };
    const out = await send(api, ensureKey(), TxType.PlaceOrder, body);
    $('o-total').textContent = `placed ${short(out.hash)}`;
    $('o-qty').value = ''; $('o-price').value = '';
    loadPro();
  } catch (e) { $('o-error').textContent = e.message; }
};
for (const id of ['o-qty', 'o-price']) $(id).addEventListener('input', () => {
  try { const [base, q] = currentMarket().split('/'); const qa = quoteAmount(toAtoms($('o-qty').value, base), priceFromHuman($('o-price').value, base, q)); $('o-total').textContent = `= ${fromAtoms(qa, q)}`; } catch { $('o-total').textContent = ''; }
});

function legText(txid, included) { return txid ? `<span class="mono">${short(txid, 12)}</span> ${included ? 'in block' : 'reported'}` : '<span class="muted">waiting</span>'; }
async function loadMine() {
  const pk = me();
  if (!pk) { $('my-orders').innerHTML = $('my-matches').innerHTML = '<tr><td colspan="8" class="muted">no key yet</td></tr>'; return; }
  const os = await j(`/orders?account=${pk}&limit=50`).catch(() => []);
  $('my-orders').replaceChildren(...os.map((o) => {
    const tr = document.createElement('tr'); const [base, q] = o.market.split('/');
    tr.innerHTML = `<td class="mono">${short(o.id)}</td><td>${o.market}</td><td>${o.side ? 'sell' : 'buy'}</td><td class="num">${fromAtoms(o.qty_left, base)}</td><td class="num">${fmt(priceToHuman(o.price, base, q))}</td><td class="num">${fromAtoms(o.bond_left, 'BLRD')}</td><td>${o.status}</td><td></td>`;
    if (o.status === 'open') { const b = document.createElement('button'); b.className = 'small'; b.textContent = 'Cancel'; b.onclick = async () => { try { await send(api, null, TxType.CancelOrder, { order_id: hex.dec(o.id) }); loadPro(); } catch (e) { alert(e.message); } }; tr.lastElementChild.appendChild(b); }
    return tr;
  }));
  if (!os.length) $('my-orders').innerHTML = '<tr><td colspan="8" class="muted">none</td></tr>';
  const ms = await j(`/matches?account=${pk}&limit=50`).catch(() => []);
  $('my-matches').replaceChildren(...ms.map((m) => {
    const tr = document.createElement('tr'); const role = m.taker === pk ? 'taker' : 'maker'; const base = m.market.split('/')[0];
    tr.innerHTML = `<td class="mono">${short(m.id)}</td><td>${m.market}</td><td>${role}</td><td class="num">${fromAtoms(m.quantity, base)}</td><td>${legText(m.taker_txid, m.taker_included_height)}</td><td>${m.maker_chain === 'blrd' ? 'ledger' : legText(m.maker_txid, m.maker_included_height)}</td><td title="${matchStatusText(m, role)}">${m.status}</td><td></td>`;
    const needTaker = role === 'taker' && m.status === 'open' && !m.taker_txid;
    const needMaker = role === 'maker' && m.status === 'open' && m.taker_txid && !m.maker_txid && m.maker_chain !== 'blrd';
    if (needTaker || needMaker) {
      const cell = tr.lastElementChild; const inp = document.createElement('input'); inp.placeholder = needTaker ? `txid: pay ${fromAtoms(m.taker_amount, m.taker_asset)} to ${short(m.taker_pay_to, 10)}` : `txid: pay ${fromAtoms(m.maker_amount, m.maker_asset)} to ${short(m.maker_pay_to, 10)}`; inp.style.width = '260px'; inp.className = 'mono';
      const b = document.createElement('button'); b.className = 'small'; b.textContent = 'Report';
      b.onclick = async () => { try { await send(api, null, TxType.ReportPayment, { match_id: hex.dec(m.id), leg: needTaker ? 0 : 1, txid: inp.value.trim() }); loadPro(); } catch (e) { alert(e.message); } };
      cell.append(inp, ' ', b);
    }
    return tr;
  }));
  if (!ms.length) $('my-matches').innerHTML = '<tr><td colspan="8" class="muted">none</td></tr>';
}
async function loadMatches() {
  const ms = await j('/matches?status=open&limit=50');
  $('matches').replaceChildren(...ms.map((m) => {
    const tr = document.createElement('tr'); const [base, q] = m.market.split('/');
    tr.innerHTML = `<td class="mono">${short(m.id)}</td><td>${m.market}</td><td class="num">${fromAtoms(m.quantity, base)}</td><td class="num">${fmt(priceToHuman(m.price, base, q))}</td><td>${legText(m.taker_txid, m.taker_included_height)}</td><td>${m.maker_chain === 'blrd' ? 'ledger' : legText(m.maker_txid, m.maker_included_height)}</td><td>${new Date(m.claimed_ts).toLocaleString()}</td>`;
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
  if (API) { try { const r = await fetch(API + '/status', { signal: AbortSignal.timeout(4000) }); if (!r.ok) api = ''; } catch { api = ''; } }
  await loadMarkets();
  await loadStatus();
  renderAccount();
  showSwap();
  loadHistory();
  scheduleQuote();
  let mode = 'simple'; try { mode = localStorage.getItem(MODE) || 'simple'; } catch {}
  setMode(mode);
  const es = new EventSource(api + '/events');
  es.onmessage = (m) => {
    try { const ev = JSON.parse(m.data); logLine(ev); if (ev.event === 'Entry') { loadStatus(); return; }
      if (!$('pro').classList.contains('hidden')) loadPro(); else { trackSwap(); loadHistory(); scheduleQuote(); } } catch {}
  };
  setInterval(() => { loadStatus(); if (!$('pro').classList.contains('hidden')) loadPro(); else { trackSwap(); loadHistory(); } }, 20000);
})();
