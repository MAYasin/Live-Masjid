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
IFACE="${IFACE:-wlan0}"
IW=$(command -v iw || echo /usr/sbin/iw)
log() { echo "$*"; logger -t netwatch "$*"; }

# Always keep Wi-Fi power saving off (it re-enables after reconnects/driver reloads).
if [ -d "/sys/class/net/$IFACE" ] && "$IW" dev "$IFACE" get power_save 2>/dev/null | grep -q "on"; then
  "$IW" dev "$IFACE" set power_save off && log "wifi power save turned off"
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
    con=$(nmcli -t -f NAME,DEVICE con show --active 2>/dev/null | awk -F: -v i="$IFACE" '$2==i {print $1; exit}')
    if [ -n "$con" ]; then nmcli con up "$con" >/dev/null 2>&1 && log "reconnected $con"
    else nmcli device connect "$IFACE" >/dev/null 2>&1 && log "reconnected $IFACE"; fi
    ;;
  6)
    log "restarting NetworkManager"
    systemctl restart NetworkManager
    ;;
  10)
    log "reloading Wi-Fi driver"
    modprobe -r brcmfmac_wcc brcmfmac 2>/dev/null; sleep 2; modprobe brcmfmac; sleep 8
    systemctl restart NetworkManager
    ;;
  20)
    log "network down ~10 min; rebooting"
    sync
    systemctl reboot
    ;;
esac
exit 0
