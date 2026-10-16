-- Standing deposit offers: a maker sets a receive address per chain and the
-- node keeps a minting order open for every asset on that chain.
alter table orders add column standing boolean not null default false;
