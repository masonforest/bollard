-- Assets come onto Bollard by burning them on their chain to the account's
-- keyless burn address. One row per (chain transaction, asset).
create table burns (
  txid    text not null,
  asset   text not null,
  account bytea not null,
  amount  bigint not null,
  block_hash   text not null,
  block_height bigint not null,
  seq     bigint not null,
  ts      bigint not null,
  primary key (txid, asset)
);
create index burns_account on burns(account);
drop table issuers;
alter table accounts drop column staked;
alter table accounts drop column unstaking;
alter table accounts drop column unstake_height;
