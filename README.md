# Bollard

A cross-chain order book with real settlement, finalized on Bitcoin.

- What trades are options: a maker's promise to deliver a coin on its chain, backed by BLRD cover the maker locks. Holders send, receive and trade options instantly and for free, and exercise them to take the coin out. Holders never need BLRD.
- Coins move on chain only for deposits (buy options with the coin) and exercises (the issuer delivers, or loses its cover to the holder). Anyone may deliver in the issuer's place and take the cover.
- Every account picks a currency. A send to someone who uses another currency converts on the book in the same entry. Every market is quoted in USDC.
- Accounts are passkeys (P-256) or secp256k1 keys.
- One leader at a time sequences everything in milliseconds. Bitcoin picks the leader: a fee-weighted lottery run through OP_RETURN entries, which also commit to the log and finalize it.
- One binary, one Postgres database, one Bitcoin RPC. Chain adapters for the rest.

The design doc has the rules in full; this repository is the node.

## Run a node

```sh
cargo build --release
bollard keygen                      # node key = Bollard account = Bitcoin address
createdb bollard
DATABASE_URL=postgres://localhost/bollard BOLLARD_KEY=<secret> \
GENESIS_HEIGHT=<height> GENESIS_LEADER=<pubkey> GENESIS_LEADER_URL=http://bollard.exchange \
bollard run
```

A node follows the leader by default. To stand for leader, send a little bitcoin to the node's address and enter the lottery:

```sh
PUBLIC_IP=<your ip> bollard enter --fee 2000
```

The fee is the entry's weight. The block that includes it runs the draw; if you win, your node starts sequencing when that block arrives.

## Make a market

```sh
bollard keygen
# issue 1 BTC of options, locking 10 BLRD as cover
bollard tx --node https://mainnet.bollard.exchange mint --asset btc:BTC --quantity 100000000 --bond 1000000000
# sell them for on-chain bitcoin (a deposit order); every claim gets its own address from the xpub
bollard tx place-order --market btc:BTC/btc:BTC --side sell --settle 1 --quantity 100000000 --price 100000000 \
  --receive-address xpub... --expiry-height 980000
# or rest them on the ledger against dollars
bollard tx place-order --market btc:BTC/eth:USDC --side sell --settle 0 --quantity 100000000 --price 9000000000000 --expiry-height 980000
```

People use the wallet at https://bollard.exchange instead: create a passkey, deposit, send, exercise to withdraw.

Amounts are in atoms: sats, gwei, 10^-6 USDC, rill (10^-8 BLRD). Prices are quote atoms per 100,000,000 base atoms.

## Layout

- `schema/bollard.fbs` — the wire format (FlatBuffers). Signed bytes are canonical.
- `src/apply.rs` — the state transition; the only code that changes state. `src/ledger.rs` holds the option balances.
- `src/lottery.rs`, `src/bitcoin_watch.rs` — entries, the draw, epochs.
- `src/node.rs` — leader sequencing, follower replay, settlement checks.
- `src/chain/` — Bitcoin and EVM adapters.
- `src/api.rs` — HTTP API and server-sent event streams.
- `web/wallet` — the passkey wallet; `web/dashboard` — the Pro view (book, charts, issuing).
- `deploy/` — nginx, OpenRC and the deploy script for the Alpine host.
