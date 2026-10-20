-- Where a maker's issued coins live on chain, and which address a delivery must come from.
alter table issuers add column source text not null default '';
alter table matches add column maker_source text not null default '';
