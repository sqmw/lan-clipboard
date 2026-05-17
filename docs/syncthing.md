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
  `19519@192.168.0.106`
- Windows target path:
  `F:\language\rust\code\lan-clipboard`
- Windows mode:
  `receiveonly`

## Device IDs

- Windows:
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
- Windows path `F:\language\rust\code\lan-clipboard` exists
- Syncthing GUI on Windows shows the folder and remote peer connected
- a file created under this repo on macOS appears on Windows after sync

## Current Blocker

As of `2026-05-16`, Windows already contains the new folder entry, but the device session is not staying connected.

Observed evidence on Windows:

- `lan-clipboard-src` is present in `config.xml`
- `F:\language\rust\code\lan-clipboard` has been created
- Syncthing REST status for `lan-clipboard-src` is `idle` with `globalTotalItems = 0`
- the device connection to the Mac is currently `connected = false`
- `syncthing.log` shows the reconnect is being broken by an existing folder mismatch:
  `codex-global-agents` fails with `remote expects to exchange plain data, but local data is encrypted`

Implication:

- this repo's Syncthing folder is configured on Windows
- actual file transfer will not start until the pre-existing `codex-global-agents` encryption mismatch is resolved or that folder is paused/removed from the pair
