#!/bin/bash
# Tighten a Live Masjid device. Run once, as root:  sudo /opt/livemasjid/harden.sh [user]
#
#  1. SSH: key sign-in only (password logins off), but only if <user> has an authorized key,
#     so you can't lock yourself out.
#  2. sudo: no blanket "NOPASSWD: ALL" for <user> (Pi OS images ship one). The app keeps its own
#     narrow rules in /etc/sudoers.d/livemasjid. Skipped if <user> has no password, because sudo
#     would then be unusable.
#  3. App files: /opt/livemasjid and the sudo rules are owned by root and writable only by root.
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
