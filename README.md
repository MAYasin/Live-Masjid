# Live Masjid Pi

An always-on player for [livemasjid.com](https://www.livemasjid.com) streams, built for a
**Raspberry Pi Zero 2 W**. Pick your masjid once in the web UI. From then on, whenever that
masjid goes live, audio starts on your Bluetooth speaker (or a wired output). It does this
without anyone opening the page, and it carries on after reboots, power cuts and network drop-outs.

- A single static Rust binary (about 3 MB). It used about 4 MB of RAM and 1–2% CPU while playing in testing.
- No runtime dependencies beyond `aplay` (alsa-utils), `bluetoothctl` (BlueZ) and bluez-alsa.
- The web UI works on phones and desktops and updates live. It can be installed as an app (PWA).

## How it decides to play

The app uses three independent layers, so a missed or wrong signal never means silence:

| Layer | Source | When it is used |
|---|---|---|
| 1. Live push | `stateQuery.php?mount=…` server-sent events, the same signal the website's autoplay uses | Held open only while waiting for a broadcast, like the site's own player |
| 2. Status check | Icecast `status-json.xsl`, polled every 20 s by default with random jitter | Catches missed or incorrect pushes. It also drives the live badges in the directory |
| 3. The stream itself | A direct connection to the mount | Audio actually arriving is the final say. The app keeps playing until the mount really disappears, and rides through drop-outs of up to 25 s. If layers 1 and 2 have both been down for 2 minutes, it probes the stream directly |

What starts playback quickly:

- **Opus first.** The Opus stream (`/<mount>`) goes live about 6 s before its MP3 mirror (`/<mount>.mp3`). The app plays Opus and falls back to MP3 automatically.
- **Burst on connect.** Icecast sends a buffered burst on connect, so audio is decodable about 0.4 s after the live signal.
- **The speaker stays warm.** "Keep speaker awake" feeds silence while waiting. The ALSA device and the Bluetooth link never have to wake up, and speakers never auto-sleep.

The app is deliberately gentle on the livemasjid servers. It sends at most one status request
every 20 s, and backs off for up to 10 minutes when the server refuses requests (403, 429 or a refused connection).

## Repository layout

```
src/            Rust service (player, stream decoders, monitor, Bluetooth, web API)
ui/index.html   The whole web UI (embedded into the binary)
deploy/         systemd units, installer, updater, atomic installer with rollback
deploy.ps1      Build on Windows and ship to the Pi over SSH
.github/        CI: build, test and publish a release on every push to main
```

## First-time setup

**New device? Follow [docs/SETUP.md](docs/SETUP.md).** In short:

1. Flash the card with Raspberry Pi Imager: Raspberry Pi OS Lite, with Wi-Fi, SSH and your key set in the customisation screen.
2. On the Windows PC, run the wizard:
   ```powershell
   ./setup.ps1
   ```
   It sets up SSH, Wi-Fi, Bluetooth and the service, configures auto-updates, checks the release pipeline, and verifies the result.
3. Open the address it prints, pick your masjid, and pair your speaker.

Supported boards:
- Raspberry Pi Zero, Zero 2 W, 2, 3, 4 and 5
- Debian-based ARM boards (Armbian, DietPi, Ubuntu), both 64-bit (`aarch64`) and 32-bit (`armv7`, `armv6`)

## Shipping updates

**From this PC (development):**

```powershell
./deploy.ps1          # build for the device's CPU, upload, install; health check + automatic rollback
./deploy.ps1 -Logs    # follow the service log
```

The updater leaves dev builds alone for 12 hours, so it won't immediately replace what you just deployed.

**Automatic (production):** push to `main`.
1. GitHub Actions builds and tests on a native ARM64 runner and publishes a release (`v<version>-<run>`).
2. Each Pi checks hourly, or straight away from **Settings → Check for updates**.
3. The Pi verifies the SHA-256, installs the update atomically, health-checks it, and rolls back if the new build fails.

## On the Pi

| What | Where |
|---|---|
| Binary | `/opt/livemasjid/bin/livemasjid` (`VERSION` next to it) |
| Settings, directory cache, play history | `/var/lib/livemasjid/` |
| Port | `/etc/default/livemasjid` (`LIVEMASJID_PORT=80`) |
| Update source | `/etc/livemasjid/update.conf` (`REPO=owner/name`) |
| Logs | `journalctl -u livemasjid -f` |
| Manual update | `sudo /opt/livemasjid/update.sh --force` |

The service runs with `Restart=always` and a systemd watchdog. The app only pets the watchdog
while its player loop and audio thread are healthy, so a hang also leads to a restart.

## Play history

Every broadcast is logged to `history.jsonl` with its start and end times, codec, what triggered
it, how it ended, and any reconnects. Progress is saved every minute, so "last heard" survives power cuts.
The **History** tab shows it per masjid, and the directory shows "heard … ago" for each masjid.
