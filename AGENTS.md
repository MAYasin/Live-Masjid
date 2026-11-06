# Live Masjid Pi — project memory

An always-on Rust service for a **Raspberry Pi Zero 2 W**. It auto-plays a selected
[livemasjid.com](https://www.livemasjid.com) stream whenever that masjid goes live, and has a web UI for management.
The user-facing documentation is in `README.md`. This file records the decisions, verified facts and gotchas
that aren't obvious from the code.

## Product decisions (agreed with the owner)

- **Management UI.** A web UI served by the Pi, LAN only for now, with an optional password. No native GUI.
- **Selection.** One masjid at a time, not a priority list.
- **Audio output.** A Bluetooth speaker first, with a wired fallback (USB, then HDMI only when a display is connected).
- **Shipping.**
  - GitHub Actions builds a release on every push to `main`.
  - The Pi pulls releases hourly through `livemasjid-update.timer`, health-checks them and rolls back on failure.
  - `deploy.ps1` is the local development loop.
- **No Docker anywhere.** The owner explicitly rejected it.
- **UI style.** Minimal and neutral, like a FAANG product: grayscale plus one dark primary colour, green only for "live",
  6 px radii, flat 1 px borders, no gradients or glows. It must not scroll sideways at 375 px. The sidebar appears at 900 px and wider.
- **Footprint matters.** In testing it used about 4 MB of RAM and 1–2% CPU while playing, as a 3 MB static binary.

## Target device

- **Address:** `pi@192.168.1.101` (also `pi.local`).
- **OS:** Raspberry Pi OS Lite 64-bit Trixie (Debian 13), aarch64, 415 MB RAM.
- **Access:** SSH key auth from the dev PC (`~/.ssh/id_ed25519`). Passwordless sudo for `pi` was enabled by the owner.
- **Installed layout:**
  - `/opt/livemasjid/{bin/livemasjid,VERSION,update.sh,install-bin.sh,netwatch.sh}`
  - data in `/var/lib/livemasjid`
  - port in `/etc/default/livemasjid` (80)
  - `/etc/livemasjid/update.conf` (`REPO=MAYasin/Live-Masjid`)
- **Service:** `livemasjid.service` runs as the `livemasjid` user (groups `audio`, `bluetooth`), with `Type=notify`,
  `WatchdogSec=45`, `Restart=always`, and `ExecStartPre=+-/usr/sbin/rfkill unblock bluetooth`.

## Build, test, deploy

- **Build:** `cargo build --release --target aarch64-unknown-linux-musl`. This is pure Rust, linked with `rust-lld`
  (`.cargo/config.toml`), so no C toolchain or zig is needed on Windows.
- **The `tls` feature** (rustls + ring) needs a C compiler, so only CI enables it. Local builds talk plain HTTP, which every endpoint supports.
- **Host `cargo test` fails on Windows** (`dlltool.exe` not found, windows-gnu toolchain). Run the tests on the Pi instead:
  `cargo test --release --no-run --target aarch64-unknown-linux-musl`, then copy the `target/.../release/deps/livemasjid-*` binary over and run it.
- **Deploy:** `./deploy.ps1` (dev loop) or `./deploy.ps1 -Setup -Repo MAYasin/Live-Masjid` (idempotent full install).
  - Dev builds are labelled `dev-<sha>`.
  - `update.sh` won't replace a dev build for 12 hours unless run with `--force`.
- **PowerShell 5.1 treats native stderr as an error.** `deploy.ps1` uses `$ErrorActionPreference="Continue"` and checks `$LASTEXITCODE` instead.
- **Over SSH, never run `pkill -f livemasjid`.** It matches the SSH command line and kills your own session. Use `pkill -x livemasjid`.
- **Shell scripts must keep LF line endings** (`.gitattributes`). `deploy.ps1` also strips CR on the Pi as a safety net.

## livemasjid.com — verified behaviour (Nov 2026)

- **Live push (SSE):** `www.livemasjid.com/stateQuery.php?mount=<m>`.
  - While the mount is offline it sends nothing at all, not even headers.
  - When it goes live it sends repeated `data: {"msg":"online","id":"<m>"}` events, with the JSON split across several `data:` lines, then closes.
  - The site's own JavaScript closes its EventSource after "online". **We only hold SSE while in the `Waiting` phase.**
    Reconnecting in a loop while live is what got us blocked.
- **Status:** Icecast `status-json.xsl` on the origin `http://livemasjid.com:8000`, then the relay `http://relay2.livemasjid.com:8000`
  (a subset of mounts), then `www/api/status-json.xsl`.
  - A mount is live when its `source` has `stream_start`.
  - `source` can be a single object instead of an array.
  - The site's page polls every 60 s. **Our default is 20 s, with jitter and back-off on 403/429/refused.**
- **Streams:**
  - `/<m>` is Ogg **Opus**, mono 48 kHz, and goes live about 6 s before `/<m>.mp3`. Some mounts are MP3-only without the suffix.
  - Icecast sends a burst of about 20 s on connect, so the first audio arrives about 0.4 s after connecting.
- **Icecast resets connections whose HTTP request arrives in fragments.** Always send the request in a single `write_all` (see `net.rs`).
- **The TLS chains on `relay.livemasjid.com:8443` and `livemasjid.com:8443` are incomplete** and fail on the Pi. Use plain HTTP on :8000.
- **There is no JSON directory API.** The masjid list is scraped from `index.php` cards (`catalog::parse`), about 471 entries
  across the Masjid, Institution and Aalim categories. A snapshot is embedded as `src/catalog_seed.json`. To regenerate it, run
  `livemasjid --parse-catalog index.html > src/catalog_seed.json`.
- **We got rate-limited during development** (aggressive polling plus the SSE reconnect loop). `www` returned 403 and the origin
  refused connections from the home IP. relay2 kept working and the app played through it.
  - **Do not work around a block by spoofing a browser User-Agent.** Be polite, and contact the livemasjid team if needed.
  - Keep the request budget small: at most one status poll per 20 s, SSE only while waiting, and a direct probe only after 2 minutes blind.

## Pi hardware gotchas (root causes found)

- **"The Pi went down" turned out to be a Wi-Fi drop-out, not a crash.**
  - Evidence: no undervoltage (`vcgencmd get_throttled` = 0x0), and the hardware watchdog would have rebooted a hung kernel within a minute.
  - Cause: brcmfmac (BCM43430) power save was on and the signal is about -64 dBm.
  - Fixes:
    - power save off: NetworkManager `wifi.powersave = 2`, per-connection setting, and `iw`
    - `netwatch.sh` timer every 30 s: reconnect, then restart NetworkManager, then reload the driver, then reboot after about 10 minutes offline
    - `RuntimeWatchdogSec=15s`
    - persistent journal
- **Bluetooth scan "not working" was rfkill, not drivers.** The adapter was soft-blocked, `bluetoothd` logged
  `Failed to set mode: Failed (0x03)`, and the firmware (`BCM43430B0 ... zero-2-w.hcd`) loaded fine. It is now unblocked
  at every service start, and the app also self-heals through a narrow sudo rule.
- **HDMI audio without a display fails** with ALSA error 524. The output list skips HDMI unless `/sys/class/drm/*HDMI*/status` reads `connected`.
- **Audio goes through an `aplay` child process.** Bluetooth uses `bluealsa:DEV=<MAC>,PROFILE=a2dp` from Debian's `bluez-alsa-utils`
  (`bluealsa.service`). While waiting, the sink feeds silence ("keep awake") so playback starts instantly.

## Open issues / next steps

- **Not yet verified end to end:** real playback through a paired Bluetooth speaker, auto-reconnect after a speaker or Pi power cycle,
  and the full GitHub Actions to Pi update path (the first push happens when this file is committed).
- **Intermittent SSH/scp "connection reset"** during uploads around 00:06–00:14 on 2026-11-07. Ping stayed clean and it cleared up
  by itself; netwatch logged one failed check. Watch for a recurrence. The Wi-Fi signal is marginal (-64 dBm, 2.4 GHz only on the Zero 2 W),
  so moving the Pi or the access point closer may help.
- **The livemasjid IP block** may still be in place. The app works through relay2 meanwhile.
- **Possible additions:** a password-protected remote-access option (Tailscale was suggested), and multiple masjids with a priority order.

## Code map

`main.rs` wiring and the systemd watchdog · `player.rs` state machine (signals to sessions, stop and suppression, probe) ·
`stream.rs` connect, decode (opus-pure / symphonia MP3) and reconnect grace · `sink.rs` aplay output thread (epochs, keep-awake, failover) ·
`monitor.rs` SSE and status poll with back-off · `bluetooth.rs` bluetoothctl supervisor and output targets · `catalog.rs` directory scrape,
cache and seed · `history.rs` JSONL sessions, persisted every minute · `web.rs` axum API, auth and SSE to the UI · `ui/index.html` the whole UI.
