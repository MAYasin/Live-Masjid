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
| 9. Lock down | Offers to run `harden.sh` (see [Lock it down](#4-lock-it-down-recommended-once)): SSH keys only, a password for sudo, a firewall for the home network, automatic security updates |

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
3. **Settings › Password:** set a password for the page, so only people who know it can control the radio. You can change or remove it
   there too.

## How the pieces fit

| Piece | Where it runs | What it does |
|---|---|---|
| `setup.sh` (the wizard) | your computer | Connects to the Pi over SSH and runs the installer on it. You use it once per device. |
| `install.sh` (the installer) | the Pi, as root, once | Installs packages, configures Wi-Fi and Bluetooth, and creates the `livemasjid` service and its user. |
| `livemasjid` (the app) | the Pi, as the `livemasjid` user, always | Plays the masjid and serves the web page. It is **not** root and can't change the system. |
| The updater | the Pi, as root, once an hour | Downloads the newest release, checks its digital signature, installs it, and rolls back if it doesn't start. |
| `harden.sh` | the Pi, as root, when you run it | Locks the device down (next section). |

## 4. Lock it down (recommended, once)

A fresh Raspberry Pi OS is set up for convenience, not safety:
- the `pi` account can run any command as the administrator ("root") without typing a password;
- the Pi accepts SSH password logins;
- nothing filters network connections;
- system security fixes are only installed when someone remembers to.

`harden.sh` fixes all of that. **The wizard offers to run it at the end (step 9).** If you skipped that, or set the device up another
way, run it once yourself:

```sh
ssh pi@masjid-radio.local          # sign in to the Pi (use your Pi's name or IP address)
sudo /opt/livemasjid/harden.sh     # type your Pi password when asked
```

`sudo` means "run this as the administrator". It asks for the password you chose in Raspberry Pi Imager. The script takes a minute
or two (it installs two small packages), prints one line per step, and then you're done. Nothing restarts. It is safe to run again
at any time, for example after a reinstall.

**What it changes, in plain words:**

| | Before | After |
|---|---|---|
| **Signing in over SSH** | with a password or a key | **only with your computer's SSH key**. If you have no key on the Pi yet, this step is skipped so you can't lock yourself out. |
| **Administrator commands** (`sudo`) | run without a password | **ask for your Pi password**. The one exception: installing a new copy of the app itself, which the developer's `./deploy.sh` uses. The app runs as an ordinary user, so that can't give anyone administrator rights. |
| **The app's files** | | owned by root, so nothing else can change them |
| **Firewall** | everything open | **only devices on your home network** can reach SSH and the web page. Nothing from the internet gets in, even if a router port is opened by mistake. The Pi can still reach the internet (streams, updates). |
| **Security updates** | manual | **installed automatically every day** (Debian security fixes only; it never reboots by itself) |

**About SSH keys:** after hardening, `ssh pi@…` no longer asks for a password, and that's expected. Your computer proves who you are
with its SSH key, a secret file (`~/.ssh/id_ed25519`) that is far harder to guess than any password. Keep that file private.
If you'd like a prompt anyway, protect the key with a passphrase on your computer: `ssh-keygen -p -f ~/.ssh/id_ed25519`. SSH (and
`./deploy.sh`) then ask for it; `ssh-agent` can remember it for a session.

**Using a VPN such as Tailscale** to reach the Pi from away? Its addresses aren't "home network". Run the script with
`sudo NO_FIREWALL=1 /opt/livemasjid/harden.sh` to skip the firewall. Then open the page by the VPN name and add that name to
`LIVEMASJID_ALLOWED_HOSTS` in `/etc/default/livemasjid`.

**For developers:** `./deploy.sh --scripts --harden` copies the newest helper scripts to the Pi and runs `harden.sh` in one go. It
asks for the Pi password once. A plain `./deploy.sh` never needs it.

## 5. Updates and the release pipeline

- Every push to `main` runs GitHub Actions (`.github/workflows/release.yml`):
  - it builds and tests **aarch64**, **armv7** and **armv6** (32-bit tests run under QEMU)
  - it publishes one release with an asset per CPU
- Each device checks hourly and installs the asset for its CPU. **Settings → Check for updates** does it immediately.
- When a release changes system configuration (`deploy/CONFIG_VERSION` increases), the updater re-applies the installer's configuration once.
- Check a device by hand: `ssh pi@<device> /opt/livemasjid/update.sh --check` (no sudo needed; it changes nothing).

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
| `ssh` no longer asks for a password | Expected after `harden.sh`: your SSH key signs you in | To be asked anyway, add a passphrase to the key: `ssh-keygen -p -f ~/.ssh/id_ed25519` |
| The page or SSH doesn't answer from another network or a VPN | The firewall from `harden.sh` admits the home network only | On the Pi: `sudo ufw allow from <address> to any port 22 proto tcp` (and port 80), or re-run `harden.sh` with `NO_FIREWALL=1` |
| *"Live Masjid doesn't answer to …"* | The page was opened by a name it doesn't know (protection against malicious websites) | Use the IP address or `<name>.local`, or add the name to `LIVEMASJID_ALLOWED_HOSTS` in `/etc/default/livemasjid` |

Logs: `ssh pi@<device> journalctl -u livemasjid -f` (or `./deploy.sh --logs`). The **System → Activity** panel shows the same events.
