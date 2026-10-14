# Bollard: Protocol Design v0.1

October 2026

## Summary

Bollard is a cross-chain exchange with a real order book, settled by direct payments between traders, and finalized on Bitcoin. Users post bids and asks on the network. A taker claims an order and pays the maker first, on the maker's chain; the maker then pays the taker, and a bond in the network token (BLRD) posted by the maker backs that second payment. A maker that fills on sight makes the swap take seconds; finality and the bond release catch up in the background. Nobody custodies anything: no vaults, no wrapped coins, no threshold signatures.

One leader at a time sequences transactions in realtime. That leader is chosen by a fee-weighted lottery, run entirely through Bitcoin OP\_RETURN data, in every Bitcoin block that has entries. A Bitcoin miner can enter at no cost, because the entry fee is paid to whoever mines the block. This is by design. The leader's log is finalized when the next winning lottery entry commits to it on Bitcoin.

## Actors and components

There are five kinds of participant. Only the leader has any power, and only for one Bitcoin block at a time.

| Actor | Does | Gets |
| --- | --- | --- |
| Maker | Rests orders backed by a BLRD bond; pays the taker once the taker's payment is in a block | The spread; the bond back once its own payment is deep |
| Taker | Claims an order and pays the maker first, on the maker's chain; needs no BLRD | The asset bought, or the maker's bond if the maker fails to deliver |
| Node (follower) | Replays the leader's log, watches external chains, serves reads | Nothing directly; runs one to verify and to be eligible to lead |
| Leader | The node whose lottery entry won the current Bitcoin block; sequences every transaction in realtime, asserts external confirmations, mints its reward | The block's BLRD issuance plus all fees in that epoch |
| Bitcoin miner | Mines blocks as usual; may include its own lottery entry and pay the entry fee to itself | Leadership of the block it mines, at no cost |
| External chains | The chains the supported assets live on carry the actual payments; the network only watches them | Ordinary transaction fees |

Components inside one node: a Bitcoin node (blocks, entries, broadcasting entries), the log (append-only, hash-chained, leader-signed), the state (accounts, orders, matches, legs), one chain adapter per supported chain, and two interfaces: one for users, and one that streams the log to followers.

Bitcoin picks the leader and finalizes its log, traders pay each other on the external chains, and the nodes sequence and watch without ever holding coins.

## The Bitcoin lottery

One Bitcoin transaction does three jobs: it enters the lottery, it announces where the entrant's node lives, and it commits to a checkpoint of the log. Bollard never writes anything else to Bitcoin.

### Entry transaction

Any Bitcoin transaction with exactly one OP\_RETURN output carrying this 75-byte payload:

| Field | Bytes | Content |
| --- | --- | --- |
| magic | 4 | `BLRD` |
| version | 1 | `0x01` |
| key hash | 20 | HASH160 of the entrant's secp256k1 signing key (the key itself is revealed in the node's first signed log entry) |
| ip | 16 | IPv6, or IPv4 mapped (`::ffff:a.b.c.d`) |
| port | 2 | big-endian |
| tip | 32 | hash of the newest log entry the entrant has seen; all zeros at genesis |

75 bytes sits under the 80-byte limit that older Bitcoin Core versions relay by default, so no node operator has to change settings.

### Weight

Weight = the entry transaction's fee in satoshis (inputs minus outputs). No burn address and no special output: the fee simply goes to the miner, so Bitcoin miners are paid by the lottery without having to know Bollard exists. A miner who includes its own entry pays the fee to itself, so for a miner entering is free and winning is guaranteed by setting the fee above every other entry in the block. That is the intended way miners opt in.

Consequence: non-miners only ever win blocks whose miner did not enter, with probability fee / total fees of entries in that block. If every pool opts in, leadership follows hash rate exactly.

### The draw for block N

1. Eligible entries = valid entries in block N. An entry is used once; it does not carry over.
2. seed = SHA256d(block hash of N, then the string `bollard/v1`).
3. Rank the entries by drawing them one at a time, each draw weighted by fee, with no entry drawn twice. Draw number i (starting at 0) goes like this: compute r = SHA256(seed, i) modulo the total fee of the entries not yet drawn. Then go through the undrawn entries in txid order, adding up their fees as you go; the first entry at which the running total exceeds r is drawn and takes rank i. Repeat until no entries are left. An entry's chance of being drawn first is its fee divided by the block's total entry fees, and every node gets the same ranking because the seed, the fees and the txid order all come from the block.
4. Leader for block N = the first entry in rank order whose tip commitment is valid: the committed entry exists and descends from the checkpoint of block N−1.
5. If block N has no entries, or none that are valid, nothing changes: the leader of block N−1 keeps leading through block N, the checkpoint stays where it was, and no reward is minted for block N. After genesis there is always exactly one leader; a block without a lottery is just a block the leader keeps working through. If the leader fails during such a stretch, failover uses the rank order from the most recent block that had entries, then earlier leaders.

Every node computes the same ranking from Bitcoin data alone. The ranking also gives the fallback order if the winner is unreachable.

### Self-election

The miner of block N chooses which entries it includes and can always win. Grinding the block hash to favor a particular non-self entry is pointless: the miner can just enter itself. So the only thing a miner gains is one epoch of leadership per block it mines, which is exactly what the design offers.

### Checkpoint commitment

The winning entry's tip becomes the checkpoint for block N. A later entry whose tip does not descend from the current checkpoint is invalid and skipped (rule 4), so checkpoints only move forward. The winner's own sequencing continues from the newest log entry it holds, which is at or after its committed tip, so finality trails the live head by a few seconds to a few minutes.

## Realtime sequencing

The leader is a sequencer: it takes signed transactions over the network, orders them, applies them, and streams the result. Nothing waits for a block.

### Leader duties

1. Accept transactions at the IP and port from its entry.
2. Validate each against current state: signature, nonce, balances, order rules.
3. Append a log entry and apply it to the state as one atomic step, then stream the entry to followers and return a signed receipt to the sender. Target: under 50 ms end to end.
4. Watch the external chains and append confirmation entries as payments confirm.

### Log entry

| Field | Content |
| --- | --- |
| seq | integer, starts at 1, increases by 1 |
| prev | hash of the previous entry |
| height | Bitcoin height of the leader's epoch |
| time | leader's clock in ms; never decreases; informational only |
| kind | one of the kinds below |
| payload | the user transaction with its signature, or the leader's assertion |
| leader | the leader's key |
| sig | leader's signature over the fields above |
| hash | SHA256 of the entry; becomes the next entry's prev |

### Kinds

- User-signed: Register, Transfer, PlaceOrder, CancelOrder, Claim, ReportPayment.
- Leader-asserted: EpochStart (height, entry txid; mints the reward), ClaimExpired (claim), LegIncluded (leg, txid, block hash, height), ClaimVoid (claim), LegMissed (leg), MatchClosed (releases or slashes the bond), Handoff (reason, replaced leader).

Rule: every leader-asserted entry must be checkable by any node from Bitcoin, the external chains and prior state. A follower that finds one false treats the log as invalid from that entry on.

### Handoff at a new block

A block with no valid winner causes no handoff; the current leader simply continues. When block N+1 arrives with a valid winner, the winner starts from the newest entry it holds; its first entry is EpochStart with prev = that entry. The old leader stops when it sees the block or the new leader's first entry. Anything the old leader signed after that point is orphaned and gets resubmitted; nonces make resubmission harmless.

### Failover inside an epoch

If followers cannot reach the leader for 30 s, they treat it as down. They move to the next entry in that block's rank order, and if the ranking is exhausted, to the previous epoch's leader. The replacement's first entry is Handoff, built on the newest entry it holds. Two leaders may briefly coexist; the next block's winning commitment decides which log survives.

## Finality

Two levels, and users pick the one they trust for a given amount, the same way they pick confirmations on Bitcoin.

- Soft: a signed receipt from the current leader. It holds unless the leader fails, equivocates or is replaced before the next block. Fine for placing and cancelling orders and for small transfers.
- Hard: an entry is final once it is an ancestor of the tip committed by a winning entry on Bitcoin. Reverting it means reorging Bitcoin past that block. Wait 1 block for ordinary trades (the settlement legs take longer anyway) and 6 for large BLRD transfers.

### Fork choice

- The canonical log is the chain of winning commitments on Bitcoin, followed by the current leader's live entries.
- Validity beats anchoring. An invalid entry invalidates everything after it, and an entry that commits to an invalid tip is skipped in the draw. A leader cannot make a false confirmation final by getting it committed.
- Availability: an entrant whose committed tip a node cannot fetch (from the entrant's own IP, then from other followers) is skipped by that node. Followers gossip log segments, so this converges, and any split heals at the next block.

### Bitcoin reorgs

Bollard follows Bitcoin's chain exactly. If Bitcoin reorgs, the epochs, leaders and checkpoints of the replaced blocks are recomputed, and live entries from a replaced leader are orphaned and resubmitted. There is no built-in confirmation lag; depth is the user's choice, as on Bitcoin.

## Order book and matching

The book lives in the log, and every node rebuilds it from there. Makers rest bonded orders; takers claim them. A claim is matched on entry against the best resting orders at or better than its price, and each assignment is a match.

### Assets and markets

- An asset is identified by its chain and its id on that chain. BLRD is the one internal asset.
- Supported assets are those the node software has an adapter for. Which assets ship first is an implementation decision, not part of this design; adding one is a software release, not a vote.
- A market is any pair of supported assets, priced as quote per base.

### Resting order (maker)

| Field | Content |
| --- | --- |
| market, side | a pair of supported assets, and buy or sell |
| quantity | in base units; partial claims allowed |
| price | limit, quote per base |
| bond | BLRD per unit of base, locked from the maker's account at placement |
| claim bond | BLRD a claimant must lock to claim; default 0 |
| fill policy | when the maker delivers: on seeing the taker's payment, on its inclusion (the protocol's own trigger), or on its finality; shown in the book |
| receive address | the maker's address on the chain of the asset being bought; fresh per order |
| expiry | a Bitcoin height; the order cancels itself there |

### Claim (taker)

| Field | Content |
| --- | --- |
| target | one order id, or a market, side and limit price |
| quantity | up to the remainder of the orders it hits |
| receive address | the taker's address on the chain of the asset being bought |

### Rules

1. Price-time priority among resting orders: best price first, then lowest seq.
2. Only makers bond. A resting order locks bond-per-unit times quantity from the maker's account at placement. A claim locks nothing unless the order asks for a claim bond.
3. A claim is matched on entry against resting orders at or better than its price, up to its quantity. Each assignment is a match with its own legs. An unmatched remainder is dropped, never rested; to rest, post a bonded order.
4. A claimed order is held for the claimant for 3 minutes of leader time. If no payment txid is posted by then, it reopens for anyone.
5. Cancel removes the unclaimed remainder at once. Claimed quantity cannot be cancelled.
6. Markets with BLRD as the base: the maker's BLRD is escrowed at placement and released to the taker by the ledger when the taker's payment is in a block. No maker action, no separate bond.
7. Dust: each adapter sets a minimum quantity for its asset.

A match does not move coins. It records who owes what to whom and holds the maker's bond; settlement is the next section.

## Settlement and bonds

The taker pays first, the maker pays second, and only the maker holds a bond. A taker who never pays loses nothing but the claim. A maker who never pays loses the bond to the taker.

### Steps for a match

1. Claim. The taker claims one order, or the best orders at a price up to a quantity. The claim is sequenced in milliseconds and the book shows the order as claimed. The taker has 3 minutes of leader time to post a payment txid, or the claim expires and the order reopens for anyone.
2. Taker pays. The taker sends the quote asset to the maker's receive address and posts ReportPayment(match, txid).
3. Taker's leg is included. Every node watches the chain for the payment to land in a block. The leader appends LegIncluded(leg, txid, block hash, height). That is the protocol's trigger. If the payment is not in a block within the taker deadline, counted in Bitcoin blocks from the claim, the claim is void and the order reopens. If a reorg later removes it, the match is void and the order reopens; a maker that had already delivered takes that loss.
4. Maker pays. The maker sends the base asset to the taker's receive address as soon as the taker's payment reaches the order's fill policy (seen, included or final), and posts ReportPayment. The maker deadline runs from the taker's LegIncluded. Filling on sight is the maker's own risk, priced into its spread.
5. Anyone may pay the maker's leg. The bond is released to whoever's payment satisfied it. If the maker is slow or gone, a third party can deliver and take the bond. That is the repo.
6. Maker's leg is included; the taker has the coins. The bond is released only once that payment is deep (the depth each chain's adapter sets, at or beyond that chain's own finality), so a maker cannot profit from a reorg of its own payment. The wait costs the maker nothing but time.
7. MatchClosed. The bond, minus the match fee, is released to the payer of the maker's leg.

### Outcomes

| What happened | Result |
| --- | --- |
| Taker never posts a txid | Claim expires after 3 minutes; order reopens; any claim bond goes to the maker |
| Taker's payment not in a block by the taker deadline, or reorged out afterwards | Claim void; order reopens; any claim bond goes to the maker; a maker that already delivered takes the loss |
| Maker's payment not deep by the maker deadline plus 2 blocks of grace | Maker's bond goes to the taker in full; order cancelled |
| Maker's payment reorged out before it is deep | Counts as not delivered; same as the row above |
| Maker's payment deep | Bond released, minus the fee, to whoever paid the maker's leg; match closed |

Partial claims prorate the bond.

### Speed

A swap feels done when the taker sees the asset in their wallet, and that happens as soon as the maker fills. A maker whose policy is seen fills within seconds of the taker's payment reaching the mempool; finality and the bond release run in the background and nobody waits for them. The maker fronts its own asset and carries its own risk, which is what makes the speed possible.

| Direction | Taker sees the asset | Confirmed |
| --- | --- | --- |
| From a 12-second chain into Bitcoin, maker fills on sight | BTC in the mempool 5 to 15 s after the claim | 1 Bitcoin block, about 10 min |
| From Bitcoin into a 12-second chain, maker fills on sight | the asset in the next block, about 15 s after the claim | that chain's next block; final per that chain |
| Between 2-second chains, maker fills on sight | about 5 s | seconds |
| Any pair, maker fills on finality | 13 to 30 min | same |

The book shows each maker's median fill time, computed from the log as the gap between the taker's ReportPayment and the maker's. Fast makers earn the flow. Nothing enforces the promise except that number, and nothing needs to: the maker has posted the bond, the taker has already paid, and the deadline is the backstop.

### Why this flow

1. The taker pays first, so takers never need BLRD; only makers bond. A claim before paying means two takers cannot pay the same maker for one order: the second claim waits or goes to the next order. The maker pays once the taker's payment is in a block, and carries only the small reorg risk of recent blocks.
2. A slashed bond goes to the counterparty in full: the taker already paid, and nothing is burned. If a third party delivered the maker's leg, the bond goes to them instead, which is what makes the repo worth doing.

### Bond sizing

There is no price oracle, so the protocol cannot judge whether a bond covers an order. The maker sets the bond in BLRD per unit; the interface shows its value from the network's own BLRD markets, and takers see it before they claim. A bond equal to the order's value means a defaulting maker loses exactly what it owed. A premium, say 110%, removes the maker's free option when BLRD falls against the base asset, and makes it worth a third party's while to deliver in the maker's place. Either is the maker's choice in v1.

## Token economics

BLRD has three jobs: back trades (bonds), pay for sequencing (fees to the leader), and reward leaders (issuance). It is never needed to hold or move the external assets.

### Issuance

Proposal: each block's leader mints R = Bitcoin's block subsidy at that height, denominated in BLRD. That is 3.125 BLRD per block today, halving when Bitcoin halves, reaching zero around 2140. The curve is already understood by everyone, so nobody has to argue about it. It is minted in EpochStart; a block with no valid entry mints nothing.

Alternatives: a flat 1 BLRD per block (no halvings to explain), or no issuance at all with a genesis allocation (less boring, more arguing).

### Fees

Flat, in BLRD, paid to the leader of the epoch the entry lands in. Flat fees need no oracle.

| Action | Fee |
| --- | --- |
| Register, PlaceOrder, CancelOrder, Claim, ReportPayment | 0 |
| Match | 0.001 BLRD, paid by the maker out of the bond at MatchClosed |
| Transfer | 0.0001 BLRD |

Placing orders and claiming are free, so the book stays deep and takers never need BLRD. The maker pays a little per match; that is the price of sequencing. Claim spam is handled by a maker-set claim bond or a per-account rate limit, not by a fee. Fees are fixed per software version.

### Burns

Nothing burns in v1. Every slashed bond goes to the party who was owed.

### How a newcomer gets BLRD

Takers never need BLRD, so most people never buy it. A maker buys it on a BLRD market as a taker: claim, pay, and the seller's escrowed BLRD is released when the payment is in a block. Leaders receive all new BLRD, so they are the natural first sellers. No faucet, no airdrop, no presale.

## Nodes

A node follows Bitcoin, replays the log, keeps the state, and watches the chains it trades on. Everything specific to a chain lives in that chain's adapter.

### Modes

- follower (default): syncs the log from the leader, verifies everything, serves reads, forwards writes to the leader.
- leader: switches on by itself when the node's entry wins. Same program.
- enter: builds and broadcasts an entry transaction carrying the node's current tip.

### Determinism

- No wall clock inside the state transition. The only time is Bitcoin height; the leader's `time` field is never used for a decision.
- Integers in base units everywhere; no floats.
- Payloads have one canonical encoding before hashing and signing.

## Security analysis

The worst a leader can do is waste one Bitcoin block. It cannot take coins, because it never holds any, and it cannot make a lie final, because every node checks every assertion.

| Threat | Worst case | Mitigation |
| --- | --- | --- |
| Leader equivocates (two logs) | Soft receipts in that epoch are unreliable | Next block's winning commitment picks one log; users wait for hard finality on anything that matters; a leader bond is a v2 option |
| Leader censors transactions | Delay of one epoch | Deadlines and the 2-block grace are measured in Bitcoin blocks, so two consecutive censoring leaders are needed to cause a wrongful default |
| Leader asserts a false LegIncluded | A bond could be released or slashed wrongly | Every follower verifies against its own chain node; the log is invalid from that entry on, and later winners that build on it are skipped |
| Leader withholds the log it committed to | Nodes cannot verify the checkpoint | Entrant is skipped; followers gossip log segments; the next block resolves any split |
| Miner elects itself | One epoch of leadership per block mined | Intended; that is the opt-in path for miners |
| Sybil entries | None | Weight is the fee; splitting a fee across entries changes nothing |
| Bitcoin reorg | Replaced blocks' epochs are recomputed | Users wait the depth they want, as on Bitcoin |
| Maker takes the payment and never delivers | Taker is out the payment | Maker's bond goes to the taker in full; anyone may deliver in the maker's place and take the bond |
| Thin bonds | A maker defaults when the price moves against it | Makers set bonds and takers see them before claiming; a premium bond makes third-party delivery worthwhile; a protocol minimum from the internal BLRD markets later |
| Claim and never pay (griefing) | A maker's order is locked for 3 minutes | Claim expires and the order reopens; a maker-set claim bond goes to the maker; per-account rate limit |
| Maker fills on sight (policy seen) | The taker replaces the unconfirmed payment and keeps both sides | The maker's choice, priced into its spread; cap by size; on Bitcoin, only for payments that do not signal replace-by-fee |
| Unreachable or DoS'd leader | Stall of up to 30 s | Failover to the next ranked entry, then the previous leader; the leader's IP is public by design, so run it like any public server |
| Taker's payment reorged out after inclusion | A maker that already delivered is out the fill | Rare, and not cheaply caused; the maker prices it into its spread. A cover mechanism can socialize it; open question |

## Initial parameters

Starting values for v1; all fixed per software version.

| Parameter | Value | Why |
| --- | --- | --- |
| Epoch | 1 Bitcoin block | Bitcoin is the only clock |
| Entry payload | 75 bytes, magic `BLRD`, version 1 | Under the 80-byte OP\_RETURN default of older Bitcoin Core versions |
| Entry weight | fee in satoshis | Pays miners, no burn address |
| Leader timeout | 30 s unreachable | Failover to the next ranked entry |
| Claim window | 3 minutes of leader time | Enough to broadcast a payment; a dead claim costs the maker little |
| Deep (bond release) | per chain, set by its adapter, at or beyond the chain's own finality (3 confirmations on Bitcoin) | Gates only the bond release; nobody else waits for it |
| Taker deadline | per chain, set by its adapter; counted in Bitcoin blocks from the claim | Payment in a block by then, or the claim is void |
| Maker deadline | per chain, set by its adapter; counted in Bitcoin blocks from the taker's LegIncluded | Then the bond goes to the taker |
| Grace G | 2 blocks | One censoring leader cannot cause a default |
| Issuance | Bitcoin subsidy at that height, in BLRD (3.125 now) | Known curve, no debate |
| Match fee | 0.001 BLRD, paid by the maker | Sequencing has a price; takers pay nothing |
| Transfer fee | 0.0001 BLRD | Spam transfers have a cost |
| Dust minimums | per asset, set by its adapter | Below the fees to move them |
| Base unit | 1 BLRD = 10^8 rill | Same precision as Bitcoin |
| Suggested finality | 1 block for trades, 6 for large BLRD transfers | Users choose, as on Bitcoin |
