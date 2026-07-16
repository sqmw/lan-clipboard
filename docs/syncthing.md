# Syncthing Sync

## Purpose

Keep this repo synced from macOS to the Windows build machine through the existing Syncthing pair.

## Current Mapping

- macOS source folder:
  `/Users/sunqin/study/language/rust/code/lan-clipboard`
- Syncthing folder ID:
  `lan-clipboard-src`
- macOS mode:
  `sendonly`
- Windows target host:
  `sunqin_lenovo_2026@192.168.124.233`
- Windows target path:
  `D:\language\rust\code\lan-clipboard`
- Windows mode:
  `receiveonly`

## Device IDs

- Current Windows:
  `FJ2K2WT-VOS463P-4HZSNCL-AUWFWRY-P6STDGG-Z4YPR3Z-U4ITU22-42G5UQM`
- Historical Windows:
  `AZ6L4MU-4PCBRJY-3P3TJR7-C7GF2FM-MHHLFNG-QU3I5MJ-4UW4UOX-7BFPNQC`
- macOS:
  `LNTEQPR-BYQRI4J-MPO573U-WOJHQVZ-RNSSOM7-H3NFZSM-PSW4ZDL-RMKREAX`

## Windows Setup Script

Use:

`tools/windows/setup-lan-clipboard-syncthing.ps1`

What it does:

- backs up `C:\Users\19519\AppData\Local\Syncthing\config.xml`
- creates or updates folder `lan-clipboard-src`
- points the folder to `F:\language\rust\code\lan-clipboard`
- keeps Windows as `receiveonly`
- asks Syncthing to restart through the local REST API when the API key is present

## Remote Execution Example

```bash
scp tools/windows/setup-lan-clipboard-syncthing.ps1 19519@192.168.0.106:C:/Users/19519/Desktop/
ssh 19519@192.168.0.106 "powershell -ExecutionPolicy Bypass -File C:\\Users\\19519\\Desktop\\setup-lan-clipboard-syncthing.ps1"
```

## Verification

- Windows `config.xml` contains folder `lan-clipboard-src`
- Windows path `D:\language\rust\code\lan-clipboard` exists
- Syncthing GUI on Windows shows the folder and remote peer connected
- a file created under this repo on macOS appears on Windows after sync

## Ignore Rules

Windows is a build machine, so generated local artifacts must stay ignored and deletable.

- Use `(?d)` for ignored build directories that Syncthing may need to delete after the macOS sender removes or does not have them.
- Current important ignored generated paths:
  `node_modules/`
  `dist/`
  `src-tauri/target/`
  `src-tauri/gen/schemas/`
  `.pnpm-store/`
  `pnpm-workspace.yaml`

## Current Status

As of `2026-07-10`, new Lenovo Windows is connected and this folder is configured as receive-only.

Observed evidence:

- macOS and `DESKTOP-F4B6E6C` are connected.
- `lan-clipboard-src` exists on Windows at `D:\language\rust\code\lan-clipboard`.
- Windows mode is `receiveonly`.
- A build created `src-tauri\target` and local `pnpm-workspace.yaml`; Syncthing reported that `src-tauri\target` needed a deletable ignore prefix.
- `.stignore` was updated so generated build directories use `(?d)` and the local pnpm approval file is ignored.
- After the ignore fix, the remaining Windows-side Syncthing error was not a missing ignore rule: `pnpm tauri dev` was still running and `target\debug\lan-clipboard.exe` was locked, so Syncthing could not delete the previously indexed `src-tauri\target` directory. Stop the dev process or close the running app, then rescan `lan-clipboard-src`.
- On `2026-07-10`, after the app process was stopped, the remaining `src-tauri\target` cache was still hard to delete in place. The folder was temporarily paused in Syncthing, then the stale target directory was moved out of the synced tree to:
  `D:\build-cache\syncthing-quarantine\lan-clipboard\target-20260710-154707`
- Final verification on Windows after rescan:
  `state=idle`
  `errors=0`
  `needFiles=0`
  `localChanged=0`

## Build Artifact Recovery

If Windows build output makes `lan-clipboard-src` appear locally changed again:

1. Stop the running Tauri app or dev command first.
2. Keep generated output ignored through `.stignore`; `src-tauri/target/` must keep the `(?d)` prefix.
3. If Windows cannot delete `src-tauri\target` in place, move it to a non-synced cache or quarantine directory such as:
   `D:\build-cache\syncthing-quarantine\lan-clipboard\...`
4. Trigger a Syncthing rescan for `lan-clipboard-src` and verify:
   `errors=0`
   `needFiles=0`
   `localChanged=0`
