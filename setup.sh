#!/usr/bin/env bash
# Live Masjid — guided setup of a new single-board computer.
#
# Runs on macOS, Linux and Windows (Git Bash or WSL). Needs: bash, ssh, scp, curl.
# Guide: docs/SETUP.md. Re-running is always safe.
#
#   ./setup.sh                                  fully interactive
#   ./setup.sh --device 192.168.1.50 --user pi  skip the first questions
#   ./setup.sh --local-build                    install a build from this computer (needs Rust)
#   ./setup.sh --yes                            accept defaults (no Wi-Fi/hostname changes, update the system)
set -u
cd "$(dirname "$0")"

DEVICE=""; USER_NAME=""; REPO=""; LOCAL_BUILD=0; YES=0
while [ $# -gt 0 ]; do
  case "$1" in
    --device) DEVICE="$2"; shift 2 ;;
    --user) USER_NAME="$2"; shift 2 ;;
    --repo) REPO="$2"; shift 2 ;;
    --local-build) LOCAL_BUILD=1; shift ;;
    --yes|-y) YES=1; shift ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

STAGE=/tmp/livemasjid-deploy
SSH_OPTS="-o ConnectTimeout=10 -o ServerAliveInterval=15 -o StrictHostKeyChecking=accept-new"
FAILED=0

# ----------------------------------------------------------------------------- helpers
if [ -t 1 ]; then C=$'\033[36m'; G=$'\033[32m'; R=$'\033[31m'; Y=$'\033[33m'; D=$'\033[2m'; N=$'\033[0m'; else C=""; G=""; R=""; Y=""; D=""; N=""; fi
step() { printf '\n%s[%s] %s%s\n' "$C" "$1" "$2" "$N"; }
info() { printf '    %s\n' "$*"; }
good() { printf '    %sOK%s   %s\n' "$G" "$N" "$*"; }
bad()  { printf '    %sFAIL%s %s\n' "$R" "$N" "$*"; FAILED=$((FAILED + 1)); }
warn() { printf '    %s!%s    %s\n' "$Y" "$N" "$*"; }
die()  { printf '    %sFAIL%s %s\n' "$R" "$N" "$*"; exit 1; }

# ask "Question" "default" -> echoes the answer
ask() {
  local answer
  if [ "$YES" = 1 ]; then echo "$2"; return; fi
  if [ -n "$2" ]; then read -r -p "    $1 [$2]: " answer; else read -r -p "    $1: " answer; fi
  echo "${answer:-$2}"
}
# ask_yes "Question" y|n -> exit status
ask_yes() {
  local answer hint
  if [ "$YES" = 1 ]; then [ "$2" = y ]; return; fi
  if [ "$2" = y ]; then hint="Y/n"; else hint="y/N"; fi
  read -r -p "    $1 ($hint): " answer
  answer="$(printf '%s' "${answer:-$2}" | tr '[:upper:]' '[:lower:]')"
  [ "${answer#y}" != "$answer" ]
}
remote() { ssh $SSH_OPTS -o BatchMode=yes "$TARGET" "$@" 2>/dev/null; }

copy_clipboard() {
  for c in pbcopy clip.exe wl-copy "xclip -selection clipboard" "xsel -b"; do
    if command -v ${c%% *} >/dev/null 2>&1; then printf '%s' "$1" | $c 2>/dev/null && return 0; fi
  done
  return 1
}

# ----------------------------------------------------------------------------- 1. this computer
printf '%sLive Masjid setup wizard%s\nGuide: docs/SETUP.md. Ctrl+C stops at any time; re-running is safe.\n' "$C" "$N"

step 1 "Checking this computer"
for tool in ssh scp curl ssh-keygen; do
  command -v "$tool" >/dev/null 2>&1 || die "'$tool' not found. Windows: run this in Git Bash (git-scm.com) or WSL. Linux: install openssh-client and curl."
done
good "ssh, scp and curl available"
KEY="$HOME/.ssh/id_ed25519"
if [ ! -f "$KEY" ]; then
  info "Creating an SSH key for this computer ($KEY)"
  mkdir -p "$HOME/.ssh" && chmod 700 "$HOME/.ssh"
  ssh-keygen -t ed25519 -N "" -C "livemasjid-setup" -f "$KEY" -q
fi
PUBKEY="$(cat "$KEY.pub")"
good "SSH key $KEY"
if copy_clipboard "$PUBKEY"; then CLIP=" (copied to clipboard)"; else CLIP=""; fi
info "Public key$CLIP. Paste it into Raspberry Pi Imager > OS customisation > Services > 'Allow public-key authentication only':"
printf '    %s%s%s\n' "$D" "$PUBKEY" "$N"

if [ -z "$REPO" ]; then
  REPO="$(git remote get-url origin 2>/dev/null | sed -nE 's#.*github\.com[:/]([^/]+/[^/.]+)(\.git)?$#\1#p')"
fi
REPO="$(ask "GitHub repository for automatic updates (owner/name)" "$REPO")"

# ----------------------------------------------------------------------------- 2. device
step 2 "Finding the device"
info "Flash the SD card first (docs/SETUP.md, step 1), insert it, power the board and wait ~2 minutes."
[ -n "$DEVICE" ] || DEVICE="$(ask "Device hostname or IP address" "raspberrypi.local")"
[ -n "$USER_NAME" ] || USER_NAME="$(ask "Username on the device" "pi")"
TARGET="$USER_NAME@$DEVICE"

reachable() {
  # Any SSH answer (even "permission denied") means the device is up.
  local out; out="$(ssh $SSH_OPTS -o BatchMode=yes "$TARGET" true 2>&1)"
  [ $? -eq 0 ] || printf '%s' "$out" | grep -qiE "permission denied|host key"
}
tries=0
until reachable; do
  tries=$((tries + 1))
  if [ $tries -ge 6 ]; then
    warn "Cannot reach $DEVICE over SSH."
    info "Check: the board is powered and on the same network; SSH was enabled in Raspberry Pi Imager;"
    info "try the IP address from your router's device list instead of the .local name."
    [ "$YES" = 1 ] && die "device unreachable"
    DEVICE="$(ask "Device hostname or IP address" "$DEVICE")"; TARGET="$USER_NAME@$DEVICE"; tries=0
    continue
  fi
  info "Waiting for $DEVICE to answer..."; sleep 10
done
good "$DEVICE answers on SSH"

# A re-flashed card gives the device a new SSH identity (host key), and SSH then refuses to connect
# ("REMOTE HOST IDENTIFICATION HAS CHANGED"). That is expected after re-flashing; otherwise it could
# mean someone is impersonating the device, so ask before forgetting the old key.
changed_host() {
  ssh $SSH_OPTS -o BatchMode=yes "$TARGET" true 2>&1 | grep -q "REMOTE HOST IDENTIFICATION HAS CHANGED"
}
if changed_host; then
  warn "$DEVICE has a new identity (SSH host key). That's normal right after re-flashing its SD card."
  info "If you did NOT just re-flash it, stop here: another machine may be pretending to be it."
  [ "$YES" = 1 ] && die "the device's SSH identity changed; run 'ssh-keygen -R $DEVICE' if it was re-flashed"
  ask_yes "Did you just re-flash this device? Forget its old key" n || die "stopped: the device's identity changed"
  # Forget the name and its address (SSH checks both).
  for h in "$DEVICE" $(ssh $SSH_OPTS -o BatchMode=yes "$TARGET" true 2>&1 | sed -n 's/.*Host key for \([^ ]*\) has changed.*/\1/p'); do
    ssh-keygen -R "$h" >/dev/null 2>&1 || true
  done
  changed_host && die "the old key is still on file; run: ssh-keygen -R $DEVICE"
  good "Old key forgotten; the new one is saved on the next connection"
fi

# ----------------------------------------------------------------------------- 3. key login
step 3 "Password-less login"
if ! remote true; then
  info "Installing this computer's key on the device. Enter the device password ($USER_NAME) when asked:"
  printf '%s\n' "$PUBKEY" | ssh $SSH_OPTS "$TARGET" "umask 077; mkdir -p ~/.ssh; cat >> ~/.ssh/authorized_keys" \
    || die "could not install the key (wrong password?)"
  remote true || die "key login still fails"
fi
good "Key login works"

# ----------------------------------------------------------------------------- 4. inspect
step 4 "Inspecting the device"
MODEL="$(remote "tr -d '\0' < /proc/device-tree/model 2>/dev/null || cat /sys/devices/virtual/dmi/id/product_name 2>/dev/null || echo unknown")"
MACHINE="$(remote "uname -m")"
OS="$(remote '. /etc/os-release; echo "$PRETTY_NAME"')"
IP="$(remote "hostname -I" | awk '{print $1}')"
SSID="$(remote "iwgetid -r 2>/dev/null || nmcli -t -f active,ssid dev wifi 2>/dev/null | sed -n 's/^yes://p'")"
HAS_BT="$(remote "ls /sys/class/bluetooth 2>/dev/null | head -1")"
SUDO_OK="$(remote "sudo -n true 2>/dev/null && echo yes")"
info "Board : $MODEL"
info "CPU   : $MACHINE"
info "OS    : $OS"
info "IP    : ${IP:-?}   Wi-Fi: ${SSID:-(not on Wi-Fi)}   Bluetooth: ${HAS_BT:-none}"
case "$MACHINE" in
  aarch64|arm64) ARCH=aarch64; TRIPLE=aarch64-unknown-linux-musl ;;
  armv7l|armv8l) ARCH=armv7; TRIPLE=armv7-unknown-linux-musleabihf ;;
  armv6l)        ARCH=armv6; TRIPLE=arm-unknown-linux-musleabihf ;;
  *) die "unsupported CPU '$MACHINE' (ARM boards only)" ;;
esac
printf '%s' "$OS" | grep -qiE "debian|raspbian|raspberry|ubuntu|armbian|dietpi" || warn "Untested OS; the installer needs a Debian-based system (apt)."
if [ -n "$IP" ]; then TARGET="$USER_NAME@$IP"; info "Using $IP from here on (survives a hostname change)."; fi

# ----------------------------------------------------------------------------- 5. choices
step 5 "Configuration"
UPGRADE=0
if ask_yes "Update the operating system, kernel, drivers and firmware first? Recommended; 5-20 minutes" y; then UPGRADE=1; fi
COUNTRY="$(ask "Wi-Fi country code (2 letters, e.g. ZA, GB, US; empty = keep)" "")"
WIFI_FILE=""
if ask_yes "Add or change a Wi-Fi network? (the current connection is kept)" n; then
  NEW_SSID="$(ask "Wi-Fi name (SSID)" "")"
  if [ -n "$NEW_SSID" ]; then
    read -r -s -p "    Wi-Fi password: " PSK; echo
    WIFI_FILE="$(mktemp)"; chmod 600 "$WIFI_FILE"
    printf 'SSID=%s\nPSK=%s\n' "$NEW_SSID" "$PSK" > "$WIFI_FILE"; PSK=""
  fi
fi
NEW_HOST="$(ask "Device hostname (empty = keep '$(remote hostname)')" "")"

# ----------------------------------------------------------------------------- 6. release pipeline
step 6 "Release pipeline"
API="https://api.github.com/repos/$REPO"
RELEASE="$(curl -fsSL --max-time 20 -H 'User-Agent: livemasjid-setup' "$API/releases/latest" 2>/dev/null)"
ASSET="livemasjid-$ARCH-linux.tar.gz"
if [ -n "$RELEASE" ]; then
  TAG="$(printf '%s' "$RELEASE" | sed -nE 's/.*"tag_name": *"([^"]+)".*/\1/p' | head -1)"
  WHEN="$(printf '%s' "$RELEASE" | sed -nE 's/.*"published_at": *"([^"]+)".*/\1/p' | head -1)"
  good "Latest release $TAG ($WHEN)"
  if printf '%s' "$RELEASE" | grep -q "\"name\": *\"$ASSET\""; then good "Has a build for this CPU: $ASSET"
  else warn "Release has no $ARCH build yet (it appears after the next push to main)."; LOCAL_BUILD=1; fi
  RUN="$(curl -fsSL --max-time 20 -H 'User-Agent: livemasjid-setup' "$API/actions/runs?per_page=1" 2>/dev/null)"
  CONCLUSION="$(printf '%s' "$RUN" | sed -nE 's/.*"conclusion": *"?([a-z_]+)"?.*/\1/p' | head -1)"
  if [ "$CONCLUSION" = success ]; then good "Last CI run: success"; elif [ -n "$CONCLUSION" ]; then warn "Last CI run: $CONCLUSION (see github.com/$REPO/actions)"; fi
else
  warn "No release found for $REPO (or GitHub unreachable)."; LOCAL_BUILD=1
fi

BINARY=""; VERSION_LABEL=""
if [ "$LOCAL_BUILD" = 1 ]; then
  CARGO="$(command -v cargo || echo "$HOME/.cargo/bin/cargo")"
  [ -x "$CARGO" ] || [ -x "$CARGO.exe" ] || die "Rust isn't installed here and there's no release to download. Install Rust (rustup.rs) or push to main first."
  info "Building locally for $TRIPLE ..."
  "$(dirname "$CARGO")/rustup" target add "$TRIPLE" >/dev/null 2>&1 || true
  VERSION_LABEL="dev-$(git rev-parse --short HEAD 2>/dev/null || echo local)"
  LIVEMASJID_BUILD="$VERSION_LABEL" "$CARGO" build --release --target "$TRIPLE" || die "build failed"
  BINARY="target/$TRIPLE/release/livemasjid"
  good "Built $BINARY"
fi

# ----------------------------------------------------------------------------- 7. install
step 7 "Installing on the device"
remote "rm -rf $STAGE && mkdir -p $STAGE" || die "cannot prepare $STAGE on the device"
# shellcheck disable=SC2086
scp -q $SSH_OPTS deploy/* ${BINARY:+"$BINARY"} "$TARGET:$STAGE/" || die "upload failed"
if [ -n "$WIFI_FILE" ]; then
  scp -q $SSH_OPTS "$WIFI_FILE" "$TARGET:$STAGE/wifi.conf" && remote "chmod 600 $STAGE/wifi.conf"
  rm -f "$WIFI_FILE"
fi
remote "sed -i 's/\r$//' $STAGE/*.sh $STAGE/*.service $STAGE/*.timer $STAGE/CONFIG_VERSION; chmod +x $STAGE/*.sh $STAGE/livemasjid 2>/dev/null; true"

ARGS="--repo $REPO"
[ "$UPGRADE" = 1 ] && ARGS="$ARGS --upgrade"
[ -n "$COUNTRY" ] && ARGS="$ARGS --country $(printf '%s' "$COUNTRY" | tr '[:lower:]' '[:upper:]')"
[ -n "$NEW_HOST" ] && ARGS="$ARGS --hostname $NEW_HOST"
[ -n "$WIFI_FILE" ] && ARGS="$ARGS --wifi-file $STAGE/wifi.conf"
[ -n "$SUDO_OK" ] || info "sudo will ask for the device password once."
LOG="$(mktemp)"
ssh -t $SSH_OPTS "$TARGET" "sudo ${VERSION_LABEL:+VERSION=$VERSION_LABEL} bash $STAGE/install.sh $ARGS" 2>&1 | tee "$LOG"
[ "${PIPESTATUS[0]}" = 0 ] || die "the installer reported an error (see above). Fix it and re-run; it's safe to repeat."

if grep -q "REBOOT REQUIRED" "$LOG"; then
  step 7b "Rebooting into the new kernel and drivers"
  # The SSH session drops when the board goes down; that is expected.
  if [ -n "$SUDO_OK" ]; then remote "sudo -n systemctl reboot" || true
  else ssh -t $SSH_OPTS "$TARGET" "sudo systemctl reboot" || true; fi
  sleep 20
  tries=0; until remote true; do tries=$((tries + 1)); [ $tries -gt 30 ] && die "device didn't come back after reboot"; sleep 5; done
  good "Back online: kernel $(remote "uname -r")"
  sleep 30  # let the service and Bluetooth settle
fi
rm -f "$LOG"

# ----------------------------------------------------------------------------- 8. verify
step 8 "Verifying"
sleep 5
UNITS="$(remote "systemctl is-active livemasjid livemasjid-update.timer livemasjid-netwatch.timer" | tr '\n' ' ')"
if printf '%s' "$UNITS" | grep -qE "inactive|failed|activating"; then bad "services not all active: $UNITS"; else good "service, update timer and network watchdog running"; fi
[ "$(remote "systemctl is-enabled livemasjid")" = enabled ] && good "starts automatically at boot" || bad "service not enabled at boot"
HOST="${IP:-$DEVICE}"
HEALTH="$(curl -fsS --max-time 10 "http://$HOST/api/health" 2>/dev/null)"
[ -n "$HEALTH" ] && good "web UI healthy (version $HEALTH)" || bad "web UI not reachable at http://$HOST/"
CHECK="$(remote "/opt/livemasjid/update.sh --check")"  # read-only; no root needed
if [ -n "$CHECK" ]; then
  val() { printf '%s\n' "$CHECK" | sed -n "s/^$1=//p"; }
  [ "$(val repo)" = "$REPO" ] && good "auto-update from github.com/$REPO (latest $(val latest), installed $(val installed))" || bad "auto-update not configured"
  [ "$(val asset_available)" = yes ] || warn "No $ARCH build in the latest release yet; updates start after the next CI release."
fi
PS="$(remote "iw dev 2>/dev/null | awk '/Interface/ {print \$2; exit}' | xargs -r -I{} iw dev {} get power_save")"
[ -n "$PS" ] && { printf '%s' "$PS" | grep -q off && good "Wi-Fi power saving off" || bad "Wi-Fi power saving still on"; }
if [ -n "$HAS_BT" ]; then
  remote "bluetoothctl show" | grep -q "Powered: yes" && good "Bluetooth powered" || bad "Bluetooth adapter not powered"
  BA="$(remote "systemctl is-active bluealsa 2>/dev/null || systemctl is-active bluealsad 2>/dev/null")"
  [ "$BA" = active ] && good "Bluetooth audio (bluez-alsa) running" || bad "bluez-alsa not running"
fi
UV="$(remote "cat /sys/devices/platform/soc/soc:firmware/get_throttled 2>/dev/null")"
if printf '%s' "$UV" | grep -qE '^0x[0-9a-fA-F]+$' && [ $(( UV & 0x10001 )) -ne 0 ]; then warn "Under-voltage detected: use a proper 5 V power supply (weak power causes Wi-Fi/Bluetooth drop-outs)."; fi

# ----------------------------------------------------------------------------- 9. lock down
step 9 "Lock the device down"
info "SSH sign-in with your key only, a password for administrator (sudo) commands, a firewall that"
info "admits only your home network, and automatic security updates. See docs/SETUP.md > Lock it down."
if remote "test -x /opt/livemasjid/harden.sh" && ask_yes "Lock it down now? Recommended (sudo may ask for the device password)" y; then
  if ssh -t $SSH_OPTS "$TARGET" "sudo /opt/livemasjid/harden.sh \$(id -un)"; then good "device locked down"
  else bad "harden.sh reported a problem (see above); it's safe to run again: sudo /opt/livemasjid/harden.sh"; fi
else
  info "Skipped. Later, on the device: sudo /opt/livemasjid/harden.sh"
fi

# ----------------------------------------------------------------------------- done
URL="http://$HOST/"
echo
if [ "$FAILED" = 0 ]; then printf '%sAll set! Open %s%s\n' "$G" "$URL" "$N"
else printf '%s%s check(s) failed - see docs/SETUP.md > Troubleshooting. UI: %s%s\n' "$Y" "$FAILED" "$URL" "$N"; fi
info "Next: Masjids > pick yours. Speaker > put your Bluetooth speaker in pairing mode > Scan > Pair."
[ -n "$NEW_HOST" ] && info "The device is also reachable as http://$NEW_HOST.local/"
exit "$FAILED"
