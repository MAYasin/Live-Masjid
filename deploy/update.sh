#!/bin/bash
# Pull the latest GitHub release and install it if it differs from what's running.
# Run hourly by livemasjid-update.timer (as root); also triggerable from the web UI.
#
#   update.sh            normal update (skips a dev build deployed < 12 h ago)
#   update.sh --force    update even over a recent dev build
#   update.sh --check    report current/latest version and asset availability; change nothing
set -euo pipefail

D=/opt/livemasjid
MODE="${1:-}"
[ -f /etc/livemasjid/update.conf ] && . /etc/livemasjid/update.conf
REPO="${REPO:-}"

case "$(uname -m)" in
  aarch64|arm64) ARCH=aarch64 ;;
  armv7l|armv8l) ARCH=armv7 ;;
  armv6l) ARCH=armv6 ;;
  *) ARCH="" ;;
esac
ASSET="livemasjid-${ARCH}-linux.tar.gz"
CUR="$(cat "$D/VERSION" 2>/dev/null || echo none)"

if [ -z "$REPO" ]; then
  echo "REPO not set in /etc/livemasjid/update.conf; skipping"
  exit 0
fi
[ -n "$ARCH" ] || { echo "no prebuilt releases for $(uname -m)"; exit 0; }

JSON="$(curl -fsSL --retry 3 --max-time 30 -H 'Accept: application/vnd.github+json' \
  "https://api.github.com/repos/${REPO}/releases/latest")" || { echo "no release found for $REPO yet"; exit 0; }
TAG="$(printf '%s' "$JSON" | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
[ -n "$TAG" ] || { echo "could not read latest release tag" >&2; exit 1; }
HAS_ASSET=no; printf '%s' "$JSON" | grep -q "\"name\": *\"$ASSET\"" && HAS_ASSET=yes

if [ "$MODE" = "--check" ]; then
  echo "repo=$REPO"
  echo "arch=$ARCH"
  echo "installed=$CUR"
  echo "latest=$TAG"
  echo "asset=$ASSET"
  echo "asset_available=$HAS_ASSET"
  echo "up_to_date=$([ "$TAG" = "$CUR" ] && echo yes || echo no)"
  exit 0
fi

# A build pushed by deploy.ps1 is left alone for 12 h so development isn't overwritten.
if [[ "$CUR" == dev-* ]] && [ "$MODE" != "--force" ] && \
   [ $(( $(date +%s) - $(stat -c %Y "$D/VERSION") )) -lt 43200 ]; then
  echo "dev build $CUR installed recently; skipping (use --force)"
  exit 0
fi
if [ "$TAG" = "$CUR" ]; then
  echo "up to date ($CUR)"
  exit 0
fi
[ "$HAS_ASSET" = yes ] || { echo "release $TAG has no $ASSET; skipping"; exit 0; }

echo "updating $CUR -> $TAG ($ARCH)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
URL="https://github.com/${REPO}/releases/download/${TAG}/${ASSET}"
curl -fsSL --retry 3 --max-time 300 -o "$TMP/$ASSET" "$URL"
curl -fsSL --retry 3 --max-time 60 -o "$TMP/$ASSET.sha256" "$URL.sha256"
WANT="$(awk '{print $1}' "$TMP/$ASSET.sha256")"
GOT="$(sha256sum "$TMP/$ASSET" | awk '{print $1}')"
[ "$WANT" = "$GOT" ] || { echo "checksum mismatch" >&2; exit 1; }
tar -xzf "$TMP/$ASSET" -C "$TMP"
NEW="$TMP/livemasjid/deploy"

if [ -d "$NEW" ]; then
  # System configuration (BlueZ, bluez-alsa, Wi-Fi, sudo rules…) changed in this release:
  # re-run the installer's configuration steps once, without touching the binary.
  have="$(cat /etc/livemasjid/config-version 2>/dev/null || echo 0)"
  want="$(cat "$NEW/CONFIG_VERSION" 2>/dev/null || echo 0)"
  if [ "$want" -gt "$have" ]; then
    echo "applying system configuration v$want (was v$have)"
    bash "$NEW/install.sh" --skip-binary || echo "configuration step failed; continuing with the update" >&2
  else
    # Keep helper scripts and units current.
    install -m 755 "$NEW/update.sh" "$NEW/install-bin.sh" "$NEW/netwatch.sh" "$NEW/bt-recover.sh" "$NEW/bt-audio.sh" "$D/"
    changed=0
    for u in "$NEW"/livemasjid*.service "$NEW"/livemasjid*.timer; do
      if ! cmp -s "$u" "/etc/systemd/system/$(basename "$u")"; then
        install -m 644 "$u" /etc/systemd/system/
        changed=1
      fi
    done
    [ "$changed" = 1 ] && systemctl daemon-reload
  fi
fi

"$D/install-bin.sh" "$TMP/livemasjid/livemasjid" "$TAG"
