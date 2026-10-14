#!/bin/sh
# Build for the host (Alpine, x86_64 musl) and install the binary + web page.
#   ./deploy/deploy.sh            build, upload, restart
#   ./deploy/deploy.sh --no-build upload what is already built
set -eu
HOST=${HOST:-root@45.77.101.95}
TARGET=x86_64-unknown-linux-musl
cd "$(dirname "$0")/.."

if [ "${1:-}" != "--no-build" ]; then
  cargo zigbuild --release --target "$TARGET"
fi

scp "target/$TARGET/release/bollard" "$HOST:/usr/local/bin/bollard.new"
scp deploy/bollard.initd "$HOST:/etc/init.d/bollard"
ssh "$HOST" mkdir -p /var/www/bollard.exchange
scp -r web/. "$HOST:/var/www/bollard.exchange/"
ssh "$HOST" 'set -e
  chmod +x /etc/init.d/bollard /usr/local/bin/bollard.new
  mv /usr/local/bin/bollard.new /usr/local/bin/bollard
  rc-service bollard restart
  sleep 2
  tail -n 5 /var/log/bollard.log'
