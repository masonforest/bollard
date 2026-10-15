# Bollard: Protocol Design v0.2

October 2026

## Summary

Bollard is a cross-chain exchange with a real order book, finalized on Bitcoin. What trades on it are options: a maker's bonded promise to deliver a coin on its own chain. A maker issues options on coins it holds, locking BLRD, the network token, as cover. Holders move options between accounts instantly and for free, trade them against each other on the book, and exercise them when they want the real coin, at which point the issuer must deliver on chain or lose its cover to the holder. Nobody custodies anything: no vaults, no wrapped coins, no threshold signatures. A balance on Bollard is a bundle of promises, each one backed by a named maker and a visible bond.

Every account picks a currency. Send someone dollars and they receive dollars; send them dollars when they hold bitcoin and the network converts on the way at the best price on the book, in the same transaction. Accounts are passkeys, so there is nothing to back up and nothing to paste.

One leader at a time sequences transactions in realtime. That leader is chosen by a fee-weighted lottery, run entirely through Bitcoin OP\_RETURN data, in every Bitcoin block that has entries. A Bitcoin miner can enter at no cost, because the entry fee is paid to whoever mines the block. This is by design. The leader's log is finalized when the next winning lottery entry commits to it on Bitcoin.

## Actors and components

There are five kinds of participant. Only the leader has any power, and only for one Bitcoin block at a time.

| Actor | Does | Gets |
| --- | --- | --- |
| Maker (issuer) | Issues options on coins it holds, locking BLRD cover; sells them for on-chain payment (a deposit) or on the book; delivers the coin when a holder exercises | The spread; the cover back as options are burned or delivered on |
| Holder (user) | Keeps a balance of options in a chosen currency; sends, receives and trades them instantly; exercises to take the coin out; needs no BLRD | The coin on exercise, or the issuer's cover if the issuer fails to deliver |
| Node (follower) | Replays the leader's log, watches external chains, serves reads | Nothing directly; runs one to verify and to be eligible to lead |
| Leader | The node whose lottery entry won the current Bitcoin block; sequences every transaction in realtime, asserts external confirmations, mints its reward | The block's BLRD issuance plus all fees in that epoch |
| Bitcoin miner | Mines blocks as usual; may include its own lottery entry and pay the entry fee to itself | Leadership of the block it mines, at no cost |
| External chains | The chains the supported assets live on carry the actual payments; the network only watches them | Ordinary transaction fees |

Components inside one node: a Bitcoin node (blocks, entries, broadcasting entries), the log (append-only, hash-chained, leader-signed), the state (accounts, balances, issuers, orders, matches), one chain adapter per supported chain, and two interfaces: one for users, and one that streams the log to followers.

Bitcoin picks the leader and finalizes its log, coins move on the external chains only when options are bought with them or exercised, and the nodes sequence and watch without ever holding coins.

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
5. If block N has no entries, or none that are valid, the leader stays the same: the leader of block N−1 keeps leading through block N and mints block N's reward as well, and the checkpoint stays where it was. After genesis there is always exactly one leader; a block without a lottery is just a block the leader keeps working through. If the leader fails during such a stretch, failover uses the rank order from the most recent block that had entries, then earlier leaders.

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

- User-signed: Transfer, Mint, Burn, Send, SetCurrency, PlaceOrder, CancelOrder, Claim, ReportPayment, Exercise.
- Leader-asserted: EpochStart (height, entry txid; mints the reward), ClaimExpired (claim), LegIncluded (leg, txid, block hash, height), ClaimVoid (claim), LegMissed (leg), MatchClosed (releases or slashes the cover), Handoff (reason, replaced leader).

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

## Accounts

An account is a public key. Two kinds are accepted: a secp256k1 key, which is also a Bitcoin address and is what nodes use, and a P-256 key held by a passkey, which is what people use. A passkey signs a transaction with an ordinary WebAuthn assertion whose challenge is the hash of the transaction; the node checks the assertion and the challenge. The key never leaves the device's secure hardware, it syncs through the platform's passkey store, and there is no secret to write down. Registration is not a transaction: an account exists the first time it is paid.

Each account has a currency: the asset it holds its balance in and wants to be paid in. The default is dollars. SetCurrency changes it.

## Options

A balance on Bollard is not a coin. It is a quantity of options on a coin, each issued by a maker who holds the real coin off the ledger and has locked BLRD as cover. The ledger tracks, for every account, how much of each asset it holds and from which issuer. Options from different issuers on the same asset are fungible for sending and trading; they differ only in who stands behind them, and the interface shows that.

### Issuing

| Transaction | Effect |
| --- | --- |
| Mint(asset, quantity, cover) | Creates `quantity` options on `asset` in the issuer's own balance and locks `cover` BLRD from the issuer's account. An issuer's cover for an asset is pooled: every option it has issued on that asset is backed by the pool, pro rata. |
| Burn(asset, quantity) | Destroys options the issuer holds and releases the matching share of its cover. |

There is no price oracle, so the protocol cannot judge whether cover is enough. The cover ratio (BLRD locked per unit issued) is public, the interface prices it from the network's own BLRD markets, and holders see it before they accept an issuer's options. An issuer with thin cover finds its options discounted on the book or refused. That is the whole enforcement, and it is the same enforcement a bank faces.

### Moving

Send(to, asset, amount) moves options from one account to another in one log entry, sequenced in milliseconds. No chain is touched and no fee is charged. If the recipient's currency differs from the asset sent, the same entry converts: the network takes the best resting orders on the book that turn the sent asset into the recipient's currency, fills them atomically, and credits the recipient with the result. If the book cannot convert the whole amount, the send fails and nothing moves. The sender always pays what they typed; the recipient always receives their own currency.

### Exercising

Exercise(asset, quantity, receive address) burns options the holder has and opens a delivery match against their issuer, oldest issuer first. The issuer must pay the coin to the holder's address on its chain by the maker deadline. The holder's share of the issuer's cover is the bond on that match, and it follows the bond rules in Settlement below: released to whoever delivers, paid to the holder if nobody does. Exercise is the only time a holder waits for a chain, and the only time a maker is forced to move a coin.

## Order book and matching

The book lives in the log, and every node rebuilds it from there. Makers rest orders; takers claim them. An order says how it settles:

- On the ledger. Both sides are options already on Bollard. The maker's side is escrowed at placement, the taker's side is debited at claim, and the fill is one atomic log entry. This is how options trade against each other, and how a send converts currencies.
- On chain (a deposit). The maker sells options for the real coin. The taker claims, pays the maker on the coin's chain, and the escrowed options are released to the taker when the payment is in a block. This is how coins come in.

### Assets and markets

- An asset is identified by its chain and its id on that chain. BLRD is the one internal asset.
- Supported assets are those the node software has an adapter for. Adding one is a software release, not a vote.
- A market is any pair of supported assets, priced as quote per base. A deposit market has the same asset on both sides: options on the base, paid for with the coin itself.

### Resting order (maker)

| Field | Content |
| --- | --- |
| market, side | a pair of supported assets, and buy or sell |
| settle | on the ledger, or on chain |
| quantity | in base units; partial claims allowed |
| price | limit, quote per base |
| claim bond | BLRD a claimant must lock to claim an on-chain order; default 0 |
| receive address | on-chain orders only: where the maker is paid: a plain address, or an xpub, from which every claim gets its own address (child 0/i), so the maker's wallet can tell payments apart without a memo |
| expiry | a Bitcoin height; the order cancels itself there |

A sell order escrows the base (options, or BLRD) at placement; a ledger buy order escrows the quote. Escrow is what backs a resting order, so there is no separate bond.

### Claim (taker)

| Field | Content |
| --- | --- |
| target | one order id |
| quantity | up to the remainder of the order |

### Rules

1. Price-time priority among resting orders: best price first, then lowest seq.
2. A ledger claim fills at once: the taker's quote (or base) is debited, the maker's escrow is released to the taker, and the match is recorded closed in the same entry. Nothing waits.
3. An on-chain claim holds the order for the claimant for 3 minutes of leader time. If no payment txid is posted by then, it reopens for anyone.
4. Cancel removes the unclaimed remainder at once and returns its escrow. Claimed quantity cannot be cancelled.
5. Dust: each adapter sets a minimum quantity for its asset.
6. One payment, one match. A transaction id can satisfy only one leg on the whole ledger, and the per-claim addresses above are what make that safe in practice.

## Settlement and cover

Coins move on chain in exactly two cases: a deposit, where a taker pays a maker for options, and an exercise, where a maker pays a holder for options burned. In a deposit the taker pays first and risks nothing but the claim. In an exercise the maker pays and its cover is the bond.

### Deposit

1. Claim. The taker claims an on-chain order. The claim is sequenced in milliseconds and the options are held in escrow for the taker. The taker has 3 minutes of leader time to post a payment txid, or the claim expires and the order reopens.
2. Taker pays. The taker sends the coin to the address derived for this claim and posts ReportPayment(match, txid). Wallets find the payment on the chain themselves; nobody has to paste anything.
3. Included. Every node watches the chain for the payment to land in a block. The leader appends LegIncluded(leg, txid, block hash, height), the escrowed options go to the taker, and the match closes. If the payment is not in a block by the taker deadline, counted in Bitcoin blocks from the claim, the claim is void and the order reopens. If a reorg later removes it, the options are already spent; the maker's own chain policy is what protects it, and a deposit order can ask for a claim bond.

### Exercise

1. Exercise. The holder's options are burned and a delivery match opens against the issuer, with the holder's pro-rata share of the issuer's cover as the bond.
2. Maker pays. The issuer sends the coin to the holder's address and posts ReportPayment. The maker deadline is counted in Bitcoin blocks from the exercise.
3. Anyone may pay. The bond is released to whoever's payment satisfied the leg. If the issuer is slow or gone, a third party can deliver and take the bond. That is the repo.
4. Included and deep. The bond is released only once the payment is deep (the depth each chain's adapter sets, at or beyond that chain's own finality), so an issuer cannot profit from a reorg of its own payment.
5. MatchClosed. The bond, minus the match fee, is released to the payer.

### Outcomes

| What happened | Result |
| --- | --- |
| Deposit: taker never posts a txid | Claim expires after 3 minutes; order reopens; any claim bond goes to the maker |
| Deposit: payment not in a block by the taker deadline | Claim void; order reopens; any claim bond goes to the maker |
| Deposit: payment in a block | Options released to the taker; match closed |
| Exercise: delivery not deep by the maker deadline plus 2 blocks of grace | The bond goes to the holder in full |
| Exercise: delivery reorged out before it is deep | Counts as not delivered; same as the row above |
| Exercise: delivery deep | Bond released, minus the fee, to whoever delivered; match closed |

### Speed

Everything that stays on the ledger is done in the time it takes the leader to sign: sends, conversions, trades between options. A deposit takes one block on the coin's chain. An exercise takes however long the issuer takes to pay plus the chain's own finality, and the deadline is the backstop. Most people never exercise: they hold dollars as dollars, pay each other, and let the makers keep the coins.

### Cover sizing

Cover is set by the issuer, in BLRD, pooled across everything it has issued on an asset. Cover equal to the value issued means a defaulting issuer loses exactly what it owed. A premium removes the issuer's free option when BLRD falls against the coin, and makes it worth a third party's while to deliver in the issuer's place. Either is the issuer's choice in v1; the ratio is public and the market prices it.

## Token economics

BLRD has three jobs: back options (cover), pay for sequencing (fees to the leader), and reward leaders (issuance). It is never needed to hold, move or trade the external assets.

### Issuance

Each block's leader mints R = Bitcoin's block subsidy at that height, denominated in BLRD. That is 3.125 BLRD per block today, halving when Bitcoin halves, reaching zero around 2140. The curve is already understood by everyone, so nobody has to argue about it. It is minted in an EpochStart entry, one per Bitcoin block, whether or not that block had a lottery: a leader who carries over through quiet blocks earns their rewards too.

### Fees

Flat, in BLRD, paid to the leader of the epoch the entry lands in. Flat fees need no oracle.

| Action | Fee |
| --- | --- |
| Send, Transfer, SetCurrency, Mint, Burn, PlaceOrder, CancelOrder, Claim, ReportPayment | 0 |
| Exercise | 0.001 BLRD, paid by the issuer out of the cover at MatchClosed |

Using the ledger is free, so the book stays deep and holders never need BLRD. The issuer pays a little per exercise; that is the price of sequencing. Spam is handled by a per-account rate limit, not by a fee. Fees are fixed per software version.

### Burns

Nothing burns in v1. Every slashed bond goes to the party who was owed.

### How a newcomer gets BLRD

Holders never need BLRD, so most people never buy it. A maker buys it on a BLRD market like any other option trade, or by deposit. Leaders receive all new BLRD, so they are the natural first sellers. No faucet, no airdrop, no presale.

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
| Issuer never delivers on exercise | Holder is out the coin | The issuer's cover goes to the holder in full; anyone may deliver in the issuer's place and take the bond |
| Thin cover | An issuer defaults when the price moves against it | Cover ratios are public and priced by the market; a premium makes third-party delivery worthwhile; a protocol minimum from the internal BLRD markets later |
| Issuer over-issues | Cover is spread thinner than holders expected | Cover is pooled and pro rata, so every holder sees the same ratio; options from a thin issuer trade at a discount or are refused |
| Claim a deposit and never pay (griefing) | A maker's order is locked for 3 minutes | Claim expires and the order reopens; a maker-set claim bond goes to the maker; per-account rate limit |
| Deposit payment reorged out after inclusion | The taker keeps options it did not pay for | Deposit makers set their chain's depth and a claim bond; small by construction since options are released only at inclusion |
| Unreachable or DoS'd leader | Stall of up to 30 s | Failover to the next ranked entry, then the previous leader; the leader's IP is public by design, so run it like any public server |
| Lost passkey | The account is unreachable | Passkeys sync through the platform's store; a second passkey on the same account is a v2 option |

## Initial parameters

Starting values for v1; all fixed per software version.

| Parameter | Value | Why |
| --- | --- | --- |
| Epoch | 1 Bitcoin block | Bitcoin is the only clock |
| Entry payload | 75 bytes, magic `BLRD`, version 1 | Under the 80-byte OP\_RETURN default of older Bitcoin Core versions |
| Entry weight | fee in satoshis | Pays miners, no burn address |
| Leader timeout | 30 s unreachable | Failover to the next ranked entry |
| Claim window | 3 minutes of leader time | Enough to broadcast a deposit payment; a dead claim costs the maker little |
| Deep (cover release) | per chain, set by its adapter, at or beyond the chain's own finality (3 confirmations on Bitcoin) | Gates only the cover release on exercise; nobody else waits for it |
| Taker deadline | per chain, set by its adapter; counted in Bitcoin blocks from a deposit claim | Payment in a block by then, or the claim is void |
| Maker deadline | per chain, set by its adapter; counted in Bitcoin blocks from an exercise | Then the cover goes to the holder |
| Grace G | 2 blocks | One censoring leader cannot cause a default |
| Issuance | Bitcoin subsidy at that height, in BLRD (3.125 now) | Known curve, no debate |
| Exercise fee | 0.001 BLRD, paid by the issuer out of cover | Sequencing has a price; holders pay nothing |
| Ledger fees | 0 | Sends, trades and conversions are free |
| Account keys | secp256k1 (33 bytes) or P-256 passkey (65 bytes) | Nodes use the first; people use the second |
| Default currency | USD (USDC on Ethereum) | What most people mean by money |
| Dust minimums | per asset, set by its adapter | Below the fees to move them |
| Base unit | 1 BLRD = 10^8 rill | Same precision as Bitcoin |
| Suggested finality | 1 block for ordinary sends and trades, 6 for large ones | Users choose, as on Bitcoin |
