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
  renderTokenButtons();
  buildPicker();
}

// ------------------------------------------------------------------ coins and the token picker
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
  const list = Object.values(assets).filter((a) => a.id !== 'eth:USDC' && (pickerChain === 'all' || a.chain === pickerChain) && (!q || a.symbol.toLowerCase().includes(q) || (ASSET_NAMES[a.id] || '').toLowerCase().includes(q) || a.id.toLowerCase().includes(q)));
  $('p-popular').replaceChildren(...list.slice(0, 4).map((a) => { const b = document.createElement('button'); b.innerHTML = `${coinIcon(a)}${a.symbol}`; b.onclick = () => choose(a.id); return b; }));
  $('p-list').replaceChildren(...list.map((a) => {
    const b = document.createElement('button'); b.className = 'token-row' + (a.id === current ? ' on' : '');
    b.innerHTML = `${coinIcon(a, true)}<span><div class="name">${ASSET_NAMES[a.id] || a.symbol}</div><div class="sym">${a.symbol}</div></span><span class="right">${CHAIN_NAMES[a.chain] || a.chain}${a.contract ? ' · ' + a.contract.slice(0, 6) + '…' + a.contract.slice(-4) : ''}</span>`;
    b.onclick = () => choose(a.id); return b;
  }));
  if (!list.length) $('p-list').innerHTML = '<div class="picker-label">Nothing matches.</div>';
}
function currentMarket() { return `${$('m-base-asset').value}/eth:USDC`; }
// ------------------------------------------------------------------ simple: buy or sell for dollars
let side = 'buy'; let simpleTimer = null; let plan = null; // plan: { fills: [{order, qty}], quote, short } or { deposit: order, qty }
let payWith = 'usdc'; let depositWatch = null;
$('s-pay').onclick = (e) => { const b = e.target.closest('button'); if (!b) return; $('s-pay').dataset.touched = '1'; payWith = b.dataset.pay; for (const x of $('s-pay').children) x.classList.toggle('on', x === b); computeSimple(); };
const USDC = 'eth:USDC';
function setSide(v) { side = v; $('s-buy').classList.toggle('on', v === 'buy'); $('s-sell').classList.toggle('on', v === 'sell'); scheduleSimple(); }
$('s-buy').onclick = () => setSide('buy'); $('s-sell').onclick = () => setSide('sell');
$('s-amount').addEventListener('input', scheduleSimple);
$('s-asset').addEventListener('input', scheduleSimple);
function scheduleSimple() { clearTimeout(simpleTimer); simpleTimer = setTimeout(computeSimple, 200); }
async function computeSimple() {
  plan = null; $('s-error').textContent = '';
  const asset = $('s-asset').value; const a = assets[asset]; if (!a) return;
  $('s-go').textContent = `${side === 'buy' ? 'Buy' : 'Sell'} ${a.symbol}`;
  let qty; try { qty = toAtoms($('s-amount').value || '0', asset); } catch (e) { $('s-quote').textContent = e.message; $('s-go').disabled = true; return; }
  if (qty <= 0n) { $('s-quote').textContent = 'Enter an amount.'; $('s-go').disabled = true; return; }
  const b = await j(`/book?market=${encodeURIComponent(asset + '/' + USDC)}`);
  // A buy can also be paid with the coin itself on its chain: a deposit order from a maker.
  const dep = side === 'buy' && a.chain !== 'blrd' ? (await j(`/book?market=${encodeURIComponent(asset + '/' + asset)}`).catch(() => ({}))).deposits?.[0] : null;
  $('s-pay-row').classList.toggle('hidden', !dep);
  if (dep) {
    $('s-pay').lastElementChild.textContent = `${a.symbol} on ${CHAIN_NAMES[a.chain] || a.chain}`;
    const usdcShort = myBalance(USDC) !== null && myBalance(USDC) === 0n;
    if (usdcShort && payWith === 'usdc' && !$('s-pay').dataset.touched) { payWith = 'chain'; for (const x of $('s-pay').children) x.classList.toggle('on', x.dataset.pay === 'chain'); }
  }
  if (dep && payWith === 'chain') {
    const take = qty < BigInt(dep.qty_left) ? qty : BigInt(dep.qty_left);
    const pay = quoteAmount(take, dep.price);
    plan = { deposit: dep, qty: take, pay };
    $('s-quote').innerHTML = `You pay <b>${fromAtoms(pay, asset)}</b> on ${CHAIN_NAMES[a.chain] || a.chain}` + (take < qty ? ` for ${fromAtoms(take, asset)}; only that much is offered` : '') + `; you get an address to pay next.`;
    $('s-go').disabled = !me(); return;
  }
  // Buying takes asks, cheapest first; selling takes bids, highest first.
  const orders = side === 'buy' ? [...(b.asks || [])].sort((x, y) => x.price - y.price) : [...(b.bids || [])].sort((x, y) => y.price - x.price);
  let left = qty, quote = 0n; const fills = [];
  for (const o of orders) { if (left <= 0n) break; const take = left < BigInt(o.qty_left) ? left : BigInt(o.qty_left); fills.push({ order: o, qty: take }); quote += quoteAmount(take, o.price); left -= take; }
  if (!fills.length) { $('s-quote').textContent = `Nobody is ${side === 'buy' ? 'selling' : 'buying'} ${a.symbol} right now.`; $('s-go').disabled = true; return; }
  plan = { fills, quote, short: left };
  const have = side === 'buy' ? quote : qty; const haveAsset = side === 'buy' ? USDC : asset;
  const mine = myBalance(haveAsset);
  const avg = fills.length ? Number(quote) / Number(qty - left) * 1e8 : 0;
  $('s-quote').innerHTML = `${side === 'buy' ? 'You pay' : 'You get'} <b>${fromAtoms(quote, USDC)}</b>` + (left > 0n ? ` for ${fromAtoms(qty - left, asset)}; only that much is on the book` : '') + ` · ${fmt(priceToHuman(BigInt(Math.round(avg)), asset, USDC), 6)} USDC/${a.symbol}` + (mine !== null && mine < have ? ` · you have ${fromAtoms(mine, haveAsset)}` : '');
  $('s-go').disabled = !me() || (mine !== null && mine < have);
}
let balances = null;
function myBalance(asset) { if (!balances) return null; return asset === 'BLRD' ? BigInt(balances.blrd || 0) : BigInt(balances.assets?.find((x) => x.asset === asset)?.amount || 0); }
$('s-go').onclick = async () => {
  if (!plan) return; $('s-error').textContent = ''; $('s-result').textContent = ''; $('s-go').disabled = true;
  const asset = $('s-asset').value; const a = assets[asset];
  try {
    ensureKey();
    if (plan.deposit) {
      const out = await send(api, null, TxType.Claim, { order_id: hex.dec(plan.deposit.id), quantity: plan.qty, receive_address: '' });
      const ms = await j(`/matches?account=${me()}&kind=deposit&limit=5`); const m = ms.find((x) => x.id === out.hash);
      if (!m) throw new Error('claimed, but the match did not show up');
      showDeposit(m); $('s-amount').value = ''; plan = null; $('s-quote').textContent = 'Enter an amount.';
      return;
    }
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
function showDeposit(m) {
  $('s-deposit').classList.remove('hidden');
  $('d-amount').textContent = fromAtoms(m.taker_amount, m.taker_asset); $('d-addr').textContent = m.taker_pay_to;
  try { const q = qrcode(0, 'M'); q.addData(assets[m.taker_asset]?.chain === 'btc' ? `bitcoin:${m.taker_pay_to}?amount=${fromAtoms(m.taker_amount, m.taker_asset, false)}` : m.taker_pay_to); q.make(); $('d-qr').innerHTML = q.createSvgTag({ cellSize: 3, margin: 2 }); } catch {}
  $('d-copy').onclick = async () => { try { await navigator.clipboard.writeText(m.taker_pay_to); } catch {} $('d-copy').textContent = 'Copied'; setTimeout(() => ($('d-copy').textContent = 'Copy address'), 1500); };
  clearInterval(depositWatch);
  depositWatch = setInterval(async () => {
    try {
      const r = await j(`/find-payment?match=${m.id}&leg=0`);
      if (r.txid && !r.reported) await send(api, null, TxType.ReportPayment, { match_id: hex.dec(m.id), leg: 0, txid: r.txid });
      const cur = (await j(`/matches?account=${me()}&kind=deposit&limit=10`)).find((x) => x.id === m.id);
      if (cur?.taker_txid) $('d-status').textContent = `Payment ${short(cur.taker_txid, 12)} seen; waiting for a block.`;
      if (cur && cur.status !== 'open') { clearInterval(depositWatch); $('s-deposit').classList.add('hidden'); $('s-result').textContent = cur.status === 'closed' ? `Bought ${fromAtoms(cur.maker_amount, cur.maker_asset)}; it is in your balance.` : `Deposit ${cur.status}.`; renderAccount(); loadSimple(); }
    } catch {}
  }, 8000);
}
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
