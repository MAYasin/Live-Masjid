#!/bin/bash
# Reset the Pi's UART-attached Bluetooth chip (BCM43430) when it stops responding.
# Symptom: kernel "hci0: Opcode 0x0c03 failed: -110", bluetoothd "Failed to set mode".
# Re-binding the hci_uart_bcm driver re-uploads the firmware and brings it back.
# Called (via a narrow sudo rule) by the livemasjid service; safe to run by hand.
set -u

DRV=/sys/bus/serial/drivers/hci_uart_bcm
log() { echo "$*"; logger -t bt-recover "$*"; }

# The serdev device name is usually serial0-0; discover it if bound, else assume.
dev=$(ls "$DRV" 2>/dev/null | grep -E '^serial[0-9]+-[0-9]+$' | head -1)
dev=${dev:-serial0-0}

log "resetting Bluetooth controller ($dev)"
if [ -e "$DRV/$dev" ]; then
  echo "$dev" > "$DRV/unbind" 2>/dev/null
  sleep 2
fi
if ! echo "$dev" > "$DRV/bind" 2>/dev/null; then
  log "rebind failed; reloading hci_uart module"
  modprobe -r hci_uart 2>/dev/null
  sleep 1
  modprobe hci_uart
fi
sleep 5
rfkill unblock bluetooth 2>/dev/null
systemctl restart bluetooth.service
sleep 2
systemctl restart bluealsa.service 2>/dev/null || systemctl restart bluealsad.service 2>/dev/null || true
log "Bluetooth controller reset complete"
