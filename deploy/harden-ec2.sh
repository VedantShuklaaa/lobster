#!/usr/bin/env bash
# One-time host hardening. Safe to run again.
#
#   sudo /opt/lobster/current/harden-ec2.sh
#
# - security updates install themselves (no automatic reboot)
# - SSH: keys only, no root login (matters only if port 22 is ever opened again)
# - fail2ban bans addresses that keep failing SSH logins
set -euo pipefail
[ "$(id -u)" = 0 ] || { echo "run with sudo"; exit 1; }

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y unattended-upgrades fail2ban

cat > /etc/apt/apt.conf.d/20auto-upgrades <<'CONF'
APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
APT::Periodic::AutocleanInterval "7";
CONF
cat > /etc/apt/apt.conf.d/52lobster-upgrades <<'CONF'
Unattended-Upgrade::Automatic-Reboot "false";
Unattended-Upgrade::Remove-Unused-Dependencies "true";
CONF

# sshd uses the first value it sees, and drop-ins are read in name order, so 10- beats cloud-init's 50-
install -d /etc/ssh/sshd_config.d
cat > /etc/ssh/sshd_config.d/10-hardening.conf <<'CONF'
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin no
CONF
sshd -t
systemctl reload ssh 2>/dev/null || systemctl reload sshd

cat > /etc/fail2ban/jail.d/lobster.local <<'CONF'
[sshd]
enabled  = true
backend  = systemd
maxretry = 4
findtime = 10m
bantime  = 1h
CONF
systemctl enable fail2ban
systemctl restart fail2ban

echo "--- ssh"
sshd -T | grep -E '^(passwordauthentication|permitrootlogin|kbdinteractiveauthentication) '
echo "--- fail2ban"
fail2ban-client status sshd || true
echo "--- unattended-upgrades"
systemctl is-enabled unattended-upgrades
