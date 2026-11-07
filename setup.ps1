<#
.SYNOPSIS
  Guided setup of Live Masjid on a new single-board computer, from this Windows PC.

.DESCRIPTION
  Walks through everything needed to turn a freshly flashed board into an always-on
  masjid radio: SSH key, Wi-Fi, hostname, Bluetooth/audio configuration, the service,
  auto-updates from GitHub releases, and a final health check.

  Supported: Raspberry Pi (Zero/Zero 2 W/2/3/4/5) on Raspberry Pi OS, and other Debian-based
  boards (Armbian, DietPi, Ubuntu) — 64-bit (aarch64) and 32-bit (armv7, armv6).
  Step-by-step guide with screenshots-in-words: docs/SETUP.md

.EXAMPLE
  ./setup.ps1                                   # fully interactive
  ./setup.ps1 -Device 192.168.1.50 -User pi     # skip the first questions
  ./setup.ps1 -LocalBuild                       # install a build from this PC instead of the latest release
#>
param(
  [string]$Device = "",
  [string]$User = "",
  [string]$Repo = "",
  [switch]$LocalBuild
)
$ErrorActionPreference = "Continue"   # native tools write progress to stderr; we check exit codes
Set-Location $PSScriptRoot
$Stage = "/tmp/livemasjid-deploy"

# ----------------------------------------------------------------------------- helpers
function Step($n, $text) { Write-Host ""; Write-Host "[$n] $text" -ForegroundColor Cyan }
function Info($text) { Write-Host "    $text" }
function Good($text) { Write-Host "    OK   $text" -ForegroundColor Green }
function Bad($text) { Write-Host "    FAIL $text" -ForegroundColor Red }
function Warn($text) { Write-Host "    !    $text" -ForegroundColor Yellow }

function Ask($question, $default = "") {
  $suffix = if ($default) { " [$default]" } else { "" }
  $answer = Read-Host "    $question$suffix"
  if ([string]::IsNullOrWhiteSpace($answer)) { return $default }
  return $answer.Trim()
}

function AskYes($question, $defaultYes = $true) {
  $hint = if ($defaultYes) { "Y/n" } else { "y/N" }
  $answer = Read-Host "    $question ($hint)"
  if ([string]::IsNullOrWhiteSpace($answer)) { return $defaultYes }
  return $answer.Trim().ToLower().StartsWith("y")
}

function AskSecret($question) {
  $secure = Read-Host "    $question" -AsSecureString
  $bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
  try { return [Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr) }
  finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }
}

function Test-Port($hostName, $port, $timeoutMs = 3000) {
  try {
    $client = New-Object System.Net.Sockets.TcpClient
    $task = $client.ConnectAsync($hostName, $port)
    $ok = $task.Wait($timeoutMs) -and $client.Connected
    $client.Close()
    return $ok
  } catch { return $false }
}

# Run a command on the device (non-interactive). Returns trimmed stdout; $LASTEXITCODE is set.
function Remote($cmd) {
  $out = ssh -o BatchMode=yes -o ConnectTimeout=10 "$script:Target" $cmd 2>$null
  return ($out | Out-String).Trim()
}

# ----------------------------------------------------------------------------- 1. this PC
Write-Host "Live Masjid setup wizard" -ForegroundColor White
Write-Host "Guide: docs/SETUP.md. Press Ctrl+C at any time to stop; re-running is safe."

Step 1 "Checking this PC"
if (-not (Get-Command ssh -ErrorAction SilentlyContinue)) {
  Bad "OpenSSH client not found. Install it: Settings > System > Optional features > OpenSSH Client."
  exit 1
}
Good "OpenSSH client"
$keyPath = Join-Path $env:USERPROFILE ".ssh\id_ed25519"
if (-not (Test-Path $keyPath)) {
  Info "Creating an SSH key for this PC ($keyPath)"
  New-Item -ItemType Directory -Force (Split-Path $keyPath) | Out-Null
  ssh-keygen -t ed25519 -N '""' -C "livemasjid-setup" -f $keyPath -q
}
$pubKey = (Get-Content "$keyPath.pub" -Raw).Trim()
Good "SSH key $keyPath"
try { Set-Clipboard -Value $pubKey; $clip = " (copied to clipboard)" } catch { $clip = "" }
Info "Public key$clip - paste it into Raspberry Pi Imager > OS customisation > Services > 'Allow public-key authentication only':"
Write-Host "    $pubKey" -ForegroundColor DarkGray

if (-not $Repo) {
  $origin = git remote get-url origin 2>$null
  if ($origin -match "github\.com[:/](?<r>[^/]+/[^/.]+)") { $Repo = $Matches.r }
}
$Repo = Ask "GitHub repository for automatic updates (owner/name)" $Repo

# ----------------------------------------------------------------------------- 2. device
Step 2 "Finding the device"
Info "Flash the SD card first (docs/SETUP.md, step 1), insert it, power the board and wait ~2 minutes."
if (-not $Device) { $Device = Ask "Device hostname or IP address" "raspberrypi.local" }
if (-not $User) { $User = Ask "Username on the device" "pi" }
$script:Target = "$User@$Device"

$tries = 0
while (-not (Test-Port $Device 22)) {
  $tries++
  if ($tries -ge 3) {
    Bad "Cannot reach $Device on port 22 (SSH)."
    Info "Check: the board is powered and on the same network; SSH was enabled in Raspberry Pi Imager;"
    Info "try the IP address from your router's device list instead of the .local name."
    $Device = Ask "Device hostname or IP address (or Ctrl+C to stop)" $Device
    $script:Target = "$User@$Device"
    $tries = 0
    continue
  }
  Info "Waiting for $Device to answer on SSH..."
  Start-Sleep -Seconds 10
}
Good "$Device answers on SSH"

# ----------------------------------------------------------------------------- 3. key login
Step 3 "Password-less login"
$null = Remote "true"
if ($LASTEXITCODE -ne 0) {
  Info "Installing this PC's key on the device. Enter the device password ($User) when asked:"
  $pubKey | ssh -o ConnectTimeout=10 $script:Target "umask 077; mkdir -p ~/.ssh; cat >> ~/.ssh/authorized_keys"
  $null = Remote "true"
  if ($LASTEXITCODE -ne 0) { Bad "Key login still fails. Check the username/password and try again."; exit 1 }
}
Good "Key login works"

# ----------------------------------------------------------------------------- 4. inspect
Step 4 "Inspecting the device"
$model = Remote "tr -d '\0' < /proc/device-tree/model 2>/dev/null || cat /sys/devices/virtual/dmi/id/product_name 2>/dev/null || echo unknown"
$machine = Remote "uname -m"
$os = Remote ". /etc/os-release; echo `$PRETTY_NAME"
$ip = (Remote "hostname -I").Split(" ")[0]
$ssid = Remote "iwgetid -r 2>/dev/null || nmcli -t -f active,ssid dev wifi 2>/dev/null | sed -n 's/^yes://p'"
$hasBt = Remote "ls /sys/class/bluetooth 2>/dev/null | head -1"
$sudoOk = Remote "sudo -n true 2>/dev/null && echo yes"
Info "Board : $model"
Info "CPU   : $machine"
Info "OS    : $os"
Info "IP    : $ip   Wi-Fi: $(if ($ssid) { $ssid } else { '(not on Wi-Fi)' })   Bluetooth: $(if ($hasBt) { $hasBt } else { 'none' })"
$arch = switch -Regex ($machine) {
  "^(aarch64|arm64)$" { "aarch64"; break }
  "^(armv7l|armv8l)$" { "armv7"; break }
  "^armv6l$"          { "armv6"; break }
  default { "" }
}
if (-not $arch) { Bad "Unsupported CPU '$machine' (ARM boards only)."; exit 1 }
if ($os -notmatch "Debian|Raspbian|Raspberry|Ubuntu|Armbian|DietPi") { Warn "Untested OS; the installer needs a Debian-based system (apt)." }
if ($ip) { $script:Target = "$User@$ip"; Info "Using $ip from here on (survives a hostname change)." }

# ----------------------------------------------------------------------------- 5. choices
Step 5 "Configuration"
$country = Ask "Wi-Fi country code (2 letters, e.g. ZA, GB, US; empty = keep)" ""
$wifiFile = $null
if (AskYes "Add or change a Wi-Fi network? (the current connection is kept)" $false) {
  $newSsid = Ask "Wi-Fi name (SSID)" ""
  if ($newSsid) {
    $psk = AskSecret "Wi-Fi password"
    $wifiFile = Join-Path $env:TEMP "livemasjid-wifi.conf"
    Set-Content -Path $wifiFile -Value "SSID=$newSsid`nPSK=$psk" -NoNewline -Encoding ascii
    $psk = $null
  }
}
$newHost = Ask "Device hostname (empty = keep '$(Remote 'hostname')')" ""

# ----------------------------------------------------------------------------- 6. release
Step 6 "Release pipeline"
$useLocal = [bool]$LocalBuild
$release = $null
try {
  $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ "User-Agent" = "livemasjid-setup" } -TimeoutSec 20
  $asset = $release.assets | Where-Object { $_.name -eq "livemasjid-$arch-linux.tar.gz" }
  $published = ([datetime]$release.published_at).ToLocalTime()
  Good "Latest release $($release.tag_name) ($published)"
  if ($asset) { Good "Has a build for this CPU: $($asset.name)" }
  else { Warn "Release has no $arch build yet (it appears after the next push to main)."; $useLocal = $true }
  try {
    $runs = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/actions/runs?per_page=1" -Headers @{ "User-Agent" = "livemasjid-setup" } -TimeoutSec 20
    $run = $runs.workflow_runs | Select-Object -First 1
    if ($run.conclusion -eq "success") { Good "Last CI run: success ($($run.head_sha.Substring(0,7)))" }
    elseif ($run) { Warn "Last CI run: $($run.status) / $($run.conclusion) - $($run.html_url)" }
  } catch { Warn "Could not read CI status." }
} catch {
  Warn "No release found for $Repo (or GitHub unreachable)."
  $useLocal = $true
}

$binary = $null
$version = ""
if ($useLocal) {
  $target = @{ aarch64 = "aarch64-unknown-linux-musl"; armv7 = "armv7-unknown-linux-musleabihf"; armv6 = "arm-unknown-linux-musleabihf" }[$arch]
  $cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
  if (-not (Test-Path $cargo)) { Bad "Rust is not installed on this PC, and there is no release to download. Install Rust (rustup) or push to main first."; exit 1 }
  Info "Building locally for $target ..."
  $sha = git rev-parse --short HEAD 2>$null
  $version = "dev-$sha"
  $env:LIVEMASJID_BUILD = $version
  & (Join-Path $env:USERPROFILE ".cargo\bin\rustup.exe") target add $target 2>$null | Out-Null
  & $cargo build --release --target $target
  Remove-Item Env:LIVEMASJID_BUILD
  if ($LASTEXITCODE -ne 0) { Bad "Build failed"; exit 1 }
  $binary = "target/$target/release/livemasjid"
  Good "Built $binary"
}

# ----------------------------------------------------------------------------- 7. install
Step 7 "Installing on the device"
$null = Remote "rm -rf $Stage && mkdir -p $Stage"
$files = @(Get-ChildItem deploy -File | ForEach-Object { "deploy/$($_.Name)" })
if ($binary) { $files += $binary }
scp -q @files "$($script:Target):$Stage/"
if ($LASTEXITCODE -ne 0) { Bad "Upload failed"; exit 1 }
if ($wifiFile) {
  scp -q $wifiFile "$($script:Target):$Stage/wifi.conf"
  Remove-Item $wifiFile -Force
  $null = Remote "chmod 600 $Stage/wifi.conf"
}
$null = Remote "sed -i 's/\r$//' $Stage/*.sh $Stage/*.service $Stage/*.timer $Stage/CONFIG_VERSION; chmod +x $Stage/*.sh $Stage/livemasjid 2>/dev/null; true"

$installArgs = @("--repo", $Repo)
if ($country) { $installArgs += @("--country", $country.ToUpper()) }
if ($newHost) { $installArgs += @("--hostname", $newHost) }
if ($wifiFile) { $installArgs += @("--wifi-file", "$Stage/wifi.conf") }
$envPrefix = if ($version) { "VERSION=$version " } else { "" }
if (-not $sudoOk) { Info "sudo will ask for the device password once." }
ssh -t $script:Target "sudo $envPrefix bash $Stage/install.sh $($installArgs -join " ")"
if ($LASTEXITCODE -ne 0) { Bad "Installer reported an error (see above). Fix it and re-run this wizard; it's safe to repeat."; exit 1 }

# ----------------------------------------------------------------------------- 8. verify
Step 8 "Verifying"
Start-Sleep -Seconds 5
$failed = 0
function Check($ok, $okText, $badText) {
  if ($ok) { Good $okText } else { Bad $badText; $script:failed++ }
}
$units = Remote "systemctl is-active livemasjid livemasjid-update.timer livemasjid-netwatch.timer"
Check ($units -notmatch "inactive|failed") "service, update timer and network watchdog running" "service/timers not all active: $($units -replace "`n", ' ')"
Check ((Remote "systemctl is-enabled livemasjid") -eq "enabled") "starts automatically at boot" "service not enabled at boot"
$healthHost = if ($ip) { $ip } else { $Device }
try {
  $health = Invoke-WebRequest -Uri "http://$healthHost/api/health" -UseBasicParsing -TimeoutSec 10
  Check ($health.StatusCode -eq 200) "web UI healthy (version $($health.Content))" "health check failed"
} catch { Check $false "" "web UI not reachable at http://$healthHost/" }
$upd = Remote "sudo -n /opt/livemasjid/update.sh --check 2>/dev/null"
if ($upd) {
  $kv = @{}; $upd -split "`n" | ForEach-Object { $p = $_.Split("=", 2); if ($p.Count -eq 2) { $kv[$p[0].Trim()] = $p[1].Trim() } }
  Check ($kv["repo"] -eq $Repo) "auto-update source github.com/$($kv['repo']) (latest $($kv['latest']), installed $($kv['installed']))" "auto-update not configured"
  if ($kv["asset_available"] -ne "yes") { Warn "No $arch build in the latest release yet; updates start after the next CI release." }
}
$ps = Remote "iw dev 2>/dev/null | awk '/Interface/ {print `$2; exit}' | xargs -r -I{} iw dev {} get power_save"
if ($ps) { Check ($ps -match "off") "Wi-Fi power saving off" "Wi-Fi power saving still on" }
if ($hasBt) {
  $bt = Remote "bluetoothctl show | grep -E 'Powered:'"
  Check ($bt -match "yes") "Bluetooth powered" "Bluetooth adapter not powered ($bt)"
  $ba = Remote "systemctl is-active bluealsa 2>/dev/null || systemctl is-active bluealsad 2>/dev/null"
  Check ($ba -eq "active") "Bluetooth audio (bluez-alsa) running" "bluez-alsa not running"
}

# ----------------------------------------------------------------------------- done
Write-Host ""
$url = "http://$healthHost/"
if ($failed -eq 0) { Write-Host "All set! Open $url" -ForegroundColor Green }
else { Write-Host "$failed check(s) failed - see docs/SETUP.md > Troubleshooting. UI: $url" -ForegroundColor Yellow }
Info "Next: Masjids > pick yours. Speaker > put your Bluetooth speaker in pairing mode > Scan > Pair."
if ($newHost) { Info "After the next reboot the device is also reachable as http://$newHost.local/" }
if (AskYes "Open the web UI now?" $true) { Start-Process $url }
