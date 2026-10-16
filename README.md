# Bollard

A cross-chain order book with real settlement, finalized on Bitcoin.

- Coins come in by being burned: every account has a keyless burn address per chain (`1BoLLARD…` on Bitcoin, `0xb0110a4d…` on Ethereum), and what lands there is minted on Bollard one for one. No custodian, no bridge, no maker promise: the supply here is exactly what was burned there.
- Holders leave by selling their balance to someone coming in, who pays them on chain. Everything else is instant and free on the ledger.
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
bollard burn-address --account <pubkey hex>     # where to send coins to bring them in
# once bitcoin and USDC have landed, rest orders against dollars
bollard tx --node https://mainnet.bollard.exchange place-order --market btc:BTC/eth:USDC --side sell \
  --settle 0 --quantity 100000000 --price 9000000000000 --expiry-height 980000
```

The website is for holders only: create a passkey, burn in, send, buy and sell, withdraw. Making happens here, in the CLI, because a maker must act while nobody has a browser open.

## Layout

- `schema/bollard.fbs` — the wire format (FlatBuffers). Signed bytes are canonical.
- `src/apply.rs` — the state transition; the only code that changes state. `src/ledger.rs` holds balances.
- `src/chain/burn.rs` — keyless burn addresses; `src/burn_watch.rs` — the leader's watch on them.
- `src/lottery.rs`, `src/bitcoin_watch.rs` — entries, the draw, epochs.
- `src/node.rs` — leader sequencing, follower replay, settlement checks.
- `src/chain/` — Bitcoin and EVM adapters.
- `src/api.rs` — HTTP API and server-sent event streams.
- `web/wallet` — the passkey wallet; `web/dashboard` — the Pro view (book, charts, issuing).
- `deploy/` — nginx, OpenRC and the deploy script for the Alpine host.
