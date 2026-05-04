#!/bin/bash
# Set up the ergo-proxy dashboard on a Debian/Ubuntu host.
# Installs caddy, certbot, configures fail2ban for web scanners,
# and sets up the graph generation cron job.
#
# Usage: sudo ./setup.sh <domain>
# Example: sudo ./setup.sh ergo-relay.blockhost.io

set -euo pipefail

DOMAIN="${1:?Usage: setup.sh <domain>}"
WEBROOT="/var/www/ergo-proxy"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "Setting up ergo-proxy dashboard for ${DOMAIN}..."

# --- Caddy ---
if ! command -v caddy >/dev/null 2>&1; then
    echo "Installing caddy..."
    apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl
    curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
    curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | tee /etc/apt/sources.list.d/caddy-stable.list
    apt-get update
    apt-get install -y caddy
fi

# Caddy config: serve static files with HTTPS
cat > /etc/caddy/Caddyfile << EOF
${DOMAIN} {
    root * ${WEBROOT}
    file_server
    log {
        output file /var/log/caddy/access.log
    }
}
EOF

# --- Web root ---
mkdir -p "$WEBROOT"
cp "${SCRIPT_DIR}/index.html" "$WEBROOT/"
cp "${SCRIPT_DIR}/generate-graphs.sh" /usr/local/bin/ergo-proxy-graphs
chmod +x /usr/local/bin/ergo-proxy-graphs

# Generate initial (empty) graphs
/usr/local/bin/ergo-proxy-graphs "$WEBROOT" 2>/dev/null || true

# --- Cron: regenerate graphs every 30 seconds ---
# systemd timer is cleaner than cron for sub-minute intervals
cat > /etc/systemd/system/ergo-proxy-graphs.service << EOF
[Unit]
Description=Generate ergo-proxy latency graphs

[Service]
Type=oneshot
ExecStart=/usr/local/bin/ergo-proxy-graphs ${WEBROOT}
User=root
EOF

cat > /etc/systemd/system/ergo-proxy-graphs.timer << EOF
[Unit]
Description=Regenerate ergo-proxy graphs every 30s

[Timer]
OnBootSec=30s
OnUnitActiveSec=30s
AccuracySec=1s

[Install]
WantedBy=timers.target
EOF

systemctl daemon-reload
systemctl enable --now ergo-proxy-graphs.timer

# --- Firewall: allow HTTPS ---
if command -v ufw >/dev/null 2>&1; then
    ufw allow 80/tcp
    ufw allow 443/tcp
    echo "Firewall: ports 80 and 443 opened"
fi

# --- fail2ban: web scanner / bot detection ---
cat > /etc/fail2ban/filter.d/ergo-web-scanners.conf << 'EOF'
# Ban bots and vulnerability scanners on first attempt.
# Matches requests for paths that no legitimate client would ever request.
[Definition]
failregex = ^.*"[A-Z]+ /(\.env|wp-login|wp-admin|phpMyAdmin|phpmyadmin|admin|xmlrpc\.php|\.git|cgi-bin|shell|eval-stdin|actuator|boaform|solr|vendor|telescope|debug|console|manager|jenkins|\.aws|config\.json|\.DS_Store|wp-content|wp-includes|administrator|magento|struts|jmx-console|invoker|\.well-known/security\.txt).*" .*$
          ^.*"[A-Z]+ /.*\.(php|asp|aspx|jsp|cgi|bak|sql|tar\.gz|zip|rar).*" .*$

ignoreregex =
EOF

cat > /etc/fail2ban/jail.d/ergo-web-scanners.conf << 'EOF'
[ergo-web-scanners]
enabled = true
filter = ergo-web-scanners
logpath = /var/log/caddy/access.log
# Ban on first match — nothing legitimate triggers these patterns
maxretry = 1
findtime = 86400
bantime = 86400
EOF

# Reload fail2ban
mkdir -p /var/log/caddy
systemctl restart caddy
systemctl restart fail2ban

echo ""
echo "Dashboard ready at https://${DOMAIN}"
echo "  Graphs: ${WEBROOT}/*.png (regenerated every 30s)"
echo "  fail2ban: web scanners banned after 1 hit (24h ban)"
echo ""
echo "Caddy will auto-provision a Let's Encrypt certificate."
