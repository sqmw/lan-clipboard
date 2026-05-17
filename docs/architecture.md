# Architecture

## 目标

在同一局域网内，让 macOS 与 Windows 设备共享剪贴板；支持常用类型，并允许配置大小限制。

## 运行形态（推荐）

- **Tauri App（UI）**
  - 负责：设备列表、配对、开关、大小限制配置、状态与日志展示
  - 不承载：协议与加密等核心逻辑（避免 TS 逻辑绑死协议演进）

- **Rust Core（库）**
  - 负责：协议模型、序列化、大小限制判断、去重策略、信任列表、日志

- **Rust Daemon（可选，后续）**
  - 负责：常驻监听剪贴板、网络收发、重试与状态上报
  - UI 通过 IPC 控制 daemon；核心逻辑仍在 core

M0 阶段可先把 daemon 合并在 `src-tauri` 内运行，后续再拆分成独立进程（保持 API 边界不变）。

## 模块边界（src-tauri 现阶段）

- `settings`：大小限制与策略（含持久化）
- `clipboard`：读写剪贴板（平台差异封装）
- `protocol`：消息模型（`ClipboardItem` 等）
- `net`：网络层（监听、连接、收发、加密封装、日志、基础去重/回环抑制）

## 当前网络闭环（M0 已落地）

- 传输：`TCP`（每条消息一行 JSON）
- 拓扑：每端既是服务端（监听）也是客户端（主动向 peers 推送）
- 发现：`mDNS`（`_lan-clipboard._tcp.local.`）用于发现候选设备
- 安全：配对码派生会话密钥，消息以 `AES-GCM-SIV` 加密
- 配置：
  - `peers` 支持 `ip` 或 `ip:port`
  - `poll_interval_ms` 默认 `900`
- 去重策略：
  - 对 payload 做 `SHA-256` 作为 `ClipboardItem.id`
  - 接收远端写入后短时静默，避免 A->B->A 回环
