#!/bin/sh
# One-time host setup for the Bollard node on Alpine. Safe to re-run.
set -eu

id bollard >/dev/null 2>&1 || adduser -D -H -s /sbin/nologin bollard
mkdir -p /var/lib/bollard /etc/bollard
chown bollard:bollard /var/lib/bollard
chmod 750 /etc/bollard

if [ ! -f /var/lib/postgresql/17/data/PG_VERSION ]; then rc-service postgresql setup; fi
rc-update add postgresql default >/dev/null 2>&1 || true
rc-service postgresql status >/dev/null 2>&1 || rc-service postgresql start
sleep 2

if [ ! -f /etc/bollard/env ]; then
  PW=$(head -c 24 /dev/urandom | base64 | tr -d '/+=')
  cat > /tmp/bollard-role.sql <<SQL
do \$\$ begin
  if not exists (select 1 from pg_roles where rolname = 'bollard') then
    create role bollard login password '$PW';
  end if;
end \$\$;
SQL
  su postgres -c "psql -q -f /tmp/bollard-role.sql"
  rm -f /tmp/bollard-role.sql
  su postgres -c "psql -tAc \"select 1 from pg_database where datname = 'bollard'\"" | grep -q 1 || su postgres -c "createdb -O bollard bollard"

  KEYS=$(/usr/local/bin/bollard keygen)
  SECRET=$(printf '%s' "$KEYS" | sed -n 's/.*"secret": "\([0-9a-f]*\)".*/\1/p')
  PUB=$(printf '%s' "$KEYS" | sed -n 's/.*"pubkey": "\([0-9a-f]*\)".*/\1/p')
  HEIGHT=$(curl -s https://bitcoin.publicnode.com -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"getblockcount","params":[]}' | sed -n 's/.*"result":\([0-9]*\).*/\1/p')
  cat > /etc/bollard/env <<ENV
DATABASE_URL=postgres://bollard:$PW@127.0.0.1/bollard
BOLLARD_KEY=$SECRET
LISTEN=127.0.0.1:8080
PUBLIC_IP=45.77.101.95
PUBLIC_PORT=8080
BITCOIN_RPC=https://bitcoin.publicnode.com
ETH_RPC=https://ethereum-rpc.publicnode.com
GENESIS_HEIGHT=$HEIGHT
GENESIS_LEADER=$PUB
RUST_LOG=info,sqlx=warn
ENV
  chown root:bollard /etc/bollard/env
  chmod 640 /etc/bollard/env
  printf '%s\n' "$KEYS" | grep -E '"(pubkey|key_hash|bitcoin_address)"'
  echo "genesis height: $HEIGHT"
else
  echo "env exists:"; grep -E '^(GENESIS_HEIGHT|GENESIS_LEADER|PUBLIC_IP)=' /etc/bollard/env
fi

rc-update add bollard default >/dev/null 2>&1 || true
rc-service bollard restart
sleep 5
tail -n 15 /var/log/bollard.log
echo "--- status ---"
curl -s http://127.0.0.1:8080/status
echo
