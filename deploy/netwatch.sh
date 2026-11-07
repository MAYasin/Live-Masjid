#!/bin/bash
# Network self-healing for the Pi Zero 2 W (its brcmfmac Wi-Fi can silently drop off the
# network and never come back, especially with power saving on).
# Runs every 30 s from livemasjid-netwatch.timer, as root. Escalates:
#   ~1 min offline  -> re-assert power-save off, reconnect Wi-Fi
#   ~3 min offline  -> restart NetworkManager
#   ~5 min offline  -> reload the Wi-Fi driver
#   ~10 min offline -> reboot
set -uo pipefail

STATE=/run/livemasjid-netwatch.count
[ -f /etc/livemasjid/netwatch.conf ] && . /etc/livemasjid/netwatch.conf
IFACE="${IFACE:-wlan0}"
IW=$(command -v iw || echo /usr/sbin/iw)
log() { echo "$*"; logger -t netwatch "$*"; }

# Always keep Wi-Fi power saving off (it re-enables after reconnects/driver reloads).
if [ -d "/sys/class/net/$IFACE" ] && "$IW" dev "$IFACE" get power_save 2>/dev/null | grep -q "on"; then
  "$IW" dev "$IFACE" set power_save off && log "wifi power save turned off"
fi

# Bluetooth controller health. A hung chip can still report "Powered: yes" while every HCI
# command times out (kernel: "hci0: Opcode 0x0401 failed: -110", "command 0x0408 tx timeout"),
# so scans find nothing and the speaker can't connect. Reset it, at most every 5 minutes.
BT_STAMP=/run/livemasjid-btreset.stamp
if [ -x /opt/livemasjid/bt-recover.sh ] && ls /sys/class/bluetooth/hci* >/dev/null 2>&1; then
  timeouts=$(journalctl -k --since "-90s" --no-pager -o cat 2>/dev/null \
    | grep -cE 'hci[0-9]+: (Opcode 0x[0-9a-f]+ failed: -110|command 0x[0-9a-f]+ tx timeout)')
  last=$(stat -c %Y "$BT_STAMP" 2>/dev/null || echo 0)
  if [ "${timeouts:-0}" -ge 2 ] && [ $(( $(date +%s) - last )) -gt 300 ]; then
    touch "$BT_STAMP"
    log "Bluetooth controller timing out ($timeouts HCI timeouts in 90 s); resetting"
    /opt/livemasjid/bt-recover.sh >/dev/null 2>&1
  fi
fi

online() {
  local gw
  gw=$(ip -4 route show default 2>/dev/null | awk '/default/ {print $3; exit}')
  if [ -n "$gw" ] && ping -c 1 -W 2 "$gw" >/dev/null 2>&1; then return 0; fi
  # Some routers drop ICMP: fall back to public resolvers.
  ping -c 1 -W 2 1.1.1.1 >/dev/null 2>&1 || ping -c 1 -W 2 8.8.8.8 >/dev/null 2>&1
}

if online; then
  [ -f "$STATE" ] && [ "$(cat "$STATE")" != 0 ] && log "network back"
  echo 0 > "$STATE"
  exit 0
fi

n=$(( $(cat "$STATE" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$STATE"
log "network unreachable (check $n)"

case "$n" in
  2|4)
    if command -v nmcli >/dev/null; then
      con=$(nmcli -t -f NAME,DEVICE con show --active 2>/dev/null | awk -F: -v i="$IFACE" '$2==i {print $1; exit}')
      if [ -n "$con" ]; then nmcli con up "$con" >/dev/null 2>&1 && log "reconnected $con"
      else nmcli device connect "$IFACE" >/dev/null 2>&1 && log "reconnected $IFACE"; fi
    else
      ip link set "$IFACE" down; sleep 2; ip link set "$IFACE" up; log "cycled $IFACE"
    fi
    ;;
  6)
    log "restarting network stack"
    systemctl restart NetworkManager 2>/dev/null || systemctl restart networking 2>/dev/null || systemctl restart wpa_supplicant 2>/dev/null || true
    ;;
  10)
    drv=$(basename "$(readlink -f "/sys/class/net/$IFACE/device/driver/module" 2>/dev/null)" 2>/dev/null)
    if [ -n "$drv" ] && [ "$drv" != "." ]; then
      log "reloading Wi-Fi driver $drv"
      modprobe -r "${drv}_wcc" "$drv" 2>/dev/null; sleep 2; modprobe "$drv"; sleep 8
    fi
    systemctl restart NetworkManager 2>/dev/null || systemctl restart networking 2>/dev/null || true
    ;;
  20)
    log "network down ~10 min; rebooting"
    sync
    systemctl reboot
    ;;
esac
exit 0
