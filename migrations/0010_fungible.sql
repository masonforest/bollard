-- Options of one asset are fungible: balances no longer remember their issuer.
-- Every lot collapses onto the empty issuer; issuance stays tracked per maker.
create table balances_merged as select account, asset, ''::bytea as issuer, sum(amount)::bigint as amount from balances group by account, asset;
drop table balances;
alter table balances_merged rename to balances;
alter table balances add primary key (account, asset, issuer);
