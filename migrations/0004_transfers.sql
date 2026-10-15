-- Sends between accounts, for activity lists. Written by apply, derived from the log.
create table transfers (
  seq      bigint primary key,
  ts       bigint not null,
  sender   bytea not null,
  receiver bytea not null,
  asset    text not null,
  amount   bigint not null,
  delivered_asset text not null,
  delivered bigint not null
);
create index transfers_sender on transfers(sender);
create index transfers_receiver on transfers(receiver);
