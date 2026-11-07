<#
.SYNOPSIS
  Build livemasjid for the Raspberry Pi Zero 2 W and ship it over SSH.

.DESCRIPTION
  Cross-compiles a static aarch64 binary with the Rust toolchain only (rust-lld + musl,
  no Docker, no C toolchain), copies it to the Pi and installs it with automatic
  health-check + rollback.

.EXAMPLE
  ./deploy.ps1                 # build + deploy (normal development loop)
  ./deploy.ps1 -Setup          # re-run the full installer with this local build
  (For a brand-new device use ./setup.ps1 — the guided wizard.)
  ./deploy.ps1 -Logs           # follow the service log
#>
param(
  [string]$PiHost = "pi@192.168.1.101",
  [switch]$Setup,
  [string]$Repo = "",
  [switch]$Logs
)
$ErrorActionPreference = "Continue"  # native tools write progress to stderr; we check $LASTEXITCODE instead
Set-Location $PSScriptRoot

if ($Logs) { ssh -t $PiHost "journalctl -u livemasjid -f -n 100"; exit $LASTEXITCODE }

$cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
if (-not (Test-Path $cargo)) { $cargo = "cargo" }

$sha = git rev-parse --short HEAD 2>$null
if ($LASTEXITCODE -ne 0 -or -not $sha) { $sha = "local" }
$dirty = if (git status --porcelain 2>$null) { "-dirty" } else { "" }
$version = "dev-$sha$dirty"
$env:LIVEMASJID_BUILD = $version

$machine = (ssh -o ConnectTimeout=8 $PiHost "uname -m" 2>$null | Out-String).Trim()
$target = switch -Regex ($machine) {
  "^(aarch64|arm64)$" { "aarch64-unknown-linux-musl"; break }
  "^(armv7l|armv8l)$" { "armv7-unknown-linux-musleabihf"; break }
  "^armv6l$"          { "arm-unknown-linux-musleabihf"; break }
  default { throw "Cannot reach $PiHost or unsupported CPU '$machine'. Use setup.ps1 for a new device." }
}
Write-Host "==> Building $version for $machine ($target)" -ForegroundColor Cyan
& $cargo build --release --target $target
if ($LASTEXITCODE -ne 0) { throw "build failed" }
Remove-Item Env:LIVEMASJID_BUILD

$bin = "target/$target/release/livemasjid"
$stage = "/tmp/livemasjid-deploy"
Write-Host "==> Uploading to $PiHost" -ForegroundColor Cyan
ssh $PiHost "rm -rf $stage && mkdir -p $stage"
if ($LASTEXITCODE -ne 0) { throw "ssh failed" }
$files = @($bin) + (Get-ChildItem deploy -File | ForEach-Object { "deploy/$($_.Name)" })
scp -q @files "${PiHost}:$stage/"
if ($LASTEXITCODE -ne 0) { throw "scp failed" }
# Guard against CRLF line endings sneaking into shell scripts / units.
ssh $PiHost "sed -i 's/\r$//' $stage/*.sh $stage/*.service $stage/*.timer && chmod +x $stage/*.sh $stage/livemasjid"

if ($Setup) {
  $repoArg = if ($Repo) { "--repo $Repo" } else { "" }
  Write-Host "==> Running first-time setup (sudo password may be requested)" -ForegroundColor Cyan
  ssh -t $PiHost "sudo VERSION=$version bash $stage/install.sh $repoArg"
} else {
  ssh $PiHost "sudo -n /opt/livemasjid/install-bin.sh $stage/livemasjid $version"
  if ($LASTEXITCODE -ne 0) {
    Write-Host "Deploy failed. If this is the first deploy, run: ./deploy.ps1 -Setup" -ForegroundColor Yellow
    exit 1
  }
}
$ip = $PiHost.Split("@")[-1]
Write-Host "==> Done: http://$ip/" -ForegroundColor Green
