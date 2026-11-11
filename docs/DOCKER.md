# Docker

Live Masjid runs natively on a Raspberry Pi ([SETUP.md](SETUP.md)); that stays the recommended way.
Docker is for two other cases:

- **Running it on a home server or cluster node** that has a speaker attached.
- **Developing it on your computer**, with hot reload.

## Run it on a home server

You need Docker with Compose on a Linux machine (amd64, arm64 or armv7) that has the speaker plugged in.

```sh
curl -O https://raw.githubusercontent.com/MAYasin/Live-Masjid/main/compose.yaml
# Set TZ in compose.yaml to your time zone, then:
docker compose up -d
```

Open `http://<server>:8080`, choose a masjid, and it plays whenever that masjid goes live.

| | |
|---|---|
| **Image** | `ghcr.io/mayasin/live-masjid:latest` (each release also has its own tag, e.g. `:v0.2.0-30`). |
| **Data** | the `livemasjid-data` volume (`/data`): settings, history, Quran downloads. |
| **Update** | `docker compose pull && docker compose up -d`. The Updates page shows when a new version exists. |
| **Port** | 8080 inside the container; change the left side of `ports:` to publish another. |
| **Restart** | Settings › Device & app › Restart exits the app; `restart: unless-stopped` starts it again. |

### Speaker

- **Wired or USB speaker:** the compose file passes `/dev/snd` into the container. The app runs in the
  `audio` group; if the host's `audio` group has a different id (`getent group audio`), put that number
  under `group_add`. The app lists the sound cards it finds under Settings › Speaker.
- **Bluetooth speaker:** pairing and the audio link stay on the host. Run `bluetoothd` and
  bluez-alsa (`bluealsa`) there, and uncomment the `/run/dbus/system_bus_socket` volume.
  The app then sees the host's Bluetooth.
  - The Pi's automatic repairs (rfkill, controller reset) and the "Low/High quality" switch are host
    jobs, so they are off in Docker.
  - Set the quality with bluealsa's `--sbc-quality` on the host.
- **No speaker:** turn on Settings › Playback › Browser listening and use "Listen on this device".

### Not available in Docker

- Updating from the page (pull a new image instead).
- The Pi's Wi-Fi and Bluetooth watchdogs (`netwatch.sh`, `bt-recover.sh`), which belong to the host.

## Develop with hot reload

```sh
docker compose -f compose.dev.yaml up        # first start compiles, about 1–2 minutes
```

Then open <http://localhost:8080>.

- **UI** (`ui/`): served from disk. Save a file and the page reloads itself. A new UI file still has to
  be added to `FILES` in `src/web/assets.rs`, as for a release.
- **Rust** (`src/`, `Cargo.toml`): `cargo watch` rebuilds and restarts the app, a few seconds for a
  small change.
- **Tests**, on Linux (they don't run natively on Windows):
  `docker compose -f compose.dev.yaml run --rm dev cargo test`
- **Audio:** there is no speaker in the dev container. Use browser listening.
- **Stop it when you're done** (`docker compose -f compose.dev.yaml stop`). It watches livemasjid.com
  from the same home connection as your Pi, and the site has blocked us for too many requests before.

Environment variables the app reads:

| Variable | Meaning |
|---|---|
| `LIVEMASJID_PLATFORM` | `container` or `native`. Detected automatically from `/.dockerenv`. |
| `LIVEMASJID_DATA` | Data folder (default `/var/lib/livemasjid`). |
| `LIVEMASJID_PORT` | Web port (default 80; 8080 in the images). |
| `LIVEMASJID_BLUETOOTH` | `off` disables Bluetooth management. |
| `LIVEMASJID_UI_DIR` | Development: serve the UI from this folder, with live reload. |
| `LIVEMASJID_REPO` | GitHub repository checked for new versions in Docker (default `MAYasin/Live-Masjid`). |
| `LIVEMASJID_LOG` | Log filter, e.g. `livemasjid=debug`. |
| `TZ` | Time zone. Salah times, history days and schedules use it. |
