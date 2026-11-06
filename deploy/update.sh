#!/bin/bash
# Pull the latest GitHub release and install it if it differs from what's running.
# Run hourly by livemasjid-update.timer (as root); also triggerable from the web UI.
set -euo pipefail

D=/opt/livemasjid
[ -f /etc/livemasjid/update.conf ] && . /etc/livemasjid/update.conf
REPO="${REPO:-}"
ASSET="livemasjid-aarch64-linux.tar.gz"
if [ -z "$REPO" ]; then
  echo "REPO not set in /etc/livemasjid/update.conf; skipping"
  exit 0
fi

CUR="$(cat "$D/VERSION" 2>/dev/null || echo none)"
# A build pushed by deploy.ps1 is left alone for 12 h so development isn't overwritten.
if [[ "$CUR" == dev-* ]] && [ "${1:-}" != "--force" ] && \
   [ $(( $(date +%s) - $(stat -c %Y "$D/VERSION") )) -lt 43200 ]; then
  echo "dev build $CUR installed recently; skipping (use --force)"
  exit 0
fi

JSON="$(curl -fsSL --retry 3 --max-time 30 -H 'Accept: application/vnd.github+json' \
  "https://api.github.com/repos/${REPO}/releases/latest")" || { echo "no release found for $REPO yet"; exit 0; }
TAG="$(printf '%s' "$JSON" | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
[ -n "$TAG" ] || { echo "could not read latest release tag" >&2; exit 1; }
if [ "$TAG" = "$CUR" ]; then
  echo "up to date ($CUR)"
  exit 0
fi

echo "updating $CUR -> $TAG"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
URL="https://github.com/${REPO}/releases/download/${TAG}/${ASSET}"
curl -fsSL --retry 3 --max-time 300 -o "$TMP/$ASSET" "$URL"
curl -fsSL --retry 3 --max-time 60 -o "$TMP/$ASSET.sha256" "$URL.sha256"
WANT="$(awk '{print $1}' "$TMP/$ASSET.sha256")"
GOT="$(sha256sum "$TMP/$ASSET" | awk '{print $1}')"
[ "$WANT" = "$GOT" ] || { echo "checksum mismatch" >&2; exit 1; }
tar -xzf "$TMP/$ASSET" -C "$TMP"

# Refresh helper scripts and units shipped with the release.
if [ -d "$TMP/livemasjid/deploy" ]; then
  install -m 755 "$TMP/livemasjid/deploy/update.sh" "$TMP/livemasjid/deploy/install-bin.sh" "$TMP/livemasjid/deploy/netwatch.sh" "$D/"
  changed=0
  for u in livemasjid.service livemasjid-update.service livemasjid-update.timer livemasjid-netwatch.service livemasjid-netwatch.timer; do
    if ! cmp -s "$TMP/livemasjid/deploy/$u" "/etc/systemd/system/$u"; then
      install -m 644 "$TMP/livemasjid/deploy/$u" /etc/systemd/system/
      changed=1
    fi
  done
  if [ "$changed" = 1 ]; then
    systemctl daemon-reload
    systemctl enable --now livemasjid-netwatch.timer >/dev/null 2>&1 || true
  fi
fi

"$D/install-bin.sh" "$TMP/livemasjid/livemasjid" "$TAG"
