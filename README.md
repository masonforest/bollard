# Bollard

A cross-chain order book with real settlement, finalized on Bitcoin.

- Makers rest orders backed by a BLRD bond; takers claim them and pay first, on the maker's chain. Takers never need BLRD.
- The maker pays the taker once the taker's payment is in a block. Anyone may deliver the maker's leg and take the bond.
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

## Trade

```sh
bollard keygen
bollard tx --node https://mainnet.bollard.exchange place-order --market btc:BTC/eth:ETH --side sell \
  --quantity 100000 --price 1500000000 --bond 100000000 --receive-address 0x... --expiry-height 980000
bollard tx claim --order <id> --quantity 100000 --receive-address bc1q...
# pay the maker on their chain, then:
bollard tx report --match <id> --leg taker --txid <txid>
```

Amounts are in atoms: sats, gwei, 10^-6 USDC, rill (10^-8 BLRD). Prices are quote atoms per 100,000,000 base atoms.

## Layout

- `schema/bollard.fbs` — the wire format (FlatBuffers). Signed bytes are canonical.
- `src/apply.rs` — the state transition; the only code that changes state.
- `src/lottery.rs`, `src/bitcoin_watch.rs` — entries, the draw, epochs.
- `src/node.rs` — leader sequencing, follower replay, settlement checks.
- `src/chain/` — Bitcoin and EVM adapters.
- `src/api.rs` — HTTP API and server-sent event streams.
- `deploy/` — nginx, OpenRC and the deploy script for the Alpine host.
