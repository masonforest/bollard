-- Claims per order, used to derive a fresh receive address for each claim
-- when the maker gave an xpub instead of a single address.
alter table orders add column claims integer not null default 0;
