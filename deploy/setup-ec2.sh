#!/usr/bin/env bash
# One-time setup of the live server on the instance. Safe to run again after changing the unit,
# the Caddyfile or the domain.
#
#   sudo /opt/lobster/current/setup-ec2.sh
#
# Needs: a release already deployed by CD, and DNS for api.vedantshukla.in pointing at this machine
# (Caddy cannot get a certificate before that).
set -euo pipefail

[ "$(id -u)" = 0 ] || { echo "run with sudo"; exit 1; }
here=$(dirname "$(readlink -f "$0")")
deploy_user=${DEPLOY_USER:-ubuntu}
domain=api.vedantshukla.in

# 1. service user: no login, no home
id lobster >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin lobster

# 2. Caddy, from the official apt repository
if ! command -v caddy >/dev/null; then
  apt-get update
  apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl gpg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
    | gpg --dearmor --yes -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
    > /etc/apt/sources.list.d/caddy-stable.list
  chmod o+r /usr/share/keyrings/caddy-stable-archive-keyring.gpg /etc/apt/sources.list.d/caddy-stable.list
  apt-get update
  apt-get install -y caddy
fi

# 3. let the deploy user restart and stop the one service, and nothing else
systemctl_bin=$(command -v systemctl)
tmp=$(mktemp)
cat > "$tmp" <<SUDO
$deploy_user ALL=(root) NOPASSWD: $systemctl_bin restart lobster-server, $systemctl_bin stop lobster-server, $systemctl_bin start lobster-server
SUDO
visudo -cf "$tmp"
install -m 440 "$tmp" /etc/sudoers.d/lobster-deploy
rm -f "$tmp"

# 4. systemd unit, started now and at every boot
install -m 644 "$here/lobster-server.service" /etc/systemd/system/lobster-server.service
systemctl daemon-reload
systemctl enable lobster-server
systemctl restart lobster-server

# 5. Caddy, once the name resolves
install -m 644 "$here/Caddyfile" /etc/caddy/Caddyfile
caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
if getent hosts "$domain" >/dev/null; then
  systemctl enable caddy
  systemctl reload-or-restart caddy
else
  echo "DNS for $domain does not resolve yet. Caddy is installed but not started."
  echo "Add the A record, then run this script again."
fi

systemctl --no-pager --lines=0 status lobster-server caddy || true
