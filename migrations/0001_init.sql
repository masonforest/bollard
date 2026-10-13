-- The log is the only consensus data. Everything else is rebuilt from it.
create table log (
  seq     bigint primary key,
  hash    bytea  not null unique,
  prev    bytea  not null,
  height  integer not null,
  ts      bigint not null,
  kind    text   not null,
  leader  bytea  not null,
  bytes   bytea  not null
);

-- Bitcoin bookkeeping: blocks seen, lottery entries, and the epoch each block decided.
create table btc_blocks (
  height    integer primary key,
  hash      text not null unique,
  prev_hash text not null,
  ts        bigint not null
);
create table entries (
  txid     text primary key,
  height   integer not null,
  key_hash bytea not null,
  ip       text not null,
  port     integer not null,
  tip      bytea not null,
  fee_sats bigint not null,
  rank     integer,
  valid    boolean
);
create index entries_height on entries(height);
create table epochs (
  height          integer primary key,
  leader_key_hash bytea not null,
  leader_key      bytea,
  leader_url      text,
  entry_txid      text,
  checkpoint_tip  bytea not null,
  first_seq       bigint,
  last_seq        bigint
);

-- State, derived from the log.
create table accounts (
  pubkey  bytea primary key,
  nonce   bigint not null default 0,
  balance bigint not null default 0,
  locked  bigint not null default 0
);
create table orders (
  id            bytea primary key,
  seq           bigint not null,
  account       bytea not null,
  market        text not null,
  base          text not null,
  quote         text not null,
  side          smallint not null,
  quantity      bigint not null,
  qty_left      bigint not null,
  price         bigint not null,
  bond          bigint not null,
  bond_left     bigint not null,
  claim_bond    bigint not null,
  fill_policy   smallint not null,
  recv_addr     text not null,
  expiry_height integer not null,
  status        text not null
);
create index orders_book on orders(market, status, side, price, seq);
create index orders_account on orders(account);
create table matches (
  id            bytea primary key,
  seq           bigint not null,
  order_id      bytea not null,
  maker         bytea not null,
  taker         bytea not null,
  market        text not null,
  side          smallint not null,
  quantity      bigint not null,
  price         bigint not null,
  bond          bigint not null,
  claim_bond    bigint not null,
  fill_policy   smallint not null,
  taker_chain   text not null,
  taker_asset   text not null,
  taker_amount  bigint not null,
  taker_pay_to  text not null,
  maker_chain   text not null,
  maker_asset   text not null,
  maker_amount  bigint not null,
  maker_pay_to  text not null,
  status        text not null,
  claimed_ts    bigint not null,
  claimed_height integer not null,
  taker_txid    text,
  taker_block_hash text,
  taker_block_height bigint,
  taker_included_height integer,
  maker_txid    text,
  maker_payer   bytea,
  maker_block_hash text,
  maker_block_height bigint,
  maker_included_height integer,
  closed_seq    bigint,
  closed_reason text
);
create index matches_status on matches(status);
create index matches_parties on matches(maker, taker);

create table meta (key text primary key, value text not null);
