#!/bin/bash
# Pull the latest GitHub release and install it if it differs from what's running.
# Run hourly by livemasjid-update.timer (as root); also triggerable from the web UI.
#
#   update.sh            normal update (skips a dev build deployed < 12 h ago)
#   update.sh --force    update even over a recent dev build
#   update.sh --check    report current/latest version and asset availability; change nothing
#
# The web UI's "Update now" leaves $DATA/update-request and starts this service: that run is
# treated as --force. Timer runs respect the app's "Automatic updates" setting (settings.json
# "auto_update": false skips them). Progress is written to $DATA/update-status.json for the UI.
set -euo pipefail

D=/opt/livemasjid
DATA=/var/lib/livemasjid
MODE="${1:-}"
MANUAL=no

# The data folder belongs to the app's user. Root never reads or writes there directly: a
# compromised app could plant a symlink (e.g. update-status.json -> /etc/shadow) and have root
# overwrite or expose the target. Everything in $DATA goes through the app's own user.
as_app() {
  if [ "$(id -u)" = 0 ]; then runuser -u livemasjid -- "$@"; else "$@"; fi
}

if [ "$MODE" != "--check" ] && as_app test -f "$DATA/update-request"; then
  as_app rm -f "$DATA/update-request"
  MANUAL=yes
  MODE=--force
fi

# Status for the web UI (only for real runs, not --check).
status() { # state message [from] [to]
  [ "$MODE" = "--check" ] && return 0
  [ -d "$DATA" ] || return 0
  local manual=false
  [ "$MANUAL" = yes ] && manual=true
  printf '{"state":"%s","message":"%s","from":"%s","to":"%s","manual":%s,"at":%s}\n' \
    "$1" "$2" "${3:-}" "${4:-}" "$manual" "$(date +%s)" \
    | as_app sh -c 'umask 022 && cat > "$1.tmp" && mv -f "$1.tmp" "$1"' _ "$DATA/update-status.json" || true
}
trap 'rc=$?; [ $rc -ne 0 ] && [ -z "${REPORTED:-}" ] && status failed "The update failed (exit $rc); the current version keeps running" "${CUR:-}" "${TAG:-}"' EXIT

if [ "$MODE" != "--check" ] && [ "$MANUAL" = no ] \
   && as_app grep -Eqs '"auto_update" *: *false' "$DATA/settings.json"; then
  echo "automatic updates are off; skipping"
  exit 0
fi
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
status checking "Checking for a new version" "$CUR"

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
  status skipped "A test build is installed; automatic updates resume within 12 hours" "$CUR" "$TAG"
  exit 0
fi
if [ "$TAG" = "$CUR" ]; then
  echo "up to date ($CUR)"
  status up_to_date "Up to date" "$CUR" "$TAG"
  exit 0
fi
[ "$HAS_ASSET" = yes ] || { echo "release $TAG has no $ASSET; skipping"; exit 0; }

echo "updating $CUR -> $TAG ($ARCH)"
status updating "Downloading and installing" "$CUR" "$TAG"
TMP="$(mktemp -d)"
trap 'rc=$?; rm -rf "$TMP"; [ $rc -ne 0 ] && [ -z "${REPORTED:-}" ] && status failed "The update failed (exit $rc); the current version keeps running" "$CUR" "$TAG"' EXIT
URL="https://github.com/${REPO}/releases/download/${TAG}/${ASSET}"
curl -fsSL --retry 3 --max-time 300 -o "$TMP/$ASSET" "$URL"
curl -fsSL --retry 3 --max-time 60 -o "$TMP/$ASSET.sha256" "$URL.sha256"
WANT="$(awk '{print $1}' "$TMP/$ASSET.sha256")"
GOT="$(sha256sum "$TMP/$ASSET" | awk '{print $1}')"
[ "$WANT" = "$GOT" ] || { echo "checksum mismatch" >&2; status failed "The download was corrupted (checksum mismatch); try again" "$CUR" "$TAG"; REPORTED=1; exit 1; }

# Ed25519 signature. The public key was installed on the device earlier; it is never taken from the download.
PUB=/etc/livemasjid/release.pub
if [ -f "$PUB" ]; then
  refuse() { echo "$1; refusing to install" >&2; status failed "$1, so it was not installed" "$CUR" "$TAG"; REPORTED=1; exit 1; }
  command -v openssl >/dev/null || refuse "openssl is missing, so the release signature can't be checked"
  curl -fsSL --retry 3 --max-time 60 -o "$TMP/$ASSET.sig" "$URL.sig" || refuse "Release $TAG has no signature"
  openssl pkeyutl -verify -pubin -inkey "$PUB" -rawin -in "$TMP/$ASSET" -sigfile "$TMP/$ASSET.sig" >/dev/null 2>&1 \
    || refuse "Release $TAG has an invalid signature"
else
  echo "no release-signing key installed yet; skipping signature check" >&2
fi
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
    install -m 755 -o root -g root "$NEW/update.sh" "$NEW/install-bin.sh" "$NEW/netwatch.sh" "$NEW/bt-recover.sh" "$NEW/bt-audio.sh" "$NEW/harden.sh" "$D/"
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

if "$D/install-bin.sh" "$TMP/livemasjid/livemasjid" "$TAG" && [ "$(cat "$D/VERSION" 2>/dev/null)" = "$TAG" ]; then
  status updated "Updated" "$CUR" "$TAG"
else
  status failed "The new version didn't start properly, so the previous one was restored" "$CUR" "$TAG"
  REPORTED=1
  exit 1
fi
