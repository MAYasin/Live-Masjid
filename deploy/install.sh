#!/bin/bash
# Live Masjid installer — idempotent; safe to re-run. Normally driven by setup.ps1, but works
# standalone on any Debian-based single-board computer (Raspberry Pi OS, Armbian, DietPi, Ubuntu).
#
#   sudo bash install.sh [--repo owner/name] [--wifi-file FILE] [--country CC] [--hostname NAME] [--skip-binary] [--upgrade]
#
# --upgrade: bring the OS, kernel, drivers and firmware fully up to date first (recommended on
#            a new device). Prints "REBOOT REQUIRED" when a new kernel needs a reboot.
#
# --wifi-file: a file with lines  SSID=<name>  PSK=<password>  (deleted after use)
# The binary is taken from ./livemasjid if present, otherwise the latest GitHub release.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="${REPO:-}"
WIFI_FILE=""
COUNTRY=""
NEW_HOSTNAME=""
SKIP_BINARY=0
UPGRADE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --repo) REPO="$2"; shift 2 ;;
    --wifi-file) WIFI_FILE="$2"; shift 2 ;;
    --country) COUNTRY="$2"; shift 2 ;;
    --hostname) NEW_HOSTNAME="$2"; shift 2 ;;
    --skip-binary) SKIP_BINARY=1; shift ;;
    --upgrade) UPGRADE=1; shift ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
[ "$(id -u)" = 0 ] || { echo "run with sudo" >&2; exit 1; }
[ -z "$REPO" ] && [ -f /etc/livemasjid/update.conf ] && REPO="$(. /etc/livemasjid/update.conf; echo "${REPO:-}")"
DEPLOY_USER="${SUDO_USER:-pi}"
BIN_VERSION="${VERSION:-}"   # build label passed by deploy.ps1 / setup.ps1
D=/opt/livemasjid

step() { echo "==> $*"; }
info() { echo "    $*"; }
warn() { echo "    ! $*" >&2; }

# set_ini FILE SECTION KEY VALUE — set "KEY = VALUE" inside [SECTION], adding either if missing.
set_ini() {
  local f=$1 s=$2 k=$3 v=$4
  awk -v sec="[$s]" -v key="$k" -v val="$v" '
    function emit() { print key " = " val; done = 1 }
    /^\[/ { if (insec && !done) emit(); insec = ($0 == sec) }
    insec && $0 ~ "^[#;[:space:]]*" key "[[:space:]]*=" { if (!done) emit(); next }
    { print }
    END { if (!done) { if (!insec) print sec; emit() } }' "$f" > "$f.tmp" && mv "$f.tmp" "$f"
}

# ---------------------------------------------------------------------------- detection
command -v apt-get >/dev/null || { echo "This installer supports Debian-based systems (apt) only." >&2; exit 1; }
MODEL="$(tr -d '\0' < /proc/device-tree/model 2>/dev/null || echo "unknown board")"
IS_PI=0; [[ "$MODEL" == *"Raspberry Pi"* ]] && IS_PI=1
ARCH="$(uname -m)"
WIFI_IF="$(for i in /sys/class/net/*/wireless; do [ -e "$i" ] && basename "$(dirname "$i")"; done 2>/dev/null | head -1)"
HAS_NM=0; command -v nmcli >/dev/null && systemctl is-active --quiet NetworkManager && HAS_NM=1
OS_NAME="$(. /etc/os-release 2>/dev/null; echo "${PRETTY_NAME:-unknown OS}")"  # subshell: os-release defines VERSION too
step "Detected: $MODEL · $ARCH · $OS_NAME"
info "Wi-Fi: ${WIFI_IF:-none} · network stack: $([ $HAS_NM = 1 ] && echo NetworkManager || echo wpa_supplicant/ifupdown)"
case "$ARCH" in aarch64|arm64|armv7l|armv8l|armv6l) ;; *) warn "architecture $ARCH has no prebuilt release; use a local build" ;; esac

# ---------------------------------------------------------------------------- packages
step "Installing packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends alsa-utils curl ca-certificates iw iputils-ping rfkill >/dev/null
BT_AUDIO=1
apt-get install -y -qq --no-install-recommends bluez >/dev/null || { warn "bluez not available"; BT_AUDIO=0; }
if [ $BT_AUDIO = 1 ]; then
  apt-get install -y -qq --no-install-recommends bluez-alsa-utils >/dev/null \
    || { warn "bluez-alsa-utils not available on this OS: Bluetooth audio disabled, wired output still works"; BT_AUDIO=0; }
fi

# ---------------------------------------------------------------------------- system update
if [ "$UPGRADE" = 1 ]; then
  step "Updating the system: kernel, drivers, firmware (this can take 10+ minutes on a Pi Zero)"
  apt-get -y -qq -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold full-upgrade >/dev/null
  apt-get -y -qq autoremove --purge >/dev/null || true
  if command -v rpi-eeprom-update >/dev/null; then rpi-eeprom-update -a >/dev/null 2>&1 || true; fi  # Pi 4/5 bootloader
  info "system up to date (running kernel $(uname -r))"
fi

# ---------------------------------------------------------------------------- hostname
if [ -n "$NEW_HOSTNAME" ] && [ "$NEW_HOSTNAME" != "$(hostname)" ]; then
  step "Hostname: $NEW_HOSTNAME"
  old="$(hostname)"
  hostnamectl set-hostname "$NEW_HOSTNAME"
  sed -i "s/\b$old\b/$NEW_HOSTNAME/g" /etc/hosts
  grep -q "$NEW_HOSTNAME" /etc/hosts || echo "127.0.1.1 $NEW_HOSTNAME" >> /etc/hosts
fi

# ---------------------------------------------------------------------------- Wi-Fi
if [ -n "$WIFI_IF" ]; then
  step "Wi-Fi"
  if [ -n "$COUNTRY" ]; then
    iw reg set "$COUNTRY" 2>/dev/null || true
    if [ $IS_PI = 1 ] && command -v raspi-config >/dev/null; then raspi-config nonint do_wifi_country "$COUNTRY" >/dev/null 2>&1 || true; fi
    info "regulatory country $COUNTRY"
  fi
  if [ -n "$WIFI_FILE" ] && [ -f "$WIFI_FILE" ]; then
    SSID="$(sed -n 's/^SSID=//p' "$WIFI_FILE")"; PSK="$(sed -n 's/^PSK=//p' "$WIFI_FILE")"
    rm -f "$WIFI_FILE"
    if [ -n "$SSID" ]; then
      if [ $HAS_NM = 1 ]; then
        nmcli con delete "livemasjid-$SSID" >/dev/null 2>&1 || true
        nmcli con add type wifi ifname "$WIFI_IF" con-name "livemasjid-$SSID" ssid "$SSID" \
          wifi-sec.key-mgmt wpa-psk wifi-sec.psk "$PSK" connection.autoconnect yes \
          connection.autoconnect-priority 10 802-11-wireless.powersave 2 >/dev/null
      else
        conf=/etc/wpa_supplicant/wpa_supplicant.conf
        [ -f "$conf" ] || printf 'ctrl_interface=DIR=/var/run/wpa_supplicant GROUP=netdev\nupdate_config=1\n' > "$conf"
        [ -n "$COUNTRY" ] && { grep -q '^country=' "$conf" && sed -i "s/^country=.*/country=$COUNTRY/" "$conf" || echo "country=$COUNTRY" >> "$conf"; }
        wpa_passphrase "$SSID" "$PSK" | grep -v '#psk' >> "$conf"
        chmod 600 "$conf"
      fi
      info "added network \"$SSID\" (takes effect on the next reconnect/reboot; the current connection is kept)"
    fi
  fi
  # Power saving makes Broadcom/Cypress Wi-Fi (all Pis) drop off the network.
  if [ $HAS_NM = 1 ]; then
    printf '[connection]\nwifi.powersave = 2\n' > /etc/NetworkManager/conf.d/livemasjid-wifi-powersave.conf
    nmcli -t -f NAME,TYPE con show | awk -F: '$2=="802-11-wireless" {print $1}' | while read -r c; do
      nmcli con modify "$c" 802-11-wireless.powersave 2 >/dev/null 2>&1 || true
    done
    systemctl reload NetworkManager >/dev/null 2>&1 || true
  fi
  iw dev "$WIFI_IF" set power_save off >/dev/null 2>&1 || true
  info "power saving off"
fi

# ---------------------------------------------------------------------------- Bluetooth
if [ -d /sys/class/bluetooth ] && [ $BT_AUDIO = 1 ]; then
  step "Bluetooth"
  rfkill unblock bluetooth || true
  conf=/etc/bluetooth/main.conf
  [ -f "$conf" ] || touch "$conf"
  set_ini "$conf" General ControllerMode bredr          # classic only: speakers don't need LE
  set_ini "$conf" General FastConnectable true          # quicker reconnects
  set_ini "$conf" General JustWorksRepairing always     # re-pair without prompts
  set_ini "$conf" Policy AutoEnable true                 # power on at boot
  set_ini "$conf" Policy ReconnectAttempts 7
  set_ini "$conf" Policy ReconnectIntervals "1,2,4,8,16,32,64"
  info "BlueZ: classic mode, fast connectable, auto power-on"

  # bluez-alsa: audio source + battery reporting, robust SBC quality (see bt-audio.sh).
  bash "$HERE/bt-audio.sh" && info "bluez-alsa: A2DP source, battery reporting, SBC $(cat /etc/livemasjid/bt-quality 2>/dev/null)"
  systemctl enable bluetooth.service >/dev/null 2>&1 || true
  systemctl restart bluetooth.service || true
  systemctl restart bluealsa.service 2>/dev/null || systemctl restart bluealsad.service 2>/dev/null || true
else
  warn "no Bluetooth (or no bluez-alsa): skipping Bluetooth setup"
fi

# ---------------------------------------------------------------------------- reliability
step "Reliability: watchdog, persistent logs"
mkdir -p /etc/systemd/system.conf.d /etc/systemd/journald.conf.d /var/log/journal
if [ -e /dev/watchdog ] || [ -e /dev/watchdog0 ]; then
  printf '[Manager]\nRuntimeWatchdogSec=15s\nRebootWatchdogSec=2min\n' > /etc/systemd/system.conf.d/livemasjid-watchdog.conf
  info "hardware watchdog 15 s"
else
  warn "no hardware watchdog on this board"
fi
printf '[Journal]\nStorage=persistent\nSystemMaxUse=50M\n' > /etc/systemd/journald.conf.d/livemasjid.conf
systemctl daemon-reexec || true
systemctl restart systemd-journald || true

# ---------------------------------------------------------------------------- service
step "Service user and files"
id livemasjid >/dev/null 2>&1 || useradd --system --home-dir /var/lib/livemasjid --shell /usr/sbin/nologin livemasjid
for g in audio bluetooth; do getent group "$g" >/dev/null && usermod -aG "$g" livemasjid; done
install -d -m 755 "$D" "$D/bin" /etc/livemasjid
install -m 755 "$HERE/update.sh" "$HERE/install-bin.sh" "$HERE/netwatch.sh" "$HERE/bt-recover.sh" "$HERE/bt-audio.sh" "$D/"
install -m 644 "$HERE"/livemasjid*.service "$HERE"/livemasjid*.timer /etc/systemd/system/
# Release-signing public key: installed once and never overwritten by a later release (trust on first use).
[ -f /etc/livemasjid/release.pub ] || { [ -f "$HERE/release.pub" ] && install -m 644 "$HERE/release.pub" /etc/livemasjid/release.pub; }
[ -f /etc/default/livemasjid ] || echo 'LIVEMASJID_PORT=80' > /etc/default/livemasjid
if [ -n "$WIFI_IF" ]; then echo "IFACE=$WIFI_IF" > /etc/livemasjid/netwatch.conf; fi
if [ -n "$REPO" ]; then
  echo "REPO=$REPO" > /etc/livemasjid/update.conf
  info "auto-update from github.com/$REPO"
fi

cat > /etc/sudoers.d/livemasjid <<EOF
$DEPLOY_USER ALL=(root) NOPASSWD: $D/install-bin.sh
livemasjid ALL=(root) NOPASSWD: /usr/bin/systemctl start --no-block livemasjid-update.service
livemasjid ALL=(root) NOPASSWD: /usr/sbin/rfkill unblock bluetooth
livemasjid ALL=(root) NOPASSWD: $D/bt-recover.sh
livemasjid ALL=(root) NOPASSWD: /usr/bin/systemctl start livemasjid-bt-audio@medium.service, /usr/bin/systemctl start livemasjid-bt-audio@high.service
EOF
chmod 440 /etc/sudoers.d/livemasjid
visudo -cf /etc/sudoers.d/livemasjid >/dev/null

systemctl daemon-reload
systemctl enable livemasjid.service livemasjid-update.timer livemasjid-netwatch.timer >/dev/null
systemctl start livemasjid-update.timer livemasjid-netwatch.timer

install -m 644 "$HERE/CONFIG_VERSION" /etc/livemasjid/config-version 2>/dev/null || true

if [ "$SKIP_BINARY" = 1 ]; then
  step "Configuration applied (binary unchanged)"
  systemctl restart livemasjid.service || true
  exit 0
fi

step "Binary"
if [ -f "$HERE/livemasjid" ]; then
  "$D/install-bin.sh" "$HERE/livemasjid" "$BIN_VERSION"
elif [ ! -x "$D/bin/livemasjid" ]; then
  "$D/update.sh" --force
else
  systemctl restart livemasjid.service
fi

# A kernel installed since boot (e.g. by --upgrade) only takes effect after a reboot.
REBOOT=0
[ -f /var/run/reboot-required ] && REBOOT=1
# Compare only kernels of the running flavour (Pi images ship several, e.g. -v8 and -2712).
CUR_KERNEL="$(uname -r)"
NEWEST_KERNEL="$(ls /lib/modules 2>/dev/null | grep -- "-${CUR_KERNEL##*-}\$" | sort -V | tail -1)"
if [ -n "$NEWEST_KERNEL" ] && [ "$NEWEST_KERNEL" != "$CUR_KERNEL" ]; then REBOOT=1; fi

IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
PORT="$(. /etc/default/livemasjid; echo "${LIVEMASJID_PORT:-80}")"
SUFFIX="$([ "$PORT" = 80 ] || echo ":$PORT")"
echo
echo "Done. Open http://${IP}${SUFFIX}/ or http://$(hostname).local${SUFFIX}/"
if [ "$REBOOT" = 1 ]; then echo "REBOOT REQUIRED: a new kernel/firmware was installed. Run: sudo reboot"; fi
