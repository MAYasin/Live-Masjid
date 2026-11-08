# Security policy

## Reporting a vulnerability

Please report security problems privately through GitHub:
**Security → Report a vulnerability** on this repository
(<https://github.com/MAYasin/Live-Masjid/security/advisories/new>).

Do not open a public issue for a vulnerability. You can expect an acknowledgement within a few days.

## Scope

- The web UI and API served by the Pi (authentication, request handling, file access).
- The installer and updater scripts in `deploy/`, `setup.sh` and `deploy.sh`.
- The release pipeline (`.github/workflows/release.yml`). Devices install the release assets, so its integrity matters.

## Supported versions

Only the latest release is supported. Devices update themselves from it.

## Notes for deployers

- The UI is meant for a trusted LAN. Set a password under Settings, and do not expose port 80 to the internet.
- Each release asset is signed with an Ed25519 key (`.sig` next to the `.tar.gz`). The public key is `deploy/release.pub`; a device stores it in `/etc/livemasjid/release.pub` on first install and `update.sh` refuses any update whose signature does not verify. A SHA-256 checksum is checked as well.
