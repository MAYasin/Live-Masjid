#!/usr/bin/env bash
# Development loop: build for the device's CPU, upload, install (health check + rollback).
# Runs on macOS, Linux and Windows (Git Bash or WSL). Needs Rust (rustup) for building.
#
#   ./deploy.sh                       build + deploy to pi@192.168.1.101
#   ./deploy.sh --host pi@my-pi.local another device
#   ./deploy.sh --setup               re-run the full installer with this build
#   ./deploy.sh --scripts             also install the helper scripts (asks for the Pi password)
#   ./deploy.sh --logs                follow the service log
# For a brand-new device use ./setup.sh (the guided wizard).
set -u
cd "$(dirname "$0")"

HOST="pi@192.168.1.101"; SETUP=0; SCRIPTS=0; LOGS=0; REPO=""
while [ $# -gt 0 ]; do
  case "$1" in
    --host) HOST="$2"; shift 2 ;;
    --setup) SETUP=1; shift ;;
    --scripts) SCRIPTS=1; shift ;;
    --repo) REPO="$2"; shift 2 ;;
    --logs) LOGS=1; shift ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
SSH_OPTS="-o ConnectTimeout=10 -o BatchMode=yes"
STAGE=/tmp/livemasjid-deploy
die() { echo "!! $*" >&2; exit 1; }

[ "$LOGS" = 1 ] && exec ssh -t "$HOST" "journalctl -u livemasjid -f -n 100"

MACHINE="$(ssh $SSH_OPTS "$HOST" uname -m 2>/dev/null)" || die "cannot reach $HOST (use ./setup.sh for a new device)"
case "$MACHINE" in
  aarch64|arm64) TRIPLE=aarch64-unknown-linux-musl ;;
  armv7l|armv8l) TRIPLE=armv7-unknown-linux-musleabihf ;;
  armv6l)        TRIPLE=arm-unknown-linux-musleabihf ;;
  *) die "unsupported CPU '$MACHINE'" ;;
esac
CARGO="$(command -v cargo || echo "$HOME/.cargo/bin/cargo")"
SHA="$(git rev-parse --short HEAD 2>/dev/null || echo local)"
[ -n "$(git status --porcelain 2>/dev/null)" ] && SHA="$SHA-dirty"
VERSION="dev-$SHA"

echo "==> Building $VERSION for $MACHINE ($TRIPLE)"
LIVEMASJID_BUILD="$VERSION" "$CARGO" build --release --target "$TRIPLE" || die "build failed"

echo "==> Uploading to $HOST"
ssh $SSH_OPTS "$HOST" "rm -rf $STAGE && mkdir -p $STAGE" || die "ssh failed"
scp -q -o ConnectTimeout=10 deploy/* "target/$TRIPLE/release/livemasjid" "$HOST:$STAGE/" || die "upload failed"
# Guard against CRLF line endings sneaking into scripts/units (e.g. checked out on Windows).
ssh $SSH_OPTS "$HOST" "sed -i 's/\r$//' $STAGE/*.sh $STAGE/*.service $STAGE/*.timer $STAGE/CONFIG_VERSION && chmod +x $STAGE/*.sh $STAGE/livemasjid"

if [ "$SETUP" = 1 ]; then
  echo "==> Running the installer (sudo may ask for a password)"
  ssh -t "$HOST" "sudo VERSION=$VERSION bash $STAGE/install.sh ${REPO:+--repo $REPO}" || die "installer failed"
else
  ssh $SSH_OPTS "$HOST" "sudo -n /opt/livemasjid/install-bin.sh $STAGE/livemasjid $VERSION" \
    || die "deploy failed. First deploy to this device? Run ./setup.sh"
  # The binary runs as the unprivileged livemasjid user, so installing it needs no password (one
  # narrow sudo rule). The helper scripts run as root, so replacing them always asks for the Pi
  # password: nothing may change code that root runs without it. Releases update them (signed).
  if [ "$SCRIPTS" = 1 ]; then
    echo "==> Installing the helper scripts (sudo asks for the Pi password)"
    ssh -t "$HOST" "cd $STAGE && sudo install -m 755 -o root -g root update.sh install-bin.sh netwatch.sh bt-recover.sh bt-audio.sh harden.sh /opt/livemasjid/" \
      || die "could not install the helper scripts"
  elif ! git diff --quiet HEAD -- deploy/ 2>/dev/null; then
    echo "   (deploy/ has uncommitted changes: add --scripts to install the helper scripts too)"
  fi
fi
echo "==> Done: http://${HOST#*@}/"
