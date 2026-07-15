<p align="center">
  <img src="./docs/image/lan-clipboard-logo.svg" width="128" alt="LAN Clipboard Logo">
</p>

<h1 align="center">lan-clipboard</h1>

<p align="center">
  <b>Share clipboard across macOS / Windows on the same LAN.</b><br>
  Tauri UI · Rust Core · Shared-domain auto discovery
</p>

<p align="center">
  <a href="https://github.com/sqmw/lan-clipboard/stargazers"><img src="https://img.shields.io/github/stars/sqmw/lan-clipboard?style=for-the-badge&color=f5c542" alt="stars"></a>
  <a href="https://github.com/sqmw/lan-clipboard/releases/latest"><img src="https://img.shields.io/github/v/release/sqmw/lan-clipboard?style=for-the-badge&color=6c63ff" alt="release"></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-MIT-brightgreen?style=for-the-badge" alt="license"></a>
  <img src="https://img.shields.io/badge/platform-Windows%20%7C%20macOS-lightgrey?style=for-the-badge" alt="platform">
</p>

<p align="center">
  <a href="./docs/README.md">📖 Docs</a> ·
  <a href="./docs/dev.md">🧰 Dev Guide</a> ·
  <a href="https://github.com/sqmw/lan-clipboard/issues">🐛 Issues</a> ·
  <a href="https://github.com/sqmw/lan-clipboard">⭐ Star</a>
</p>

<p align="center">
  <a href="README.md"><b>🇨🇳 中文</b></a> | <b>🇬🇧 English</b>
</p>

---

## 🖼️ UI Screenshot

<p align="center">
  <img src="./docs/image/screen-shot.png" width="920" alt="LAN Clipboard UI Screenshot">
</p>

<p align="center"><i>Built around shared-domain status, transfer visibility, and only the configuration users actually need.</i></p>

## 🎯 Design Principles

- **Minimal first**: the UI keeps only the pieces that matter in daily use, such as domain members, network selection, size limit, and transfer progress
- **Efficiency first**: the app is designed around “open, join by shared code, copy, sync” instead of exposing a complex connection flow
- **Debuggable without clutter**: logs, progress, and member state stay available, but the main screen remains quiet and focused

## ✨ Features

- **Shared-domain model**: devices with the same high-entropy 26-character pairing key on the same LAN join one domain automatically
- **Shared-domain debounce**: the same content is allowed only one effective send in the domain; duplicates are dropped both while the first send is still in flight and after it has already synced successfully
- **Auto discovery**: `mDNS + UDP heartbeat` maintain a bounded candidate cache; trust starts only after the fixed-size TCP handshake
- **Event-driven sync**: clipboard changes are queued and pushed to peers via TCP binary frames
- **Common payloads**: plain text / PNG image / files & folders / basic rich text (HTML/RTF)
- **Authenticated encryption**: every connection gets independent session keys; control and file frames are always encrypted with no plaintext downgrade
- **Multi-NIC support**: pick the correct local IP to avoid virtual adapters on Windows
- **Debug friendly**: transfer progress, type, and previews; logs live under “Advanced / Logs”

## 🚀 Quick Start (Windows + macOS)

1. Connect both devices to the same LAN and open the app.
2. Copy the app-generated 26-character pairing key from one device to the other, then click “Save”.
3. If you have multiple NICs/virtual adapters, pick the correct local IP in “Network”, then save.
4. Click “Refresh” and confirm the peer shows up in the members list.
5. Copy text/images/files(or folders)/rich text on one device, then paste on the other.

Additional note:
If the same file or clipboard content is copied repeatedly in a short burst, the app guarantees only the first effective sync. Later duplicate copies are dropped before send to reduce loop risk and avoid wasting bandwidth.

Upgrading from `1.x / v3` to `2.0.0 / v4` requires upgrading every device and pairing again. The old six-digit-code settings are backed up before migration; old and new protocols do not interoperate and never downgrade to plaintext.

## 📚 Docs

- `docs/README.md`: docs index
- `docs/status.md`: supported scope, limits, and key parameters (including throughput notes)
- `docs/dev.md`: development / debugging guide
- `docs/todo.md`: milestones and TODOs

## ⚠️ Current Limits

- “Any type” does not mean perfect parity for app-private clipboard formats; we only promise a cross-platform supported set (see `docs/protocol.md`)
- Throughput is not yet tuned to consistently saturate LAN bandwidth for large files/images; more data-plane optimizations are planned (see “Throughput” in `docs/status.md`)
- Authentication is scoped to the group holding one pairing key, not per-device certificates. Share that key only with trusted devices (see `docs/security.md`)
