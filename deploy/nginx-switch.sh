#!/bin/sh
# Move bitcoin.dance from the bun server's own :80/:443 to nginx, with the
# existing certificate so HTTPS never drops, then let certbot take over.
set -eu
cp /etc/init.d/stable-mainnet /root/stable-mainnet.initd.bak-$(date +%Y%m%d%H%M)
cp /root/stable-mainnet.initd /etc/init.d/stable-mainnet
chmod +x /etc/init.d/stable-mainnet
rm -f /etc/nginx/http.d/default.conf
cp /root/sites.conf /etc/nginx/http.d/sites.conf
# Serve HTTPS from the first second with the certificate the bun server got.
cat > /etc/nginx/http.d/tls.conf <<'TLS'
server {
    listen 443 ssl;
    listen [::]:443 ssl;
    http2 on;
    server_name bitcoin.dance www.bitcoin.dance;
    ssl_certificate /var/lib/stable-tls/cert.pem;
    ssl_certificate_key /var/lib/stable-tls/key.pem;
    location / { proxy_pass http://127.0.0.1:3000; }
}
server {
    listen 443 ssl;
    listen [::]:443 ssl;
    http2 on;
    server_name sepolia.bitcoin.dance;
    ssl_certificate /var/lib/stable-tls/cert.pem;
    ssl_certificate_key /var/lib/stable-tls/key.pem;
    location / { proxy_pass http://127.0.0.1:3100; }
}
TLS
nginx -t
rc-service stable-mainnet restart
sleep 3
rc-update add nginx default >/dev/null 2>&1 || true
rc-service nginx restart
sleep 2
echo "--- listeners ---"; netstat -tlnp | grep -E ':(80|443|3000|3100|8080) '
echo "--- checks ---"
curl -s -o /dev/null -w "http  bitcoin.dance  %{http_code} -> %{redirect_url}\n" -H 'Host: bitcoin.dance' http://127.0.0.1/
curl -sk -o /dev/null -w "https bitcoin.dance  %{http_code} %{size_download} bytes\n" --resolve bitcoin.dance:443:127.0.0.1 https://bitcoin.dance/
curl -sk -o /dev/null -w "https sepolia        %{http_code} %{size_download} bytes\n" --resolve sepolia.bitcoin.dance:443:127.0.0.1 https://sepolia.bitcoin.dance/
curl -s -o /dev/null -w "http  bollard (default) %{http_code}\n" http://127.0.0.1/status
tail -n 3 /var/log/stable-mainnet.log
