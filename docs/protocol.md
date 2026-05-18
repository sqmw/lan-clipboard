# Protocol

## “任何类型”的定义（跨 macOS 与 Windows）

严格意义上的“任意剪贴板格式”在跨 OS 情况下不可保证等价（macOS UTI 与 Windows 原生格式不具备完整可逆映射）。

因此本项目采用“**跨平台支持的格式集合**”作为承诺边界：

- `text/plain`
- `image/png`
- 文件列表 / 文件目录打包传输
- `text/html`
- `text/rtf`

对于未知/不支持格式：
- 规则：按策略 **跳过** 或 **降级**（例如只提取 `text/plain` 作为回退）
- 必须带日志与 UI 提示（便于用户理解为什么没同步）

## 消息模型（M0）

`ClipboardItem`：
- `id`：单次复制事件 ID（UUID）
- `content_hash`：内容哈希（本地去抖 / 辅助判重）
- `created_at_ms`：时间戳（毫秒）
- `source_device_id`：来源设备
- `payload`：内容（按类型编码）
- `size_bytes`：内容大小（用于限额判定）

`payload` 采用明确的可移植编码：
- 文本：UTF-8
- 图片：PNG bytes
- 文件列表：`tar` 打包后的 bytes + 顶层名称列表
- HTML/RTF：UTF-8 bytes（必要时附加编码字段）
- 枚举序列化：使用 `bincode` 兼容的外部枚举表示；不要使用 JSON 内部 tag 表示，否则二进制解码会失败

M0 线协议（当前实现）：
- 传输：`TCP`
- 帧格式：`4 bytes` big-endian 长度前缀 + `bincode(WireMessage)`；不再使用一行 JSON 或 base64 body
- 节点关系：每个节点监听本地端口，并向“共享域内自动发现的设备 + 手动配置的兜底地址”主动推送
- 本机触发：监听到本机剪贴板变化后，立即生成一条 `ClipboardItem` 放入发送队列
- 远端触发：收到远端 `ClipboardItem` 后，先进入接收队列，再顺序写入本机剪贴板
- 发现信道：
  - `mDNS` 服务类型 `_lan-clipboard._tcp.local.`
  - UDP 广播心跳端口 `32911`（默认每 `500ms` 发送一次）
- 发现筛选：仅接受 `shared_code` 相同的设备
- 加密封装：`WireMessage`（raw body bytes + 可选 12 bytes nonce）

`WireMessage` 字段：
- `v`：协议版本（当前 `2`）
- `encrypted`：是否加密
- `source_device_id`：发送端设备 ID
- `nonce`：加密随机数（加密时必填，12 bytes）
- `body`：`bincode(ClipboardItem)` 明文或密文主体

`ClipboardItem` 关键语义：
- `id`：单次复制事件 ID，使用 `UUID`
- `content_hash`：内容哈希，用于本地去抖和辅助判重
- `created_at_ms` + `source_device_id`：冲突比较顺序；多设备同时复制时按该顺序收敛

UDP 心跳字段：
- `v`：发现协议版本（当前 `1`）
- `app`：固定为 `lan-clipboard`
- `device_id`：发送端设备 ID，用于避免自发现和成员去重
- `device_name`：展示名
- `shared_code`：共享域筛选字段
- `tcp_port`：该设备的剪贴板 TCP 监听端口

成员缓存策略：
- mDNS 与 UDP 心跳都会合并进同一个共享域成员缓存
- UI 的成员列表读取缓存；“刷新”只做补充扫描，不再把一次空扫描当作远端离线
- 远端停止发送发现信号后，成员缓存会在约 `30s` 后过期

队列策略：
- 发送队列：本机每次监听到新的复制事件，就把该事件入队并顺序广播
- 接收队列：远端事件先入队，再按顺序写入本机剪贴板
- 发送重试：当前轮未发现成员、或只送达部分成员时，事件会短退避后重新入队
- 接收重试：系统剪贴板短时被占用时，事件会短退避后重新尝试写入
- 回环抑制：远端写入本机剪贴板后，短时间内忽略本机监听器看到的回写事件

大内容传输策略：
- 图片的 `size_bytes` 按原始 PNG 字节计算；协议和 TCP 帧都不再额外做 base64 膨胀
- 文件/目录复制会在发送前打成 `tar` bundle，远端解包到临时目录后再写回系统剪贴板
- 若图片超出当前大小限制，发送前会自动等比缩小，直到进入限制范围或无法继续缩小
- 图片重编码使用快速 PNG 压缩；Windows 源端若剪贴板已有原生 `PNG` bytes，则直接复用，避免无意义重编码
- TCP 连接超时保持短超时；TCP 写超时按实际 wire payload 大小动态放宽，避免图片还在传输时被固定短超时中断

低延迟参数：
- 剪贴板监听内部间隔：`50ms`
- 主同步循环空闲间隔：`10ms`
- 队列首轮退避：`30ms`
- 队列最大退避：`500ms`

富文本策略：
- 当前优先识别 `HTML`，其次识别 `RTF`，最后才回退到纯文本
- 富文本当前按 UTF-8 字符串载荷同步，适用于常见网页、文档和编辑器复制场景
- Windows 侧 `CF_HDROP` 文件列表、`PNG` / `CF_DIBV5` / `CF_DIB` / `CF_BITMAP` 图片和 `HTML Format` 走原生剪贴板格式读取；`RTF` 仍保留兼容读取路径
- 私有剪贴板格式、应用内自定义对象和复杂富媒体嵌入仍不在当前协议保证范围内

默认密钥策略：
- 若配置了 `pairing_code`：优先使用 `pairing_code`
- 否则：直接使用 `shared_code`
- 因此“同网段且共享码一致即可加入共享域”的产品语义，与“默认加密仍然存在”的安全语义可以同时成立

## 大小限制

设置项（可持久化）：
- `max_item_bytes`：单条内容上限（默认 256 KiB，统一限制可发送内容大小）

判定逻辑：
- 若超限：本地仍可复制，但 **不对外广播**；记录 reason 与 size，供 UI 展示。
