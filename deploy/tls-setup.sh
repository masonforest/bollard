#!/bin/sh
# Issue Let's Encrypt certificates through nginx and point the TLS blocks at
# them. Usage: sh tls-setup.sh bitcoin.dance sepolia.bitcoin.dance
#        sh tls-setup.sh bollard.exchange      (once its DNS points here)
set -eu
EMAIL=mason@kissr.co
NAMES=""; for n in "$@"; do NAMES="$NAMES -d $n"; done
certbot certonly --nginx --non-interactive --agree-tos -m "$EMAIL" --keep-until-expiring --expand $NAMES

tls_block() { # name cert-dir upstream-port-or-webroot
if [ "${3#/}" != "$3" ]; then body="root $3; index index.html; location / { try_files \$uri \$uri/ @api; } location @api { proxy_pass http://127.0.0.1:8080; }"; else body="location / { proxy_pass http://127.0.0.1:$3; }"; fi
cat <<TLS
server {
    listen 443 ssl;
    listen [::]:443 ssl;
    http2 on;
    server_name $1;
    ssl_certificate /etc/letsencrypt/live/$2/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/$2/privkey.pem;
    include /etc/letsencrypt/options-ssl-nginx.conf;
    $body
}
TLS
}
: > /etc/nginx/http.d/tls.conf
[ -d /etc/letsencrypt/live/bitcoin.dance ] && { tls_block "bitcoin.dance www.bitcoin.dance" bitcoin.dance 3000; tls_block sepolia.bitcoin.dance bitcoin.dance 3100; } >> /etc/nginx/http.d/tls.conf
[ -d /etc/letsencrypt/live/bollard.exchange ] && tls_block "bollard.exchange www.bollard.exchange" bollard.exchange /var/www/bollard.exchange >> /etc/nginx/http.d/tls.conf
grep -q mainnet.bollard.exchange /etc/letsencrypt/live/bollard.exchange/fullchain.pem 2>/dev/null; openssl x509 -in /etc/letsencrypt/live/bollard.exchange/fullchain.pem -noout -ext subjectAltName 2>/dev/null | grep -q mainnet.bollard.exchange && tls_block mainnet.bollard.exchange bollard.exchange 8080 >> /etc/nginx/http.d/tls.conf

# HTTP -> HTTPS for names that have certificates. ACME challenges still pass:
# certbot's nginx plugin inserts its own location ahead of the redirect.
[ -d /etc/letsencrypt/live/bitcoin.dance ] && sed -i 's#location / { proxy_pass http://127.0.0.1:3000; }#return 301 https://$host$request_uri;#; s#location / { proxy_pass http://127.0.0.1:3100; }#return 301 https://$host$request_uri;#' /etc/nginx/http.d/sites.conf
[ -d /etc/letsencrypt/live/bollard.exchange ] && sed -i 's#location / { try_files $uri $uri/ @api; }#return 301 https://$host$request_uri;#' /etc/nginx/http.d/sites.conf
openssl x509 -in /etc/letsencrypt/live/bollard.exchange/fullchain.pem -noout -ext subjectAltName 2>/dev/null | grep -q mainnet.bollard.exchange && sed -i 's#location / { proxy_pass http://127.0.0.1:8080; }#location / { if ($host = mainnet.bollard.exchange) { return 301 https://$host$request_uri; } proxy_pass http://127.0.0.1:8080; }#' /etc/nginx/http.d/sites.conf

nginx -t && nginx -s reload
grep -q 'certbot renew' /etc/crontabs/root || echo '17 3 * * * certbot renew --quiet --deploy-hook "nginx -s reload"' >> /etc/crontabs/root
echo "--- certificates ---"; certbot certificates 2>/dev/null | grep -E "Certificate Name|Domains|Expiry"
