-- A deposit order may mint on claim against the maker's stake instead of
-- escrowing options minted in advance.
alter table orders add column mint boolean not null default false;
