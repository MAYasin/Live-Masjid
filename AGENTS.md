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
  - `deploy.sh` is the local development loop.
- **No Docker anywhere.** The owner explicitly rejected it.
- **Setup of new devices** is a bash wizard, `setup.sh` (macOS, Linux, Windows via Git Bash/WSL; bash 3.2-compatible for macOS), driving the generic `deploy/install.sh` over SSH, with a guide in `docs/SETUP.md`.
  It targets any Pi on Pi OS plus Debian-based ARM boards, 64-bit and 32-bit (aarch64, armv7, armv6).
- **Audio: keep Bluetooth and wired both supported.** The owner currently has only a Bluetooth speaker, so Bluetooth must be rock-solid.
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

- **Build:** `cargo build --release --target <t>`, where `<t>` is `aarch64-unknown-linux-musl`, `armv7-unknown-linux-musleabihf` or
  `arm-unknown-linux-musleabihf` (ARMv6). This is pure Rust, linked with `rust-lld` (`.cargo/config.toml`), so no C toolchain or zig
  is needed. The Pi Zero 2 W can run the 32-bit binaries too, which is handy for testing them.
- **CI** (`release.yml`): a matrix over the three targets on `ubuntu-24.04-arm`; 32-bit tests run under `qemu-arm-static`. One release
  holds `livemasjid-{aarch64,armv7,armv6}-linux.tar.gz`, and `update.sh` picks the asset by `uname -m`.
- **`deploy/CONFIG_VERSION`:** bump it whenever system configuration changes in `install.sh`. `update.sh` then runs
  `install.sh --skip-binary` once on every device.
- **`install.sh` must read `/etc/os-release` in a subshell.** That file defines its own `VERSION=`, which once clobbered the build label.
- **The `tls` feature** (rustls + ring) needs a C compiler, so only CI enables it. Local builds talk plain HTTP, which every endpoint supports.
- **Host `cargo test` fails on Windows** (`dlltool.exe` not found, windows-gnu toolchain). Run the tests on the Pi instead:
  `cargo test --release --no-run --target aarch64-unknown-linux-musl`, then copy the `target/.../release/deps/livemasjid-*` binary over and run it.
- **Deploy:** `./deploy.sh` (dev loop; picks the target from the device's `uname -m`) or `./deploy.sh --setup --repo MAYasin/Live-Masjid`
  (idempotent full install). The PowerShell scripts were replaced by these on 2026-11-07.
  - Dev builds are labelled `dev-<sha>`.
  - `update.sh` won't replace a dev build for 12 hours unless run with `--force`.
- **Over SSH, never run `pkill -f livemasjid`.** It matches the SSH command line and kills your own session. Use `pkill -x livemasjid`.
- **Shell scripts must keep LF line endings** (`.gitattributes`). `deploy.sh` and `setup.sh` also strip CR on the device as a safety net.

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
- **We got the home IP banned by the site owner (Cloudflare error 1006, 2026-11-06).** The cause was our development traffic:
  4 s polling plus the SSE reconnect loop. `www` returns 403 and the origin :8000 refuses connections; relay2 keeps working.
  - **Never work around a block** with a spoofed User-Agent, a VPN or a proxy. Only the site owner can lift it; the owner was asked to contact them.
  - `site.rs` (`SiteGuard`): any 403/429 from `www` pauses **all** website traffic (SSE, directory, www status) for 6 h, then 12 h, then 24 h.
    The back-off is saved to `site.json`, so restarts don't reset it.
    The UI shows a notice. Playback needs only the Icecast servers, so it is unaffected.
  - Request budget:
    - status poll at most once per 20 s with jitter (to the Icecast servers, not the website)
    - SSE only while `Waiting`, and at least 15 s between reconnects
    - directory refresh monthly; after a failure, 6 h doubling up to 7 days
    - direct probe only after 2 minutes with no signal

## Third-party integrations (`src/integrations/`)

- **All fetching happens on the Pi, through `curl`** (system TLS works in every build), with an honest User-Agent, one request per site
  at a time and TTL caches. Browsing the UI therefore never adds load to the sites. A 403/429 trips that site's `SiteGuard`
  (persisted: `site.json` for livemasjid, `site-masjidscreen.json`).
- **Salah times:** `https://masjidscreen.co.za/<mount>/api/prayertimes.php` returns `salaah_times.{fajr,zuhr,asr,maghrib,esha,jummah}.{athaan,jamaat}`.
  Unknown masjids return 404, which shows as "not_listed" (cached 24 h). Successes are cached 6 h and re-fetched when the date changes.
  The UI highlights the next athaan or jamaat with a countdown, and shows Jumu'ah instead of Zuhr on Fridays.
- **Recordings:** the RSS feed at `https://www.livemasjid.com/api/get_mountrecordings.php?mountname=<m>&feeditems=40` (iso-8859-1,
  single-quoted `enclosure url=` on `cdn.livemasjid.com` .mp3, no durations), cached 15 min. Override it with `LIVEMASJID_RECORDINGS_FEED`
  (`{mount}`, `{n}`) for testing. **The livemasjid IP block was lifted by 2026-11-07 15:40** (the feed returns 200 again).
- **Recording playback** (`stream/recording.rs`): `curl` → `ByteQueue` → the shared decoder, with pause/position on `DecodeCtx`.
  The API plays by **id from the cached list, never a raw URL** (no SSRF). A broadcast that starts during a recording interrupts it;
  stopping or finishing a recording resumes live if the masjid is on air. Verified: play, pause (position held), resume, stop, then back to live.

## Pi hardware gotchas (root causes found)

- **"The Pi went down" turned out to be a Wi-Fi drop-out, not a crash.**
  - Evidence: no undervoltage (`vcgencmd get_throttled` = 0x0), and the hardware watchdog would have rebooted a hung kernel within a minute.
  - Cause: brcmfmac (BCM43430) power save was on and the signal is about -64 dBm.
  - Fixes:
    - power save off: NetworkManager `wifi.powersave = 2`, per-connection setting, and `iw`
    - `netwatch.sh` timer every 30 s: reconnect, then restart NetworkManager, then reload the driver, then reboot after about 10 minutes offline
    - `RuntimeWatchdogSec=15s`
    - persistent journal
- **Bluetooth has two failure modes that both look like "Powered: no":**
  1. **rfkill soft-block** (Pi OS default). `bluetoothd` logs `Failed to set mode: Failed (0x03)`. It is unblocked at every service start
     (`ExecStartPre`) and by the app through a narrow sudo rule.
  2. **Hung controller.** The UART-attached BCM43430 stops answering: the kernel logs `hci0: Opcode 0x0c03 failed: -110`, and `bluetoothd` logs
     `Failed to set mode: Authentication Failed (0x05)` every few seconds. It happened while Wi-Fi was saturated, since the chip is shared.
     The fix is to re-bind the `hci_uart_bcm` serdev (`serial0-0`), which re-uploads the firmware. `deploy/bt-recover.sh` does this, and
     `bluetooth/adapter.rs` runs it automatically after 20 s unhealthy, with back-off of 1, 5, 15 and 30 minutes. Verified on the device.
- **Speaker refusing audio** (`avdtp_connect_cb … Invalid exchange (52)` / `br-connection-unknown`) means **key missing**: the speaker forgot
  the bond, for example after pairing with a phone (BlueZ maps EBADE to "key missing"). It was confirmed when re-pairing fixed it immediately.
  `bluetooth/speaker.rs` handles it:
  - "connected" means **audio-ready** (an A2DP PCM appears in `bluealsa-aplay -L`), not just the ACL link
  - a link that comes up without audio counts as a refusal; after 3 refusals the state is `needs_repair`: retrying stops, the UI shows **Re-pair**,
    and the app scans every 2 min to re-pair automatically when the speaker is in pairing mode
  - a speaker that is simply off backs off quietly (5 s doubling to 60 s)
- **Bluetooth agent:** without one, a speaker reconnecting by itself is rejected (`Authentication attempt without agent`).
  `bluetooth/agent.rs` keeps an interactive `bluetoothctl` with a `NoInputNoOutput` default agent and answers `yes` to prompts.
  The adapter is kept non-pairable and non-discoverable except during UI pairing.
- **BlueZ and bluez-alsa configuration** (installer): `ControllerMode = bredr`, `FastConnectable = true`, `JustWorksRepairing = always`, and
  `ReconnectIntervals`. The bluealsa drop-in runs `-p a2dp-source --keep-alive=10` (the stock unit also enabled `a2dp-sink`).
  Verified recovery after a forced disconnect and after a bluetoothd restart: about 9 s each.
- **Speaker battery** is only reported over HFP: the speaker sends battery AT commands, and bluealsa forwards them to the BlueZ battery provider.
  `deploy/bt-audio.sh` therefore runs bluealsa with `-p a2dp-source -p hfp-ag --sbc-quality=<q> --keep-alive=10`. The SCO channel is never opened.
  Verified: `Battery Percentage: 0x64 (100)`. The UI shows it and the log warns once at 20% or below.
- **SBC quality** setting `bt_quality`: `medium` ("Balanced", the default) or `high`. **`low` was removed on 2026-11-07**: on the owner's VSW07
  speaker it only sustained about 150 KB/s instead of the 192 KB/s real time, so playback was choppy and the buffer drifted minutes behind live.
  Medium and high both measured 193 KB/s. Measure throughput with the delta of `wchar` in `/proc/<livemasjid pid>/io` over 20–30 s.
- **Latency cap:** if the jitter buffer grows past 60 s (the output played slower than real time), the pump trims to about 20 s behind live
  (`ByteQueue::trim_front` drops whole Ogg pages) and logs it once.
- **UI assets use content-hash ETags** (FNV-1a of the file). Version-based ETags served stale JS after dev deploys, because every dev build
  from one commit shares the label `dev-<sha>-dirty`.
- **The service is sandboxed (`ProtectSystem=full`)**, so `/etc` is read-only even for `sudo` children. Root actions that write `/etc` must go
  through a systemd unit, for example `systemctl start livemasjid-bt-audio@<q>.service` (exact sudo rule), like the update service does.
- **Watchdog restart loop (2026-11-07, 74 restarts in 1.5 h).** A stalled Bluetooth link made `aplay`'s pipe stop accepting audio. The sink's
  blocking write froze its thread, its heartbeat went stale, the app stopped petting systemd, and `WatchdogSec=45` killed the service.
  Each restart dropped the speaker, which looked like Bluetooth flapping. Fix (`sink.rs`): writes go through a writer thread and a bounded queue;
  after 5 s without progress the output counts as stalled (`audio device stopped accepting data`), and `aplay` is killed and re-opened.
  Verified by `kill -STOP` on `aplay`: recovered in about 6 s with 0 restarts. **Never do blocking I/O on the sink or player thread.**
- **Half-hung Bluetooth chip:** it reports `Powered: yes`, but HCI commands time out (`hci0: Opcode 0x0401 failed: -110`,
  `command 0x0408 tx timeout`), so scans find nothing. The app cannot read the kernel log, so `netwatch.sh` (root, every 30 s) counts these
  lines and runs `bt-recover.sh` when there are 2 or more in 90 s, at most once every 5 min.
  The upgrade from kernel 6.18.50 to 6.18.60 (via `install.sh --upgrade`) left 0 timeouts after reboot.
- **System page data** (`sysinfo.rs`, `/api/system`) comes from /proc, /sys, `iw` and `bluealsa-aplay -L`. On this kernel, Pi under-voltage is
  read from hwmon `rpi_volt/in0_lcrit_alarm`; `get_throttled` doesn't exist.
- **HDMI audio without a display fails** with ALSA error 524. The output list skips HDMI unless `/sys/class/drm/*HDMI*/status` reads `connected`.
- **Audio goes through an `aplay` child process.** Bluetooth uses `bluealsa:DEV=<MAC>,PROFILE=a2dp` from Debian's `bluez-alsa-utils`
  (`bluealsa.service`). While waiting, the sink feeds silence ("keep awake") so playback starts instantly.
- **This kernel can't kill sockets** (`ss -K` does nothing). To simulate a network stall, use `sudo ip route add blackhole <relay-ip>`, then `del`.
- **Never pipe into `python -` on the dev PC.** `python` is the Windows Store stub and blocks forever.

## Seamless playback (stream pipeline)

`socket → pump → OggFilter → ByteQueue (jitter buffer, up to 4 MB) → decoder thread → sink`

- **The buffer holds about 20 s**, which is Icecast's burst on connect. It keeps playing while the pump reconnects.
- **Stall detection:** 6 s socket read timeout (pages normally arrive about once per second).
- **On reconnect, `OggFilter` skips** the replayed headers and every page whose granule is ≤ the last forwarded one. Playback resumes
  exactly where it stopped: no gap if the buffer lasted, never a repeat. A new serial means the encoder restarted upstream, so a fresh decoder is used.
  MP3 has no granules, so an MP3 reconnect may repeat up to the burst length.
- **Verified:** a 16 s blackhole of relay2 was inaudible; the buffer went from 21 s down to about 2 s and back to 21 s. `buffer_ms` is shown in the UI.

## Open issues / next steps

- **Wizard:** the full interactive `setup.sh` run on a freshly flashed card is not yet verified. `deploy.sh --setup` (same installer,
  including `--upgrade` and reboot detection) was verified from Git Bash on Windows.
- **The 32-bit CI build** is verified: release `v0.1.0-3` (2026-11-07) has aarch64, armv7 and armv6 assets, and the Pi updated to it through the timer path.
- **The livemasjid IP ban** was lifted (verified 2026-11-07). Keep the request budget to stay that way.
- **Wi-Fi was marginal** (-64 dBm; -50 dBm by 2026-11-07, after the setup changed). It is 2.4 GHz and shared with Bluetooth. Large uploads to the Pi saturated it and coincided with stalls and the
  Bluetooth chip hang. Keep dev uploads small, and move the Pi or the access point closer if stalls recur.
- **Possible additions:** password-protected remote access (Tailscale), and multiple masjids with a priority order.

## Code map

- `main.rs` wiring and the systemd watchdog · `state.rs` shared state and `Status` · `config.rs` settings and password hashing
- `player.rs` state machine (signals → sessions, stop and suppression, probe)
- `stream/` `mod.rs` session and reconnect · `buffer.rs` jitter buffer · `ogg.rs` seamless page filter · `decode.rs` Opus and MP3 decoding
- `sink.rs` aplay output thread (epochs, keep-awake, failover)
- `monitor/` `sse.rs` live push · `poll.rs` Icecast status · `site.rs` website back-off guard
- `bluetooth/` `mod.rs` supervisor · `adapter.rs` power, rfkill and controller reset · `devices.rs` pair, use, forget, re-pair · `speaker.rs` audio-ready state machine · `agent.rs` persistent agent · `ctl.rs` bluetoothctl · `outputs.rs` ALSA targets
- `sysinfo.rs` device, Wi-Fi, Bluetooth and app health for the System page
- `integrations/` `mod.rs` curl fetch, caches and guards · `salah.rs` masjidscreen · `recordings.rs` RSS feed · `stream/recording.rs` playback
- `catalog.rs` directory scrape, cache and seed · `history.rs` JSONL sessions · `net.rs` raw Icecast HTTP and endpoints
- `web/` `mod.rs` router and `ApiError` · `auth.rs` · `assets.rs` (embedded UI, ETag) · `api/{status,stations,player,speaker,system}.rs`
- Device setup: `setup.sh` (wizard), `deploy.sh` (dev loop) · `deploy/install.sh` (generic installer) · `deploy/update.sh` (`--check`, `--force`) · `docs/SETUP.md`
- `ui/` `index.html` markup only · `css/{base,layout,components,views}.css` · `js/` ES modules: `main.js` router, `store.js` live status,
  `api.js`, `dom.js`, `format.js`, `views/{now,masjids,speaker,history,settings,system,recordings,salah}.js`. New UI files must be added to `FILES` in `web/assets.rs`.
