# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

---

## Development Commands

Frontend & Tauri:
- Install dependencies: `pnpm install`
- Start development server: `pnpm tauri dev`
- Build frontend: `pnpm build`
- Preview production frontend: `pnpm preview`
- Build full Tauri app: `pnpm tauri build`

Testing & Logs:
- Runtime logs are available via the `get_runtime_logs` Tauri command.
- Sync progress via `get_transfer_progress`.

Running specific tasks in Rust/Tauri:
- Start/stop sync: `start_sync` / `stop_sync`
- Device discovery: `discover_devices`
- Settings access: `get_settings` / `set_settings`

---

## High-Level Architecture

### Rust Core (`src-tauri/src`)
- **lib.rs**: Initializes modules, runs main Tauri setup.
- **commands.rs**: Exposes Tauri commands to frontend (settings, clipboard, sync, device discovery, logs).
- **protocol.rs**: Defines `ClipboardPayload` and serialization for clipboard items (text, image, files).
- **state.rs**: Application runtime state, including `AppState` with `Arc<Mutex<...>>`.
- **settings.rs**: User settings handling, serialization, path helpers.
- **desktop.rs**: Handles desktop-specific behavior and shell interactions.
- **net.rs**: Networking, mDNS discovery, UDP heartbeats, TCP transfer logic.

### Frontend (`src`)
- **main.ts**: Vite entry, initializes Tauri frontend, binds commands.
- **i18n.ts**: Internationalization support.
- **styles.css**: Global CSS.
- Assets: SVGs, images, icons for tray and main UI.

### Build & Config
- **package.json / pnpm**: Frontend scripts, devDependencies, Tauri CLI.
- **tsconfig.json**: TypeScript config.
- **vite.config.ts**: Vite bundler config.
- **src-tauri/Cargo.toml**: Rust crate, dependencies, Tauri plugins.
- **src-tauri/tauri.conf.json**: Tauri configuration (tray, bundle, app windows).

### Protocol & Network
- Uses **mDNS** (`_lan-clipboard._tcp.local.`) and **UDP heartbeat** (port 32911) for local device discovery.
- TCP connections for clipboard payload transfer, respecting size limits (~4MB buffer per connection).
- Clipboard payload types: text, image (PNG), file.
- Windows/macOS differences handled: polling vs callback, case-insensitive paths, tray initialization.

### Frontend/Backend Interactions
- Rust exposes commands via `#[tauri::command]`.
- Frontend triggers sync, reads/writes clipboard, displays transfer status.
- Commands include `start_sync`, `stop_sync`, `get_settings`, `set_settings`, `read_clipboard_snapshot`, `write_clipboard_item`, `discover_devices`.

### Key Dev Notes
- Main window not created by default; must use `WebviewWindowBuilder::new(...)` to show.
- Windows: watch for `_.*` resource files; can break UTF-8 parsing.
- Clipboard and file sync deduplicate content by fingerprint.
- Logs and progress critical for debugging transfers.

### References
- `docs/dev.md` for detailed dev setup.
- `docs/architecture.md` for module-level architecture.
- `docs/protocol.md` for protocol details.
- `docs/security.md` for security considerations.
- `docs/status.md` for runtime status and debug info.

---

This CLAUDE.md equips Claude Code with enough context to work effectively with the Rust+Tauri frontend app, understand modules, run development builds, and debug clipboard sync.