# Protocol

## 承诺边界

跨 macOS 与 Windows 只承诺以下可移植格式：

- `text/plain`
- `image/png`
- 文件与目录组成的文件列表
- `text/html`
- `text/rtf`

应用私有对象、任意 UTI、任意 Windows clipboard format 和复杂富媒体嵌入不保证跨平台等价。未知格式会跳过或降级到纯文本，并留下可诊断状态。

## 版本与兼容性

- 应用版本：`2.0.0`
- TCP 线协议：`v4`
- UDP announcement：`v2`
- `v4` 是从旧 `v3` 的不兼容升级；没有旧帧 fallback，也没有可关闭的加密路径。
- 所有设备必须一起升级并使用新的 26 位配对密钥。

## 模型分层

本机模型 `ClipboardPayload` 不实现 serde，可包含本机 `PathBuf`：

- `FileList`：发送端系统剪贴板返回的源路径。
- `FileBundleDir`：接收端本机生成的 staging 路径。

线上模型 `WireClipboardPayload` 只包含 UTF-8 字符串或 bytes，文件路径永不进入线上 DTO。文件只在线上传输顶层可移植名称和 tar 字节流。

`ClipboardItem` 的公共语义：

- `id`：非 nil UUID，标识一次复制事件。
- `source_device_id`：非 nil UUID，必须与本连接认证的 peer 一致。
- `content_hash`：64 位十六进制内容指纹。
- `created_at_us`：事件时间；最多允许比接收端当前时间快 `5min`。
- `size_bytes`：必须等于解码后的实际大小，且不能超过对应硬限制。
- `payload`：可移植内容。

## 发现协议

### mDNS

- service type：`_lan-clipboard._tcp.local.`
- instance/device id：规范小写 UUID。
- TXT：`device_id`、`device_name`、`domain_id`。
- port：`1024..=65535`。

### UDP

- 端口：`32911`
- announcement version：`2`
- 字段：`app`、`device_id`、`device_name`、`domain_id`、`tcp_port`
- 默认发送间隔：`500ms`

`domain_id` 是配对密钥经域分离 SHA-256 后的 16-byte 十六进制摘要，只用于减少无关候选；发现结果不是认证结果。输入会验证 UUID、名称长度/控制字符、端口和可用 IPv4。

发现资源限制：mDNS 每次最多消费 `256` 个 resolved 事件并保留 `100` 个候选；可达性探测最多 `8` 个线程且最多占 `400ms`，daemon shutdown 最多占 `100ms`；后台扫描总预算 `900ms`，手动扫描总预算 `2200ms`；UDP 每个主循环最多消费 `64` 包或 `3ms`。

多网卡推荐只把 RFC1918 地址按 `10/8`、`172.16/12`、`192.168/16` 做候选预筛，不伪造 `/24` 子网掩码。显式选择 IPv4 时，TCP connect/探测绑定该源地址；UDP 仍绑定共享 wildcard 端口接收广播，但在 macOS/Windows 由内核接口索引限制收发，约束失败即禁用 UDP，不静默退回任意接口。

## TCP 会话

连接角色由 TCP 决定：connector 永远是握手 client 和应用 sender，acceptor 永远是握手 server 和应用 receiver。双向同步使用两条独立连接，不按设备名或剪贴板方向猜角色。

任何长度前缀之前先执行固定长度 PSK 握手：

```text
server -> client  Challenge  (88 bytes)
client -> server  Response   (88 bytes)
server -> client  Ack        (56 bytes)
```

消息头包含 `LCB4` magic、version、kind 和保留位；保留位必须为零。消息体绑定双方 UUID、32-byte 随机 nonce 和 HMAC-SHA256。完整 transcript 经 HKDF-SHA256 派生：

- `session_id[16]`
- client→server / server→client control key
- client→server / server→client raw key

生产握手完整 challenge/response/ack 共用一个 `2s` 绝对期限，握手 socket 缓冲约 `16KiB`。认证成功后才切换为按实际 payload 大小计算的 `8s..120s` 帧总期限和较大 socket 缓冲；帧读取另有 `30s` idle 上限，任一更短期限先到即失败。

## 控制帧

```text
u32_be frame_len
u8     version = 4
u8     flags = encrypted
bytes  session_id[16]
u64_be control_sequence
bytes  nonce[12]
bytes  AES-256-GCM-SIV(ciphertext || tag)
```

AEAD AAD 是 version/flags/session/sequence 固定头。每条连接的 control sequence 从 `0` 开始，必须严格相等、成功认证与解析后才递增，溢出即关闭连接。

密文内是有上限、拒绝 trailing bytes 的 `bincode(EncodedWireBody)`：

- `ClipboardItem`
- `FileStreamRawStart`
- `FileStreamEnd`

文本、图片和富文本的实际 payload 硬上限为 `8MiB`；控制帧只额外允许固定 `256KiB` 协议开销。该限制独立于文件总大小设置，用于避免单帧巨额内存分配。

## 文件流

文件传输顺序：

```text
control seq 0: FileStreamRawStart
raw chunks 0..N-1
control seq 1: FileStreamEnd
```

raw frame：

```text
u32_be frame_len
u8     version = 4
u8     flags = encrypted
bytes  session_id[16]
bytes  transfer_uuid[16]
u64_be chunk_index
bytes  nonce[12]
bytes  ChaCha20-Poly1305(ciphertext || tag)
```

raw AAD 绑定固定头。chunk index 必须从 `0` 连续增长；每帧明文最多 `16MiB`。`FileStreamRawStart` 声明总大小、chunk 数、来源 UUID和顶层名称；结束帧再次提交 item id、实际 chunk 数和全流 SHA-256。

发送端通过 no-follow 句柄边遍历源文件、生成 tar、计算逻辑文件树 hash、加密和写网；只有该 hash 与复制时捕获值一致才发送结束帧。tar 中 file/dir mode 固定为 `0644/0755`，`uid/gid/mtime` 固定为 `0`，目录分隔符固定为 `/`。接收端边解密、校验和解包，不生成完整 archive 临时文件；两端都会验证实际 tar 字节数等于声明值。

单帧 idle 期限为 `30s`。整段文件接收另有绝对总期限：`30s + ceil(total_bytes / 1MiB/s)`，最小实际值 `31s`、最大 `30min`；trickle 数据只能刷新 idle 期限，不能延长总期限。

归档限制：

- 最大 `20,000` 个条目、深度 `32`、顶层名称 `256` 个。
- 拒绝链接、reparse point、特殊文件、绝对路径、父级跳转、重复/折叠冲突路径。
- 每个组件必须是 UTF-8 可移植名称；拒绝 Windows ADS、设备名和尾随点/空格。
- 只允许发送端声明过的顶层名称，结束时声明集合必须全部出现。
- 同时接收文件最多 `2` 个。

## 队列与最新值

共享域是单槽位最新值模型。比较键为：

```text
created_at_us -> source_device_id -> id
```

新事件会淘汰旧队列/旧传输；远端写回通过共享槽位指纹、inflight 指纹、短期忽略哈希和内部 staging 路径共同防回环。

网络派生去重状态同样有硬上限：最近事件 UUID 保留 `120s`、最多 `4096` 条；已应用内容 hash 保留 `10s`、最多 `1024` 条。两者按 FIFO 淘汰，重复 key 不扩张 sidecar。

发送调度：

```text
新任务 > 旧重试
text/html/rtf > image > file
同层较新事件优先
```

peer 广播并发最多 `8`。一轮只记录失败 peer；重试不会再次发送给已经成功的 peer。首轮退避 `30ms`，最大退避 `500ms`，最多 `24` 次且总年龄不超过 `30s`。

接收写回遇到剪贴板短暂占用时使用同一有界退避；永久错误、过期事件和停止同步都会清理未使用的文件 staging。

## 大小参数

- 初始 `max_item_bytes`：`256KiB`。
- 可配置范围：`1 byte..=1000MiB`；UI 以 MiB 显示但按字节精确往返。
- 文件 tar 总流必须不超过 `max_item_bytes`。
- 文本/PNG/HTML/RTF 还受 `8MiB` 单帧硬上限约束。
- 超限内容保留在本地剪贴板，不广播，并记录明确错误。

测试态使用小 payload、loopback、直接 codec 和可注入接收期限快速覆盖边界；生产大小、连接数、`2s` 握手总期限、`31s..30min` 文件总期限和 `30s` 重试总年龄不会为了测试而写死成更小值。
