# Setting up a Live Masjid radio

This guide takes a brand-new board to a working, self-updating masjid radio in about 20 minutes.
Every step after flashing is automated by **`setup.sh`**, which runs on macOS, Linux and Windows (Git Bash or WSL). Re-running it is always safe.

## What you need

| Item | Notes |
|---|---|
| A board | **Raspberry Pi Zero 2 W** (recommended). Also any Pi 3/4/5, Pi Zero W, Pi 2, or a Debian-based ARM board (Orange Pi, Rock, …) |
| microSD card | 8 GB or larger, good quality (A1/A2) |
| Power supply | A proper 5 V supply. Weak phone chargers cause random reboots |
| Speaker | A Bluetooth speaker, **or** a USB audio adapter with a powered speaker (most reliable, no pairing) |
| A computer | macOS, Linux or Windows on the same network, with **Raspberry Pi Imager**. On Windows, run the scripts in **Git Bash** (comes with [Git for Windows](https://git-scm.com/download/win)) or WSL installed ([raspberrypi.com/software](https://www.raspberrypi.com/software/)) |

> The Pi Zero 2 W has no headphone jack. For a set-and-forget installation, a ~$5 USB audio adapter
> on a micro-USB OTG cable is the most dependable output. Bluetooth works well too; the app
> reconnects and repairs itself, but speakers can sleep or forget pairings.

## 1. Flash the SD card

1. Run `./setup.sh` once and stop at step 2 (Ctrl+C). Step 1 prints your computer's **public SSH key** and copies it to the clipboard.
2. Open **Raspberry Pi Imager**:
   - **Device:** your board (e.g. *Raspberry Pi Zero 2 W*).
   - **OS:** *Raspberry Pi OS (other) → Raspberry Pi OS Lite (64-bit)*. Use the 32-bit Lite image on a Pi Zero W, Pi 1 or Pi 2.
   - **Storage:** the SD card.
3. Click **Next → Edit settings** (OS customisation):
   - **General:**
     - hostname, e.g. `masjid-radio`
     - username and password (remember them)
     - **Configure wireless LAN**: your Wi-Fi name and password, and the **Wireless LAN country** (required for Wi-Fi to turn on)
     - time zone
   - **Services:** tick **Enable SSH**, then choose **Allow public-key authentication only** and paste the key from step 1.
4. **Save → Yes** and write the card. Put it in the board and power it on. The first boot takes 2–3 minutes.

**Other boards:** flash the vendor's Debian/Ubuntu image (Armbian, DietPi, …). Enable SSH and connect it to the network
with its first-boot tool, or plug in Ethernet for the first run. The wizard can add Wi-Fi afterwards.

## 2. Run the wizard

In a terminal (macOS/Linux) or Git Bash (Windows), in the repository folder:

```powershell
./setup.sh
```

| Step | What happens |
|---|---|
| 1. This PC | Checks for OpenSSH, creates an SSH key if needed, and asks for the GitHub repo used for updates (default: this repo) |
| 2. Device | Asks for the hostname or IP (e.g. `masjid-radio.local`) and username, then waits until it answers |
| 3. Login | Sets up password-less login (asks for the device password once if the key wasn't added in Imager) |
| 4. Inspect | Shows the board, CPU, OS, IP address, Wi-Fi network and Bluetooth |
| 5. Configure | **System update** (OS, kernel, Wi-Fi/Bluetooth drivers and firmware; recommended, 5–20 min, reboots automatically if needed). Optional: Wi-Fi country, an extra/replacement Wi-Fi network (password typed hidden, never stored on this PC), new hostname |
| 6. Release check | Confirms the latest GitHub release has a build for this CPU and that CI is green. Without one, it builds locally (needs Rust) |
| 7. Install | Uploads the installer and runs it (sudo may ask for the device password once). See below |
| 8. Verify | Service running and enabled at boot, web UI healthy, auto-update configured, Wi-Fi power saving off, Bluetooth on |

What the installer configures on the device (`deploy/install.sh`, safe to re-run):

- **Packages:** `bluez`, `bluez-alsa-utils`, `alsa-utils`, `curl`, `iw`, `rfkill`.
- **Wi-Fi:** power saving **off** (it makes Pi Wi-Fi drop off the network), and the optional extra network and country.
- **Bluetooth:**
  - radio unblocked and powered at boot
  - classic-only mode (`ControllerMode = bredr`), fast reconnects, re-pairing without prompts
  - bluez-alsa acting only as an audio *source*
- **Reliability:**
  - hardware watchdog (15 s)
  - network watchdog every 30 s: reconnect, restart networking, reload the driver, reboot after 10 min offline
  - persistent logs capped at 50 MB
- **Service:** `livemasjid` runs as its own user, starts at boot, restarts on failure, and has a systemd watchdog.
- **Updates:** checks GitHub releases hourly, verifies the checksum, health-checks the new version and rolls back on failure.

## 3. First use

Open the address shown at the end (e.g. `http://masjid-radio.local/` or `http://192.168.1.50/`).

1. **Masjids:** search and pick your masjid. It plays whenever the masjid is live, even with the page closed.
2. **Speaker:**
   - **Bluetooth:** put the speaker in pairing mode (usually hold its Bluetooth or power button until it flashes), tap **Scan**, then **Pair**. Tap **Play test sound** to check.
   - **USB/wired:** plug it in; **Fallback output → Automatic** picks it up.
3. **Settings:** optionally set a password for the page.

### Lock it down (recommended)

Pi OS lets the `pi` user run anything as root without a password, and accepts SSH passwords. Once you can sign in with your SSH key
(the wizard sets that up), run this on the Pi once:

```sh
sudo /opt/livemasjid/harden.sh
```

Afterwards:

- SSH accepts keys only.
- `sudo` asks for the password, except for installing a new app binary, which the dev loop (`./deploy.sh`) uses.
- The app's files belong to root.

The app itself never runs as root. It has a few exact sudo rules (rfkill, Bluetooth recovery and the update service).
Updates are signed releases. Also set a password for the web page under Settings › Password.

## 4. Updates and the release pipeline

- Every push to `main` runs GitHub Actions (`.github/workflows/release.yml`):
  - it builds and tests **aarch64**, **armv7** and **armv6** (32-bit tests run under QEMU)
  - it publishes one release with an asset per CPU
- Each device checks hourly and installs the asset for its CPU. **Settings → Check for updates** does it immediately.
- When a release changes system configuration (`deploy/CONFIG_VERSION` increases), the updater re-applies the installer's configuration once.
- Check a device by hand: `ssh pi@<device> sudo /opt/livemasjid/update.sh --check`.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Wizard can't reach the device | Still booting, wrong name, SSH not enabled | Wait 3 min; use the IP from your router; re-flash with SSH enabled |
| Speaker page: *"Your speaker has forgotten this Pi"* | The speaker lost the pairing key (e.g. after pairing with a phone). It accepts the link but refuses audio | Put the speaker in pairing mode and tap **Re-pair**. The Pi also re-pairs by itself when it sees the speaker in pairing mode |
| Speaker connects only sometimes | The speaker is off or sleeping, or out of range | Keep **Keep speaker awake** on, and move the speaker closer. The Pi reconnects automatically |
| Scan finds no devices, or the speaker keeps dropping | The Bluetooth chip is half-hung: it still says "on" but every command times out | Automatic: the network watchdog spots the timeouts in the kernel log within ~1 min and resets the chip. Keep the system updated (re-run `./setup.sh`) |
| *"Bluetooth chip is not responding"* | The Pi's Wi-Fi/Bluetooth chip hung (usually under heavy Wi-Fi load) | Automatic: the app resets the chip. Persistent? Improve the Wi-Fi signal |
| Device drops off the network | Weak Wi-Fi or power saving | The installer disables power saving and the watchdog self-heals. Improve the signal (−65 dBm or better) |
| *"livemasjid.com is refusing requests from this network"* | The website blocked your IP | Playback still works through the stream servers. Ask the livemasjid team to lift the block. Never use a VPN or proxy to get around it |
| No sound at all | No output available | Check the Speaker page for the output in use, and try **Play test sound** |

Logs: `ssh pi@<device> journalctl -u livemasjid -f` (or `./deploy.sh --logs`). The **System → Activity** panel shows the same events.
