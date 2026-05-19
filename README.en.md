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

- **Shared-domain model**: devices with the same 6-digit shared code on the same LAN join one domain automatically
- **Auto discovery**: `mDNS + UDP heartbeat` maintain a live member cache; click “Refresh” for an instant scan
- **Event-driven sync**: clipboard changes are queued and pushed to peers via TCP binary frames
- **Common payloads**: plain text / PNG image / files & folders / basic rich text (HTML/RTF)
- **Encrypted transfer**: toggleable, derived from the shared code by default
- **Multi-NIC support**: pick the correct local IP to avoid virtual adapters on Windows
- **Debug friendly**: transfer progress, type, and previews; logs live under “Advanced / Logs”

## 🚀 Quick Start (Windows + macOS)

1. Connect both devices to the same LAN and open the app.
2. Set the same 6-digit shared code on both sides and click “Save”.
3. If you have multiple NICs/virtual adapters, pick the correct local IP in “Network”, then save.
4. Click “Refresh” and confirm the peer shows up in the members list.
5. Copy text/images/files(or folders)/rich text on one device, then paste on the other.

## 📚 Docs

- `docs/README.md`: docs index
- `docs/status.md`: supported scope, limits, and key parameters (including throughput notes)
- `docs/dev.md`: development / debugging guide
- `docs/todo.md`: milestones and TODOs

## ⚠️ Current Limits

- “Any type” does not mean perfect parity for app-private clipboard formats; we only promise a cross-platform supported set (see `docs/protocol.md`)
- Throughput is not yet tuned to consistently saturate LAN bandwidth for large files/images; more data-plane optimizations are planned (see “Throughput” in `docs/status.md`)
