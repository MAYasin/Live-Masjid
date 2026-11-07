#!/bin/bash
# Install a new livemasjid binary atomically, restart, health-check, and roll back on failure.
# Usage: install-bin.sh <path-to-binary> [version-label]
# Allowed via sudo without a password for the deploy user (see install.sh).
set -euo pipefail

NEW="${1:?usage: install-bin.sh <binary> [version]}"
D=/opt/livemasjid
[ -f /etc/default/livemasjid ] && . /etc/default/livemasjid
PORT="${LIVEMASJID_PORT:-80}"

[ -f "$NEW" ] || { echo "no such file: $NEW" >&2; exit 2; }
install -d -m 755 "$D/bin"
install -m 755 -o root -g root "$NEW" "$D/bin/livemasjid.new"

# Sanity check as the unprivileged service user (never execute an uploaded file as root).
if ! OUT=$(runuser -u livemasjid -- "$D/bin/livemasjid.new" --version 2>&1); then
  echo "new binary does not run on this system: $OUT" >&2
  rm -f "$D/bin/livemasjid.new"
  exit 3
fi
VER="${2:-$(echo "$OUT" | awk '{print $2}')}"

if [ -f "$D/bin/livemasjid" ]; then
  cp -p "$D/bin/livemasjid" "$D/bin/livemasjid.prev"
  cp -p "$D/VERSION" "$D/VERSION.prev" 2>/dev/null || echo unknown > "$D/VERSION.prev"
fi
mv -f "$D/bin/livemasjid.new" "$D/bin/livemasjid"
echo "$VER" > "$D/VERSION"
systemctl restart livemasjid.service

for _ in $(seq 1 25); do
  sleep 1
  if curl -fsS --max-time 2 "http://127.0.0.1:${PORT}/api/health" >/dev/null 2>&1; then
    echo "livemasjid $VER installed and healthy"
    exit 0
  fi
done

echo "health check failed for $VER" >&2
if [ -f "$D/bin/livemasjid.prev" ]; then
  echo "rolling back to $(cat "$D/VERSION.prev")" >&2
  mv -f "$D/bin/livemasjid.prev" "$D/bin/livemasjid"
  mv -f "$D/VERSION.prev" "$D/VERSION"
  systemctl restart livemasjid.service
fi
exit 1
