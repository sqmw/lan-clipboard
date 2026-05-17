# Protocol

## “任何类型”的定义（跨 macOS 与 Windows）

严格意义上的“任意剪贴板格式”在跨 OS 情况下不可保证等价（macOS UTI 与 Windows 原生格式不具备完整可逆映射）。

因此本项目采用“**跨平台支持的格式集合**”作为承诺边界：

- `text/plain`
- `image/png`
- `text/html`（后续）
- `text/rtf`（后续）
- 文件列表（后续）

对于未知/不支持格式：
- 规则：按策略 **跳过** 或 **降级**（例如只提取 `text/plain` 作为回退）
- 必须带日志与 UI 提示（便于用户理解为什么没同步）

## 消息模型（M0）

`ClipboardItem`：
- `id`：去重用（hash）
- `created_at_ms`：时间戳（毫秒）
- `source_device_id`：来源设备
- `payload`：内容（按类型编码）
- `size_bytes`：内容大小（用于限额判定）

`payload` 采用明确的可移植编码：
- 文本：UTF-8
- 图片：PNG bytes
- HTML/RTF：UTF-8 bytes（必要时附加编码字段）
- 文件列表：结构化路径/元数据（跨平台需要协议层抽象）

M0 线协议（当前实现）：
- 传输：`TCP`
- 帧格式：每条 `ClipboardItem` 一行 JSON（`\n` 分隔）
- 节点关系：每个节点监听本地端口，并向配置的 peers 主动推送
- 发现信道：`mDNS` 服务类型 `_lan-clipboard._tcp.local.`
- 加密封装：`WireMessage`（base64 body + 可选 nonce）

`WireMessage` 字段：
- `v`：协议版本（当前 `1`）
- `encrypted`：是否加密
- `source_device_id`：发送端设备 ID
- `nonce_base64`：加密随机数（加密时必填，12 bytes）
- `body_base64`：明文或密文主体

## 大小限制

设置项（可持久化）：
- `max_item_bytes`：单条内容上限（默认 256 KiB，统一限制可发送内容大小）

判定逻辑：
- 若超限：本地仍可复制，但 **不对外广播**；记录 reason 与 size，供 UI 展示。
