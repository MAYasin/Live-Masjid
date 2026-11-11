#!/bin/bash
# Tighten a Live Masjid device. Run once, as root:  sudo /opt/livemasjid/harden.sh [user]
#
#  1. SSH: key sign-in only (password logins off), but only if <user> has an authorized key,
#     so you can't lock yourself out.
#  2. sudo: no blanket "NOPASSWD: ALL" for <user> (Pi OS images ship one). The app keeps its own
#     narrow rules in /etc/sudoers.d/livemasjid. Skipped if <user> has no password, because sudo
#     would then be unusable.
#  3. App files: /opt/livemasjid and the sudo rules are owned by root and writable only by root.
#  4. Firewall (ufw): nothing from outside the home network gets in. From home-network addresses
#     (10.x, 172.16-31.x, 192.168.x, and IPv6 link-local/ULA) it allows SSH, the web page and mDNS
#     (so pi.local keeps working). Outgoing traffic (streams, updates) is unaffected. Existing ufw
#     rules are kept. Skip with NO_FIREWALL=1 (e.g. if you reach the Pi over a VPN like Tailscale).
#  5. Security updates: Debian security fixes install automatically every day
#     (unattended-upgrades). It never reboots by itself.
#
# Safe to run again. <user> defaults to the account that ran sudo.
set -euo pipefail

[ "$(id -u)" = 0 ] || { echo "run with sudo" >&2; exit 1; }
U="${1:-${SUDO_USER:-}}"
[ -n "$U" ] && id "$U" >/dev/null 2>&1 || { echo "usage: sudo $0 <user>" >&2; exit 1; }
HOME_DIR="$(getent passwd "$U" | cut -d: -f6)"

# 1. SSH: keys only. sshd keeps the first value it reads, and drop-ins are read in name order,
#    so this file (10-…) wins over Pi OS's 50-cloud-init.conf ("PasswordAuthentication yes").
if [ -s "$HOME_DIR/.ssh/authorized_keys" ]; then
  conf=/etc/ssh/sshd_config.d/10-livemasjid-keys-only.conf
  cat > "$conf" <<'EOF'
# Written by Live Masjid's harden.sh: sign in with an SSH key only.
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin no
EOF
  chmod 644 "$conf"
  if sshd -t; then
    systemctl reload ssh 2>/dev/null || systemctl reload sshd
    echo "SSH: key sign-in only"
  else
    rm -f "$conf"
    echo "SSH: the new setting didn't validate; left unchanged" >&2
  fi
else
  echo "SSH: $U has no authorized key, so password logins stay on (add a key, then run this again)"
fi

# 2. sudo: remove blanket passwordless rules for the user (commented out, with a backup).
if [ "$(passwd -S "$U" | awk '{print $2}')" = P ]; then
  backup="$(mktemp -d /root/sudoers-backup.XXXXXX)"
  changed=0
  for f in /etc/sudoers.d/*; do
    [ -f "$f" ] && [ "$f" != /etc/sudoers.d/livemasjid ] || continue
    if grep -Eq "^[[:space:]]*${U}[[:space:]].*NOPASSWD:[[:space:]]*ALL" "$f"; then
      cp -p "$f" "$backup/"
      sed -i -E "s/^([[:space:]]*${U}[[:space:]].*NOPASSWD:[[:space:]]*ALL.*)$/# Disabled by Live Masjid's harden.sh: \1/" "$f"
      changed=1
      echo "sudo: removed passwordless ALL for $U from $f"
    fi
  done
  if ! visudo -c >/dev/null; then
    cp -p "$backup"/* /etc/sudoers.d/ 2>/dev/null || true
    echo "sudo: the result didn't validate; restored the backup" >&2
    exit 1
  fi
  [ "$changed" = 1 ] || echo "sudo: no blanket passwordless rule for $U"
  rmdir "$backup" 2>/dev/null || echo "sudo: backup of the changed files in $backup"
else
  echo "sudo: $U has no password, so sudo was left as it is (set one with 'passwd', then run this again)"
fi

# 3. App files: root-owned, not writable by anyone else.
if [ -d /opt/livemasjid ]; then
  chown -R root:root /opt/livemasjid
  chmod -R go-w /opt/livemasjid
fi
if [ -f /etc/sudoers.d/livemasjid ]; then
  chown root:root /etc/sudoers.d/livemasjid
  chmod 440 /etc/sudoers.d/livemasjid
fi
echo "App files: owned by root"

# Packages for steps 4 and 5 (refresh the package lists once if something is missing).
need=""
command -v ufw >/dev/null || need="$need ufw"
dpkg -s unattended-upgrades >/dev/null 2>&1 || need="$need unattended-upgrades"
if [ -n "$need" ]; then
  echo "Installing:$need"
  apt-get update -qq
  # shellcheck disable=SC2086
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq $need >/dev/null
fi

# 4. Firewall.
if [ "${NO_FIREWALL:-}" = 1 ]; then
  echo "Firewall: skipped (NO_FIREWALL=1)"
else
  # The web port, read in a subshell: /etc/default/livemasjid must not clobber this script's variables.
  PORT="$( ( [ -f /etc/default/livemasjid ] && . /etc/default/livemasjid; echo "${LIVEMASJID_PORT:-80}" ) )"
  ufw default deny incoming >/dev/null
  ufw default allow outgoing >/dev/null
  for net in 10.0.0.0/8 172.16.0.0/12 192.168.0.0/16 169.254.0.0/16 fe80::/10 fc00::/7; do
    ufw allow from "$net" to any port 22 proto tcp comment 'livemasjid: SSH from the home network' >/dev/null
    ufw allow from "$net" to any port "$PORT" proto tcp comment 'livemasjid: web page' >/dev/null
    ufw allow from "$net" to any port 5353 proto udp comment 'livemasjid: mDNS (pi.local)' >/dev/null
  done
  ufw --force enable >/dev/null
  echo "Firewall: on (SSH, web page on port $PORT and mDNS from the home network only)"
fi

# 5. Automatic security updates (Debian's default unattended-upgrades policy: security fixes only).
cat > /etc/apt/apt.conf.d/20auto-upgrades <<'CONF'
// Written by Live Masjid's harden.sh: install security updates every day.
APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
CONF
echo "Security updates: installed automatically every day (no automatic reboots)"
