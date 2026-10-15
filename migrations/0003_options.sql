-- Options: bonded delivery promises on the ledger.
create table balances (
  account bytea not null,
  asset   text  not null,
  issuer  bytea not null,
  amount  bigint not null,
  primary key (account, asset, issuer)
);
create index balances_account on balances(account);
create table issuers (
  account bytea not null,
  asset   text  not null,
  issued  bigint not null default 0,
  cover   bigint not null default 0,
  primary key (account, asset)
);
alter table accounts add column preferred text;
alter table orders add column settle smallint not null default 1;
alter table matches add column kind text not null default 'trade';
-- Passkeys: credential id -> public key, so a device that has never seen an
-- account can sign in. Not consensus data.
create table passkeys (
  cred_id text primary key,
  pubkey  bytea not null,
  created bigint not null
);
-- What a resting order has set aside, and how much of it is left.
alter table orders add column escrow_asset text not null default '';
alter table orders add column escrow_left bigint not null default 0;
