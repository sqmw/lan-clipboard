<p align="center">
  <img src="./docs/image/lan-clipboard-logo.svg" width="128" alt="LAN Clipboard Logo">
</p>

<h1 align="center">lan-clipboard</h1>

<p align="center">
  <b>同一局域网内的 macOS / Windows 共享剪贴板。</b><br>
  Tauri UI · Rust Core · 共享域自动发现
</p>

<p align="center">
  <a href="https://github.com/sqmw/lan-clipboard/stargazers"><img src="https://img.shields.io/github/stars/sqmw/lan-clipboard?style=for-the-badge&color=f5c542" alt="stars"></a>
  <a href="https://github.com/sqmw/lan-clipboard/releases/latest"><img src="https://img.shields.io/github/v/release/sqmw/lan-clipboard?style=for-the-badge&color=6c63ff" alt="release"></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-MIT-brightgreen?style=for-the-badge" alt="license"></a>
  <img src="https://img.shields.io/badge/platform-Windows%20%7C%20macOS-lightgrey?style=for-the-badge" alt="platform">
</p>

<p align="center">
  <a href="./docs/README.md">📖 文档</a> ·
  <a href="./docs/dev.md">🧰 开发/联调</a> ·
  <a href="https://github.com/sqmw/lan-clipboard/issues">🐛 反馈</a> ·
  <a href="https://github.com/sqmw/lan-clipboard">⭐ Star</a>
</p>

<p align="center">
  <b>🇨🇳 中文</b> | <a href="README.en.md"><b>🇬🇧 English</b></a>
</p>

---

## ✨ 特性

- **共享域模型**：同一局域网内填写相同 6 位共享码的设备自动加入同一共享域
- **自动发现**：`mDNS + UDP 心跳` 维护成员缓存；点击“刷新”可立即补充扫描
- **事件驱动同步**：剪贴板变化入队后以 TCP 二进制帧推送到共享域成员
- **常用类型**：文本 / 图片(PNG) / 文件与目录 / 基础富文本(HTML/RTF)
- **加密传输**：可开关，默认使用共享码派生密钥
- **多网卡支持**：可选择本机使用网络，避免虚拟网卡影响发现
- **调试友好**：发送/接收进度、类型与预览展示；日志入口收纳在“高级/日志”

## 🚀 快速开始（Win + macOS）

1. 两台设备连接同一局域网并启动应用。
2. 在两端设置相同的 6 位共享码，点击“保存配置”。
3. 如遇多网卡/虚拟网卡，先在“使用网络”选择实际局域网 IP，再保存。
4. 点击“刷新”确认成员列表出现对端设备。
5. 在任意一端复制文本/图片/文件(目录)或富文本，对端可直接粘贴。

## 📚 文档入口

- `docs/README.md`：文档总入口
- `docs/status.md`：当前支持、边界、关键参数（含吞吐说明）
- `docs/dev.md`：开发 / 联调 / 排障
- `docs/todo.md`：里程碑与待办

## ⚠️ 当前边界

- “任何类型”不等于“任意私有剪贴板格式完全等价”；以跨平台支持的格式集合为边界（见 `docs/protocol.md`）
- 当前版本仍未把局域网传输速度稳定优化到“吃满带宽”；对大文件/大图片高吞吐场景仍需继续优化（见 `docs/status.md` 的“吞吐说明”）
