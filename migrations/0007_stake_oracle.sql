-- Back to maker options backed by BLRD stake, now measured in dollars by the
-- ledger's own BLRD/USDC market.
drop table if exists burns;
create table issuers (
  account bytea not null,
  asset   text  not null,
  issued  bigint not null default 0,
  primary key (account, asset)
);
alter table accounts add column staked bigint not null default 0;
alter table accounts add column unstaking bigint not null default 0;
alter table accounts add column unstake_height integer not null default 0;
