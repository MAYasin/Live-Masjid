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
- **Docker (changed 2026-11-11):** first rejected, now wanted for two uses only: local development (`docker compose` with hot reload)
  and users running it on a home cluster (an image + compose; audio to an ALSA/USB speaker on that node, Bluetooth only via the host's BlueZ).
  **The Pi keeps the native install** (`setup.sh`/`install.sh`); never move the Pi to Docker.
- **Setup of new devices** is a bash wizard, `setup.sh` (macOS, Linux, Windows via Git Bash/WSL; bash 3.2-compatible for macOS), driving the generic `deploy/install.sh` over SSH, with a guide in `docs/SETUP.md`.
  It targets any Pi on Pi OS plus Debian-based ARM boards, 64-bit and 32-bit (aarch64, armv7, armv6).
- **Audio: keep Bluetooth and wired both supported.** The owner currently has only a Bluetooth speaker, so Bluetooth must be rock-solid.
- **UI style.** Minimal and neutral, like a FAANG product: grayscale plus one dark primary colour, green only for "live",
  6 px radii, flat 1 px borders, no gradients or glows. It must not scroll sideways at 375 px. The sidebar appears at 900 px and wider.
- **UX cleanup (2026-11-08, chosen by the owner):** home has no recent broadcasts, and shows one warning only when something is wrong
  (`alertFor`). The status tiles are one compact row under the masjid card (`paintTiles`): plain-language values (Speaker, Live signal,
  Internet, then Buffer while live or "Live now"), each tile a button that opens Speaker/Device & app/Masjids. Pages keep set-and-forget
  options under a collapsed "Advanced". Addresses are shortened (`shortPlace`).
- **Masjid page (2026-11-11, chosen by the owner):** a masjid row opens `#masjid/<mount>` (`views/masjid.js`): Listen / Follow, that
  masjid's salah times and all its recordings (no popup). The Now page has no recordings list, only a "Recordings" button on the card
  (`#masjid/<mount>/recordings` scrolls to them). Old `#recordings/<mount>` links redirect. The salah card is a component (`SalahCard`
  in `views/salah.js`, one per card; `salah.init()` wires the shared "Change source" dialog once, from `now.js`).
- **Navigation (2026-11-11, chosen by the owner):** five items in the phone tab bar and the sidebar: Now, Masjids, Quran, History, Settings.
  There is no More sheet. Settings is a hub (`views/settings.js`, one row per sub-page with a one-line live status) for Speaker, Playback
  (`views/playback.js`), Device & app (`views/system.js`: the app's own CPU, memory, uptime and Quran storage first, then the Pi, then
  collapsed details and the activity log), Updates (`views/updates.js`), Password (`views/security.js`) and "Install on this phone".
  Sub-pages have `section: "settings"` in `VIEWS` (main.js), so the Settings item stays highlighted, and a `.back-link` (hidden at 900 px+).
  The masjid card on Now always shows the broadcast; the Quran or a recording has its own player card under it (`ui/js/miniplayer.js`,
  also at the top of the Quran page) with icon buttons.
- **History (2026-11-11):** kept for 1 year (`history::KEEP_MS`) as well as the existing caps (2000 sessions, 4 MB file). Each session
  gets a salah label (`Session.salah`) shortly after it starts: `salah_match::salah_for` matches the start against that day's times
  (60 min before the adhān to 60 min after the iqāmah, closest wins, Zuhr on a Friday is Jumu'ah). Older sessions of the followed masjid
  without a label get an estimate from today's times (`salah_estimated`, shown as a dashed "around X" tag). The page shows the masjid's
  short name, day headers with count and total, and "Show older" in pages of 150. Filters: masjid scope, period chips (7 days / 30 days /
  Year) and salah chips (Zuhr includes Jumu'ah). No calendar: the owner had it removed. The Year view lists this month in full and
  earlier months as summary rows; tapping one shows that month as a removable range chip. `/api/history` takes `from`, `to`, `salah`,
  `mount`, `limit` and returns `sessions`, `total`, `listened_ms`, `last` and `days` (per local day, whole year: `history::per_day`).
  `KEEP` is 4000 sessions so a busy year fits.
- **Data (2026-11-11):** Settings › Data has "Clear history" (`/api/system/clear-history`) and "Reset app" (`/api/system/reset`).
  Reset removes everything in the data folder except `quran/` (`reset_data`, never following symlinks), then exits at once, and
  systemd or Docker restarts it fresh. BlueZ pairings stay at the OS level; only the app's speaker choice is forgotten.
  The Masjids search is cleared every time that page opens.
- **Phone install over plain HTTP:** iPhone Safari "Add to Home Screen" gives a full-screen app (apple-touch-icon and meta tags). Chrome
  installs a real app only on HTTPS, so Android gets a shortcut. The owner chose "best effort on http" for now; HTTPS (own domain with
  DNS-01, or Tailscale) is the way to a real Android install. Home-screen widgets need native apps. PNG icons are drawn by a small Node
  rasteriser (no image tools on the dev PC) into `ui/icons/` and served at `/icons/<name>`.
- **Footprint matters.** In testing it used about 4 MB of RAM and 1–2% CPU while playing, as a 3 MB static binary.

## Target device

- **Address:** `pi@192.168.1.101` (also `pi.local`).
- **OS:** Raspberry Pi OS Lite 64-bit Trixie (Debian 13), aarch64, 415 MB RAM.
- **Access:** SSH key auth from the dev PC (`~/.ssh/id_ed25519`), key sign-in only. `pi` has **no blanket passwordless sudo** (removed
  2026-11-11 by `harden.sh`): only `sudo -n /opt/livemasjid/install-bin.sh` works without a password; anything else asks for it.
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
  - `update.sh` won't replace a dev build for 12 hours unless run with `--force`. This is intentional (the owner prefers it):
    a test build stays until it is 12 hours old even if a newer release appears, so development isn't overwritten mid-test.
    After testing, finish with a release (push, then "Update now" or `update.sh --force`) so the device is back on releases.
  - **Updates from the UI** (System → App): "Check for updates" runs `update.sh --check` as the app user (GitHub API) and shows the
    answer; "Update now" writes `<data>/update-request` and starts `livemasjid-update.service`, and `update.sh` treats that run as `--force`.
    The "Automatic updates" switch is `Settings::auto_update`; timer runs grep `settings.json` for `"auto_update": false` and skip.
    `update.sh` writes `<data>/update-status.json` (`checking/updating/updated/up_to_date/skipped/failed`); the page polls it and
    reloads once the version changes. No new sudo rules were needed. `deploy.sh --scripts` installs the helper scripts (asks for the Pi password).
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
  - Icecast sends a fixed-size **byte** burst (~120–150 KB) on connect: about **28 s of Opus** (~35 kbps) but about **17 s of MP3** (64 kbps).
    A player that plays the burst from its start is that far behind live; that is why Opus "sounded behind MP3" (measured 2026-11-07).
    At the live edge the two are within about 1 s (cross-correlated in Chrome), so we keep Opus and skip the burst instead (`stream/edge.rs`).
  - Chrome's plain `<audio>` won't skip a live MP3 burst: it reads only ~2–3 s ahead and the rest waits in the socket.
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
  (persisted: `site.json` for livemasjid, `site-masjidscreen.json`, `site-masjidboard.json`).
- **Salah times, primary source: masjidboardlive.com** (`integrations/masjidboard.rs`; the owner found masjidscreen's times inaccurate,
  2026-11-08). Its JSON API is `https://masjidboardlive.com/boards/api/board.php?id=<board>` → `{"ok":true,"data":{…,"meta":{…}}}`.
  It has every time, Jumu'ah (`jumuahTime1..3` with `jumuahHeadings` "0,3,6", meaning Adhān/Sunan/Khutbah, codes 0–10 in
  `JUMUAH_HEADINGS`), `jumuah_khateeb`, `{fajr,asr,esha}Next{Date,Time}` changes, `islamicDateAdjust`, `liveStreamURL` and `meta.web_url`.
  An unknown id gives `{"ok":false,"error":"not_found"}`. Placeholders `~~~~`/`#N/A`/`00:00`/`&nbsp;` mean "none". The board page polls every 30 s; we cache 1 h.
  - **Board ids ≠ mounts, and the API matches ids loosely.** `lakewood` → board `lakewood-kauthar`; `medina` → an unrelated Hercules masjid;
    `greenside` → a board whose stream is `greensidemasjid`. Rule: use a board automatically only when its `liveStreamURL` equals the mount;
    if it names no stream, offer it as `suggestion`; if it names another stream, ignore it. The user can paste a board link
    (`parse_board_ref`: `?id=`, `?<id>`, premium `?mid=`), which is saved per mount in `Settings::salah_boards` ("none" = masjidscreen only).
  - The directory (`findmasjid/list.php`) loads through an obfuscated POST (`assets/php/live_page.php`); we don't scrape it.
- **Salah times, fallback: masjidscreen** `https://masjidscreen.co.za/<mount>/api/prayertimes.php` returns `salaah_times.{fajr,zuhr,asr,maghrib,esha,jummah}.{athaan,jamaat}`.
  Unknown masjids return 404, which shows as "not_listed" (cached 24 h). Successes are cached 6 h and re-fetched when the date changes.
- **Salah card UI** (`views/salah.js`):
  - Gregorian date plus the Hijri date (`Intl` `islamic-umalqura`, shifted by `hijri_adjust`, moving on at sunset/Maghrib).
  - The 5 salah with adhān and iqāmah; the next one gets a countdown.
  - Other day times in a compact grid; Zawāl is shown as a range, and Suhūr/Fajr are merged when they're equal.
  - Jumu'ah and changes on one line each. Jumu'ah replaces Zuhr on Fridays.
- **Reciters** (2026-11-08): the catalogue `https://www.mp3quran.net/api/v3/reciters?language=eng` has 244 reciters and 299 recitations
  ("moshaf"), fetched monthly into `<data>/quran/reciters.json`. Each recitation is keyed by its server path (`abdulrahman-sudais-r1`).
  Only https `*.mp3quran.net` servers are accepted, some recitations lack surahs (`surah_list`), and popular reciters sort first (`POPULAR`).
  Files live in `<data>/quran/<key>/`. On first start, the old `quran/sudais` folder is renamed (downloads survive; there is no rollback link,
  as the owner said the app isn't officially released). `Settings::quran_reciter` is the default; a playlist may pin `reciter`.
- **Scheduled playlists:** `Playlist.days` (0 = Sunday) + `time` "HH:MM". `schedule.rs` reads `date +%w %H:%M %F` every 20 s and fires
  once per day per playlist. The player starts it at once, or, if live is playing or about to, waits up to 2 h for the broadcast to end
  (`quran_pending`), then skips it. Verified: a playlist due one minute later fired and played.
- **Quran** (`quran/` library, `stream/local.rs` playback, `views/quran.js`): Al-Sudais (the default) from
  `https://cdn.mp3quran.net/audio/abdulrahman-sudais/r1/NNN.mp3` (from `mp3quran.net/api/v3/reciters`, id 54). The files are 128 kbps CBR
  MP3 with an ID3 tag, from 0.4 MB (114) to 90 MB (2, 1:33:58).
  - **Storage:** `<data>/quran/sudais/NNN.mp3` (`.part` while downloading).
  - **Downloads:** one at a time, `curl --limit-rate 400K -C -` (resumable; the rate is gentle on the Wi-Fi/Bluetooth chip).
    Playing a surah puts it first and pauses any other download. "Download all" works through the rest in order. Failures back off from 15 s to 10 min.
  - **Playing while downloading:** `Growing` reader waits for the file to grow, and gives up after 90 s with no growth.
  - **Seeking:** byte offset = ID3 length + time × bitrate, then the next frame confirmed by the frame after it. It waits until that
    part is downloaded. `DecodeCtx::position_base_ms` keeps the position right after a seek.
  - **Playlists** (2026-11-08): named, stored on the Pi in `quran-playlists.json` (`Library::save_playlist`, at most 50) and shared by all users,
    each with a repeat toggle. Playing one carries a `PlaylistCtx` snapshot (surahs, index, repeat), so editing doesn't disturb playback.
    `neighbour()` picks next/previous inside the playlist (wrapping only with repeat) or in Quran order. The end of a playlist without
    repeat stops; the resume after a live broadcast keeps the playlist.
  - **Live precedence** (player): a broadcast that starts during the Quran stops it and saves `quran_resume` (surah and position;
    `auto` = it wasn't paused by the user). "Play now" also saves it. The Quran continues once the broadcast has **closed**: no
    session, not live (or stopped by the user), for 15 s straight, checked on the 5 s tick. A reconnect keeps its session, so it never
    triggers a resume. Verified: interrupted at 0:10 → live attempt ended → resumed at 0:10 about 15 s later.
  - Verified on the Pi: An-Nās downloaded in under 2 s; Al-Baqarah started within 8 s while downloading; seek to 2:00 → 2:09; pause held; next; stop.
- **Recordings:** the RSS feed at `https://www.livemasjid.com/api/get_mountrecordings.php?mountname=<m>&feeditems=40` (iso-8859-1,
  single-quoted `enclosure url=` on `cdn.livemasjid.com` .mp3, no durations), cached 15 min. Override it with `LIVEMASJID_RECORDINGS_FEED`
  (`{mount}`, `{n}`) for testing. **The livemasjid IP block was lifted by 2026-11-07 15:40** (the feed returns 200 again).
- **Recording playback** (`stream/recording.rs`): `curl` → `ByteQueue` → the shared decoder, with pause/position on `DecodeCtx`.
  The API plays by **id from the cached list, never a raw URL** (no SSRF). A broadcast that starts during a recording interrupts it;
  stopping or finishing a recording resumes live if the masjid is on air. Verified: play, pause (position held), resume, stop, then back to live.

- **Listening in the browser** (setting `browser_listening`, off by default). Both endpoints refuse while the setting is off.
  - **In sync with the speaker:** the sink copies exactly what it hands to the device (`SinkHandle::mirror`, a tokio broadcast) as mono 24 kHz s16.
    `/api/listen/live.wav` streams it as an endless WAV (at most 6 listeners; a lagging listener skips ahead). It is paced by the Pi's audio
    device, so it mirrors live and recordings alike.
  - The browser (`ui/js/local.js`) holds 0.85 s buffered (`SYNC_TARGET_S`, matching the speaker's path: writer queue, 64 KB pipe, aplay 250 ms,
    Bluetooth), plus a per-device "Timing" offset (−/+ 0.1 s, localStorage). **Steer on the peak of `buffered.end - currentTime` over ~2 s**: Chrome
    moves data into `buffered` in ~0.5 s steps, so the value is a sawtooth whose peak is the true lag (steering on the mean left it ~0.35 s late).
    More than 0.8 s off: seek within the buffer (works on the endless WAV); otherwise `playbackRate` 0.97–1.08, pitch preserved.
    Chrome prebuffers ~2.4 s before it starts, so without the seek it took ~35 s to converge. Measured: peak 0.81–0.97 s, 0 stalls.
  - **Two modes** (segmented control, remembered per device): "In sync with speaker" (above) and "Lowest delay": `fetch` livemasjid's MP3,
    drop the connect burst except the newest 1.5 s (`mp3Start`), and append to MediaSource (`audio/mpeg`, sequence mode). It holds ~1.3–1.5 s.
    Browsers without MediaSource (iPhone Safari) fall back to a plain `<source>` (burst-delayed).
  - The Pi's socket Send-Q for the WAV stays under 3 KB, so no lag hides on the Pi side.
  - `/api/recordings/url` gives "Play here" for recordings, which are independent of the speaker.

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
- **SBC quality** setting `bt_quality`: `high` (the default since 2026-11-07, at the owner's request) or `medium` (shown as "Low"). The UI never offers real `low`. **`low` was removed on 2026-11-07**: on the owner's VSW07
  speaker it only sustained about 150 KB/s instead of the 192 KB/s real time, so playback was choppy and the buffer drifted minutes behind live.
  Medium and high both measured 193 KB/s. Measure throughput with the delta of `wchar` in `/proc/<livemasjid pid>/io` over 20–30 s.
- **Keep-awake silence was under-feeding (fixed 2026-11-07).** The sink waited 25 ms before each 20 ms silence chunk, which is about 80% of
  real time (measured 154 KB/s), so idle Bluetooth speakers kept under-running. That caused the instability between broadcasts while
  24/7 streams were fine. It now writes silence back-to-back, paced by the device (1 ms receive timeout while feeding). Measured
  193 KB/s at about 0% CPU. **Always verify any output path at real time (192 KB/s, 48 kHz stereo s16).**
- **Latency cap:** if the buffer exceeds the live delay + max(3 s, delay/2) (after a drop-out, the missed audio refills it), the pump trims
  back to the delay (`ByteQueue::trim_front` drops whole Ogg pages). It waits 8 s after joining, because the estimate needs time to settle.
- **SIGSEGV at start-up from axum `.nest()` (2026-11-08).** One build crashed 17 ms after start while matchit 0.8.4 (pinned by axum 0.8.9)
  formatted "Invalid route \"/{*__private__axum_fallback}\": Insertion failed due to conflict…" with a null route string. It was
  layout-dependent: the same source built with symbols kept didn't crash. Fix: the `/api` routes are registered with full paths and
  `merge`d instead of `nest`ed, so no catch-all fallback is inserted. Diagnose such crashes with gdb on the Pi (`gdb -batch -ex run -ex bt`),
  then read the buffer behind `x0` to see what was being written.
- **Optimistic controls (2026-11-08).** `runAction` swaps a button for a spinner and then restores the HTML it saved before the click, so
  a status push that arrived in between (play → pause icon) was overwritten until the next 5 s tick. Player controls therefore don't use it:
  `miniplayer.js` applies the expected state at once (`optimistic()`, confirmed by the live status or dropped after 4 s or on error), ticks
  the clock locally between pushes, and `starting(title)` shows a card the moment a Play button (surah, playlist, recording) is tapped.
  The home Play/Stop swap at once too (`liveAction`). Keep spinners for slow, failure-prone actions (Bluetooth, downloads, updates).
- **Quran speed and volume (2026-11-09).** Speed 0.75–2× without a pitch change: `stream/stretch.rs` (streaming WSOLA: 30 ms segments,
  10 ms crossfade, ±8 ms best-match search on every second sample) runs in `PcmOut::send` for Quran playback only; positions stay in
  recitation time. One speed value (`qspeed` in the player) is shared by every surah, so seek/next/resume keep it; a play sets it from the
  request, the playlist's `speed`, or `Settings::quran_speed`. Changing it in the card saves the default unless the playing playlist has its
  own. Measured on the Pi: 1.5× advanced 30 s in 20 s, ~11% of one core. The Quran has its own volume (`Settings::quran_volume`), fully
  independent of the broadcast volume (2026-11-11): every PCM message carries a `sink::Channel` (Broadcast for live and recordings, Quran
  for `local.rs`), and the sink applies `volume` or `quran_volume` per channel. The player card's slider controls it.
- **Quran pause is instant:** pausing drops the ~2 s already queued for the speaker (`sink.new_epoch()`) and stores the *heard* position
  (`heard_ms` = decoded − `queued_ms` − `OUTPUT_LAG_MS` 700); resume restarts from the file at that position. Status positions are heard positions.
- **Signed-release failures** (missing signature, bad signature, checksum mismatch) are reported in `update-status.json` with the reason.
- **Range sliders paint their fill on the `<input>` background** (`--fill`), not on `::-webkit-slider-runnable-track`: Safari doesn't repaint
  the track pseudo-element when the custom property changes (the Quran seek bar looked stuck at 50%).
- **`node --check` isn't a reliable syntax check for the UI's ES modules here** (it passed a broken string). Check in the browser
  (console errors) after deploying.
- **UI assets use content-hash ETags** (FNV-1a of the file). Version-based ETags served stale JS after dev deploys, because every dev build
  from one commit shares the label `dev-<sha>-dirty`.
- **The service is sandboxed (`ProtectSystem=full`)**, so `/etc` is read-only even for `sudo` children. Root actions that write `/etc` must go
  through a systemd unit, for example `systemctl start livemasjid-bt-audio@<q>.service` (exact sudo rule), like the update service does.
- **Watchdog restart loop (2026-11-07, 74 restarts in 1.5 h).** A stalled Bluetooth link made `aplay`'s pipe stop accepting audio. The sink's
  blocking write froze its thread, its heartbeat went stale, the app stopped petting systemd, and `WatchdogSec=45` killed the service.
  Each restart dropped the speaker, which looked like Bluetooth flapping. Fix (`sink/device.rs`): writes go through a writer thread and a bounded queue;
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

- **Join at the live edge** (setting `live_delay_s`: 3 s by default, "Near live"; 8 "Balanced"; 20 "Extra steady"; `stream::LIVE_DELAY_MS`
  applies immediately). `read_burst` reads until a read waits more than 400 ms (150 ms split the burst on Wi-Fi), then
  `edge::opus_pages` keeps the header pages and the newest pages that start a fresh packet, and `edge::mp3_start` cuts at a frame confirmed
  by the next one. Before this the Pi played the whole burst and drifted to about 29 s behind live. MP3 reconnects also re-join at the edge.
- **`buffer_ms`** = the jitter buffer plus decoded PCM queued for the sink (`SinkHandle::queued_ms`, up to ~1.3 s in the 64-chunk channel).
  With a 3 s delay it reads about 1.3–2.2 s; the rest is in the pipe, aplay and Bluetooth. A drop-out longer than the delay plays silence,
  then resumes where it stopped (Ogg). Latency grows by the gap until the cap trims it.
- **Stall detection:** 6 s socket read timeout (pages normally arrive about once per second). relay2 routinely pauses 2–3 s
  between pages (seen from both the PC and the Pi), which is harmless.
- **Network outage ≠ broadcast over** (2026-11-07). If no server answers at all (DNS failure, unreachable, refused), a reconnect keeps
  retrying every ≤4 s for up to 10 min (`NETWORK_GRACE`); it ends only when a server answers 404. Before this, a 69 s Wi-Fi outage
  logged "Broadcast ended", split the history session, and resumed only on the next status check (+25 s).
- **Origin `livemasjid.com:8000` refuses connections from everywhere** we tested (PC and Pi, 2026-11-07), while `www` works, so it is
  not a block on us. Streams come from relay2.
- **On reconnect, `OggFilter` skips** the replayed headers and every page whose granule is ≤ the last forwarded one. Playback resumes
  exactly where it stopped: no gap if the buffer lasted, never a repeat. A new serial means the encoder restarted upstream, so a fresh decoder is used.
  MP3 has no granules, so an MP3 reconnect may repeat up to the burst length.
- **Verified (with the old 20 s buffer):** a 16 s blackhole of relay2 was inaudible. With the 3 s default, a stall longer than about 2 s is
  audible as a gap. Users on weak Wi-Fi should pick "Extra steady".

## Open issues / next steps

- **Wizard:** the full interactive `setup.sh` run on a freshly flashed card is not yet verified. `deploy.sh --setup` (same installer,
  including `--upgrade` and reboot detection) was verified from Git Bash on Windows.
- **The 32-bit CI build** is verified: release `v0.1.0-3` (2026-11-07) has aarch64, armv7 and armv6 assets, and the Pi updated to it through the timer path.
- **The livemasjid IP ban** was lifted (verified 2026-11-07). Keep the request budget to stay that way.
- **Stream "reconnects" on 2026-11-07 were the Pi's Wi-Fi.** In an 8-minute test, ping to the router (192.168.1.1) lost ~2% of
  packets plus one 69 s outage, while the PC on the same LAN streamed with no stall. That day: 67 interruptions and 22 netwatch
  "network unreachable" checks at -55 dBm. The fix is the Pi's placement, a USB Wi-Fi adapter or Ethernet, not code.
- **Wi-Fi was marginal** (-64 dBm; -50 dBm by 2026-11-07, after the setup changed). It is 2.4 GHz and shared with Bluetooth. Large uploads to the Pi saturated it and coincided with stalls and the
  Bluetooth chip hang. Keep dev uploads small, and move the Pi or the access point closer if stalls recur.
- **Possible additions:** password-protected remote access (Tailscale), and multiple masjids with a priority order.

## Code map

- `main.rs` wiring and the systemd watchdog · `platform.rs` native install vs container, Bluetooth on/off, dev UI folder · `state.rs` shared state and `Status` · `config.rs` settings and password hashing
- `player/` `mod.rs` the `Player` state machine: one handler per input (commands, signals, live events, on-demand events, tick), live precedence, Quran resume, schedules, probe · `live.rs` `LiveSession`, `Trigger`, `StopReason`, history end labels · `media.rs` on-demand item (recording or surah), heard position, next surah (pure, tested)
- `stream/` `source.rs` the `Source` trait (every playback kind runs on its own thread via `source::spawn`) and `Events<E>` (events tagged with the session token, so stale ones are ignored) · `mod.rs` live session and reconnect · `buffer.rs` jitter buffer · `ogg.rs` seamless page filter · `decode.rs` Opus and MP3 decoding
- `sink/` `mod.rs` `SinkHandle` and the output thread (epochs, keep-awake, failover, per-channel volume) · `device.rs` one `aplay` process with a non-blocking writer and stall detection · `pcm.rs` gain, encoding, browser mirror, chime (pure, tested)
- `monitor/` `sse.rs` live push · `poll.rs` Icecast status · `site.rs` website back-off guard
- `bluetooth/` `mod.rs` supervisor · `adapter.rs` power, rfkill and controller reset · `devices.rs` pair, use, forget, re-pair · `speaker.rs` audio-ready state machine · `agent.rs` persistent agent · `ctl.rs` bluetoothctl · `outputs.rs` ALSA targets
- `sysinfo.rs` device, Wi-Fi, Bluetooth and app health (incl. Quran storage) for Device & app · `salah_match.rs` labels broadcasts with a salah
- `integrations/` `mod.rs` curl fetch, caches and guards · `salah.rs` source rules (`choose`, over the `Sources` trait: masjidboardlive + masjidscreen; tested with a fake) and masjidscreen · `masjidboard.rs` masjidboardlive · `recordings.rs` RSS feed · `stream/recording.rs` playback
- `quran/` `mod.rs` `Library` (files, delete, info) · `reciters.rs` catalogue · `playlists.rs` playlists and `validate` · `download.rs` queue and worker behind the `Fetcher` trait (`Curl` on the Pi, a fake in tests) · `schedule.rs` scheduled playlists · `stream/local.rs` file playback with seek (growing files) · `web/api/quran.rs`
- `catalog.rs` directory scrape, cache and seed · `history.rs` JSONL sessions · `net.rs` raw Icecast HTTP and endpoints
- `web/` `mod.rs` router and `ApiError` · `auth.rs` · `assets.rs` (embedded UI, ETag) · `api/{status,stations,player,speaker,system}.rs`
- Device setup: `setup.sh` (wizard), `deploy.sh` (dev loop) · `deploy/install.sh` (generic installer) · `deploy/update.sh` (`--check`, `--force`) · `docs/SETUP.md`
- `ui/` `index.html` markup only · `miniplayer.js` Quran/recording player card · `install.js` install guide · `icons/` PNG app icons · · `css/{base,layout,components,views}.css` · `js/` ES modules: `main.js` router, `store.js` live status,
  `api.js`, `dom.js`, `format.js` (incl. `clock`, `uptime`, `fileSize`), `components.js` (shared `icon`, `emptyState`, `definitionList`, `meter`, `pollWhileOpen`: use these rather than hand-written markup or timers), `views/{now,masjids,quran,speaker,history,settings,playback,system,updates,security,recordings,salah}.js`. New UI files must be added to `FILES` in `web/assets.rs`.

## Signed releases

- CI signs every `livemasjid-<arch>-linux.tar.gz` with Ed25519 (`openssl pkeyutl -sign -rawin`) using the repo secret `RELEASE_SIGNING_KEY`,
  and publishes `<asset>.sig`. The release job fails if the secret is missing. The private key lives only in that secret and in the owner's backup.
- `deploy/release.pub` is the public key. `install.sh` copies it to `/etc/livemasjid/release.pub` **only if absent** (trust on first use), so a
  later release can't swap the key. `update.sh` verifies the signature when that file exists and refuses unsigned or invalid releases.
  Devices without the key skip the check once (with a warning) and get the key on their next config update (`CONFIG_VERSION` 6).
- Verify by hand: `openssl pkeyutl -verify -pubin -inkey deploy/release.pub -rawin -in <asset> -sigfile <asset>.sig`.

## Code conventions (refactor of 2026-11-11)

- **Clippy-clean** (`cargo clippy --release --target aarch64-unknown-linux-musl --all-targets` gives no warnings). Keep it that way.
  The MSRV is 1.88: don't use newer std APIs (e.g. `Atomic*::try_update`); clippy flags them.
- **Rules live in pure functions with unit tests**; I/O sits behind small traits so the rules can be tested with fakes:
  `stream::Source` (playback kinds), `quran::Fetcher` (downloads), `integrations::salah::Sources` (salah providers).
- **Behaviour-preserving refactors are checked on the Pi**: the test binary (see Build), then the real paths (Quran play/pause/seek/
  next/stop through the API, a live broadcast, the browser console). Don't deploy while a broadcast is playing: a deploy restarts the service.

## Docker (2026-11-11)

- **Files:** `Dockerfile` (users' image), `compose.yaml` (users), `Dockerfile.dev` + `compose.dev.yaml` (development), `docs/DOCKER.md`.
- **The image cross-builds on the build machine** (`--platform=$BUILDPLATFORM`): static musl via `rust-lld`, including `x86_64-unknown-linux-musl`
  (added to `.cargo/config.toml`). QEMU runs only the runtime stage's `apt-get`. A local arm64 + armv7 build took about 7 minutes.
  There is no `tls` feature (it needs a C cross-compiler); plain HTTP works for every endpoint.
- **CI:** the `image` job in `release.yml` (after `release`) pushes `ghcr.io/<owner>/live-masjid:{latest,<tag>}` for amd64/arm64/armv7.
  The package has to be made public once in GitHub's package settings for anonymous `docker compose pull`.
- **`platform.rs`:** `Native` vs `Container` (`LIVEMASJID_PLATFORM`, else `/.dockerenv`).
  - In a container: no sudo (adapter recovery is skipped), "Update now" explains `docker compose pull`, and "Check" asks the GitHub
    releases API directly.
  - Bluetooth runs only with the host's D-Bus socket mounted (`bluetooth_enabled`). The status and `/api/system/updates` carry `platform`.
- **Dev hot reload:** `LIVEMASJID_UI_DIR` makes `web/assets.rs` read UI files from disk on each request (still only the paths in `FILES`).
  It also injects a script that polls `/__dev/stamp` (newest mtime under the folder) and reloads the page. `cargo watch --poll` restarts
  on Rust changes (measured about 5 s); polling is used because file events from a Windows host don't reliably reach the container.
- **`docker compose -f compose.dev.yaml run --rm dev cargo test`** runs the tests on Linux x86 (68 passed), with no copy to the Pi.
- **The dev container adds to the livemasjid.com traffic from the home IP:** stop it after use.

## Security (2026-11-11)

- **No back doors.** Nothing installs or changes code that root runs without the Pi password or a signed release.
  - `deploy.sh` (dev loop) installs only the binary, through the narrow rule `pi ALL=(root) NOPASSWD: /opt/livemasjid/install-bin.sh`.
    The binary runs as the unprivileged `livemasjid` user. `install-bin.sh` copies it in as root-owned, and checks it with `--version`
    as `livemasjid`, never as root.
  - Root-run scripts (`update.sh`, `netwatch.sh`, `bt-*.sh`, `install-bin.sh`, `harden.sh`) change only through signed releases or
    `deploy.sh --scripts`, which asks for the password.
- **`harden.sh`** (installed in `/opt/livemasjid`; run `sudo /opt/livemasjid/harden.sh`; safe to repeat):
  - SSH key sign-in only, via a `10-` drop-in that wins over Pi OS's `50-cloud-init.conf`. Skipped without an authorized key.
  - Comments out the user's `NOPASSWD: ALL` (Pi OS's `010_pi-nopasswd`), with a backup and `visudo -c`. Skipped if the user has no password.
  - Root-owns `/opt/livemasjid` and the sudo rules.
- **The app user's sudo rules are exact commands** (rfkill unblock, `bt-recover.sh`, two `systemctl start` units). Keep them that way:
  no arguments the app chooses, no wildcards, and only scripts owned by root and not writable by others.
- **Root never touches the data folder directly.** `update.sh` reads and writes `/var/lib/livemasjid` through `runuser -u livemasjid`
  (`as_app`). The app owns that folder, so a planted symlink could otherwise make root overwrite or `chmod` any file.
- **Web (`web/guard.rs`, every request):**
  - The `Host` must be an IP address, `localhost`, `*.local`, the host name (alone or on `.lan`/`.home`/`.internal`/`.localdomain`/
    `.home.arpa`), or a name in `LIVEMASJID_ALLOWED_HOSTS`. This blocks DNS rebinding.
  - Never allow `<hostname>.<public domain>`: an attacker can register `raspberrypi.example.com`.
  - Requests that change something must have no `Origin` or one matching `Host`. This blocks cross-site requests, which matters most
    with no password set.
  - Session cookie: HttpOnly, SameSite=Strict.

## More gotchas

- **SIGILL from the ARMv8 crypto extensions (fixed 2026-11-11).** `target-cpu=cortex-a53` also turns on the `aes`/`sha2` instructions, but
  the Broadcom chips in the Pi Zero 2 W, 3 and 4 leave them out. SHA-256 (the UI password) killed the service with `status=4/ILL`, so
  setting a password crashed it.
  - `.cargo/config.toml` now adds `-C target-feature=-aes,-sha2`. Keep it.
  - `config::password_tests` hashes on the Pi and would crash the test run if this comes back.
