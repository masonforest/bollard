-- One stake per maker backs everything it issues. Cover per asset is gone.
alter table accounts add column staked bigint not null default 0;
alter table accounts add column unstaking bigint not null default 0;
alter table accounts add column unstake_height integer not null default 0;
alter table issuers drop column cover;
