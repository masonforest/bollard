-- Shares of an exercise that other issuers owe the deliverer; paid once the
-- holder has been paid and the delivery is deep.
create table exercise_shares (
  match_id bytea not null,
  issuer   bytea not null,
  quantity bigint not null,
  value    bigint not null,
  primary key (match_id, issuer)
);
