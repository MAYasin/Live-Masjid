#!/bin/bash
# One-time (idempotent) Pi setup. Run as root from a directory containing the deploy files:
#   sudo bash install.sh [--repo owner/name]
# Installs dependencies, the service user, systemd units, the auto-updater and the binary
# (the one next to this script if present, otherwise the latest GitHub release).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="${REPO:-}"
while [ $# -gt 0 ]; do
  case "$1" in
    --repo) REPO="$2"; shift 2 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
[ -z "$REPO" ] && [ -f "$HERE/REPO" ] && REPO="$(tr -d '[:space:]' < "$HERE/REPO")"
[ "$(id -u)" = 0 ] || { echo "run with sudo" >&2; exit 1; }
DEPLOY_USER="${SUDO_USER:-pi}"
D=/opt/livemasjid

echo "==> Installing packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends bluez bluez-alsa-utils alsa-utils curl ca-certificates rfkill iw iputils-ping >/dev/null

echo "==> Bluetooth: unblock radio, power on at boot"
rfkill unblock bluetooth || true
if [ -f /etc/bluetooth/main.conf ]; then
  if grep -qE '^\s*#?\s*AutoEnable\s*=' /etc/bluetooth/main.conf; then
    sed -i -E 's/^\s*#?\s*AutoEnable\s*=.*/AutoEnable=true/' /etc/bluetooth/main.conf
  else
    printf '\n[Policy]\nAutoEnable=true\n' >> /etc/bluetooth/main.conf
  fi
  # Reconnect to trusted speakers quickly and keep trying.
  sed -i -E 's/^\s*#?\s*ReconnectAttempts\s*=.*/ReconnectAttempts=7/' /etc/bluetooth/main.conf || true
fi
systemctl enable bluetooth.service >/dev/null 2>&1 || true
# Restart so bluetoothd re-initialises the now-unblocked adapter.
systemctl restart bluetooth.service || true
for u in bluealsa.service bluealsad.service; do
  if systemctl list-unit-files "$u" >/dev/null 2>&1 && systemctl enable --now "$u" >/dev/null 2>&1; then
    echo "    $u enabled"; break
  fi
done

echo "==> Wi-Fi: disable power saving (the Zero 2 W drops off the network with it on)"
if [ -d /etc/NetworkManager/conf.d ]; then
  printf '[connection]\nwifi.powersave = 2\n' > /etc/NetworkManager/conf.d/livemasjid-wifi-powersave.conf
  nmcli -t -f NAME,TYPE con show 2>/dev/null | awk -F: '$2=="802-11-wireless" {print $1}' | while read -r c; do
    nmcli con modify "$c" 802-11-wireless.powersave 2 >/dev/null 2>&1 || true
  done
  systemctl reload NetworkManager >/dev/null 2>&1 || true
fi
/usr/sbin/iw dev wlan0 set power_save off >/dev/null 2>&1 || true

echo "==> Watchdogs: hardware watchdog + persistent logs"
mkdir -p /etc/systemd/system.conf.d /var/log/journal
printf '[Manager]\nRuntimeWatchdogSec=15s\nRebootWatchdogSec=2min\n' > /etc/systemd/system.conf.d/livemasjid-watchdog.conf
systemctl daemon-reexec || true

echo "==> Journald: cap log size to protect the SD card"
mkdir -p /etc/systemd/journald.conf.d
printf '[Journal]\nStorage=persistent\nSystemMaxUse=50M\n' > /etc/systemd/journald.conf.d/livemasjid.conf
systemctl restart systemd-journald || true

echo "==> Service user"
id livemasjid >/dev/null 2>&1 || useradd --system --home-dir /var/lib/livemasjid --shell /usr/sbin/nologin livemasjid
usermod -aG audio,bluetooth livemasjid

echo "==> Files"
install -d -m 755 "$D" "$D/bin" /etc/livemasjid
install -m 755 "$HERE/update.sh" "$HERE/install-bin.sh" "$HERE/netwatch.sh" "$HERE/bt-recover.sh" "$D/"
install -m 644 "$HERE"/livemasjid*.service "$HERE"/livemasjid*.timer /etc/systemd/system/
[ -f /etc/default/livemasjid ] || echo 'LIVEMASJID_PORT=80' > /etc/default/livemasjid
if [ -n "$REPO" ]; then
  echo "REPO=$REPO" > /etc/livemasjid/update.conf
  echo "    auto-update from github.com/$REPO"
fi

echo "==> sudo rules (deploy without password, update from UI)"
cat > /etc/sudoers.d/livemasjid <<EOF
$DEPLOY_USER ALL=(root) NOPASSWD: $D/install-bin.sh
livemasjid ALL=(root) NOPASSWD: /usr/bin/systemctl start --no-block livemasjid-update.service
livemasjid ALL=(root) NOPASSWD: /usr/sbin/rfkill unblock bluetooth
livemasjid ALL=(root) NOPASSWD: /opt/livemasjid/bt-recover.sh
EOF
chmod 440 /etc/sudoers.d/livemasjid
visudo -cf /etc/sudoers.d/livemasjid >/dev/null

systemctl daemon-reload
systemctl enable livemasjid.service livemasjid-update.timer livemasjid-netwatch.timer >/dev/null
systemctl start livemasjid-update.timer livemasjid-netwatch.timer

echo "==> Binary"
if [ -f "$HERE/livemasjid" ]; then
  "$D/install-bin.sh" "$HERE/livemasjid" "${VERSION:-}"
elif [ ! -x "$D/bin/livemasjid" ]; then
  "$D/update.sh" --force
else
  systemctl restart livemasjid.service
fi

IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
PORT="$(. /etc/default/livemasjid; echo "${LIVEMASJID_PORT:-80}")"
echo
echo "Done. Open http://${IP}$( [ "$PORT" = 80 ] || echo ":$PORT" )/ or http://$(hostname).local$( [ "$PORT" = 80 ] || echo ":$PORT" )/"
