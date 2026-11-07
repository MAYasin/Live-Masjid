#!/bin/bash
# Configure bluez-alsa for Live Masjid and restart it.
#   bt-audio.sh [low|medium|high|xq]     (default: the saved setting, else "medium")
#
# * a2dp-source  – the Pi sends audio to the speaker (nothing can stream *to* the Pi)
# * hfp-ag       – Handsfree control channel only; lets the speaker report its battery level.
#                  The low-quality call (SCO) audio is never opened.
# * --sbc-quality – "medium" (default) is indistinguishable from "high" for speech. "low" was
#                  removed: on some speakers its bitpool cannot sustain real time (choppy audio).
# Called by install.sh and (through a narrow sudo rule) by the livemasjid service.
set -euo pipefail

SAVED=/etc/livemasjid/bt-quality
QUALITY="${1:-$(cat "$SAVED" 2>/dev/null || echo medium)}"
[ "$QUALITY" = low ] && QUALITY=medium   # "low" was removed: it can't sustain real time on some speakers
case "$QUALITY" in medium|high|xq) ;; *) echo "invalid quality: $QUALITY" >&2; exit 2 ;; esac

BIN="$(command -v bluealsad || command -v bluealsa || true)"
UNIT=""
for u in bluealsa.service bluealsad.service; do systemctl cat "$u" >/dev/null 2>&1 && { UNIT=$u; break; }; done
[ -n "$BIN" ] && [ -n "$UNIT" ] || { echo "bluez-alsa not installed"; exit 0; }

mkdir -p /etc/livemasjid "/etc/systemd/system/$UNIT.d"
echo "$QUALITY" > "$SAVED"
CONF="/etc/systemd/system/$UNIT.d/livemasjid.conf"
NEW="$(printf '[Service]\nExecStart=\nExecStart=%s -S -p a2dp-source -p hfp-ag --sbc-quality=%s --keep-alive=10\n' "$BIN" "$QUALITY")"
if [ "$(cat "$CONF" 2>/dev/null)" != "$NEW" ]; then
  printf '%s\n' "$NEW" > "$CONF"
  systemctl daemon-reload
  systemctl enable "$UNIT" >/dev/null 2>&1 || true
  systemctl restart "$UNIT"
  echo "bluez-alsa: SBC quality $QUALITY, A2DP source + HFP (battery)"
else
  echo "bluez-alsa: unchanged (SBC quality $QUALITY)"
fi
