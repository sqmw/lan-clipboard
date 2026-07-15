# Architecture

## 目标与边界

LAN Clipboard 在同一局域网内同步 macOS / Windows 的常用剪贴板格式。Tauri 负责桌面生命周期和 UI，Rust core 独占设置、剪贴板、协议、发现、认证、队列与资源边界；前端不实现安全协议。

当前进程内同时包含 UI 与同步引擎，暂不拆 daemon。若未来拆进程，应保持 `settings / clipboard / protocol / net` 的现有边界，不把 wire 或平台路径逻辑搬到 TypeScript。

## 顶层模块

| 模块 | 单一职责 |
| --- | --- |
| `desktop` | 托盘、按需主窗口、单实例与退出行为 |
| `commands` | 最小 Tauri IPC、设置事务、发现 singleflight、状态 DTO |
| `settings` | 配置模型、规范化、验证、原子持久化、备份与恢复 |
| `storage` | 运行时推导的用户级缓存目录与安全文件类型检查 |
| `clipboard` | 平台剪贴板读取/写入协调及格式子模块 |
| `protocol` | 仅本机使用的 `ClipboardItem / ClipboardPayload` 模型 |
| `net` | 同步引擎门面、presence 与网络运行时 |
| `state` | Tauri `AppState`，持有设置 revision、singleflight 和服务实例 |

本机 `ClipboardPayload` 故意不实现 serde；任何含 `PathBuf` 的 variant 都不能跨 IPC 或网络边界。

## Clipboard 子模块

| 子模块 | 职责 |
| --- | --- |
| `files` | 文件列表、流式 tar、精确大小估算、解包与剪贴板回写 |
| `file_access` | no-follow 句柄打开、文件身份与遍历前后元数据复核 |
| `path_policy` | staging lease、containment、可移植路径和保留策略 |
| `fingerprint` | 内容语义指纹，文件使用结构与完整内容哈希 |
| `image_payload` | PNG、Windows DIB/bitmap 转换、尺寸/像素/源大小限制 |
| `rich_text` | macOS 原生 HTML/RTF、Windows CF_HTML 与受限 helper |
| `platform` | 平台 change token 与小型系统辅助 |
| `types` | `ClipboardError` 与写回结果 |

新增格式应进入对应子模块；`clipboard/mod.rs` 只维护优先级、统一限额和短暂 clipboard busy 重试。

## Net 子模块

### 控制与状态

| 子模块 | 职责 |
| --- | --- |
| `state` | `RuntimeInner`、socket registry、队列/缓存/进度容器 |
| `lifecycle` | TCP/UDP 监听、worker 创建、readiness 与主循环 |
| `workers` | inbound apply 与两类 outbound 调度循环 |
| `domain` | 当前可见 peer 与过期队列/staging 清理；发现瞬态不作为传输终止信号 |
| `queue` | lane、优先级、失败 peer 与有界退避 |
| `transfers` | UI 可消费的传输状态与保留 |
| `logs` | 有界内存日志、错误状态和安全落盘轮转 |

### 发现与认证

| 子模块 | 职责 |
| --- | --- |
| `presence` | mDNS 注册与重试；同步禁用时停止广播 |
| `discovery` | mDNS 扫描、字段校验、网卡选择、有界可达性探测 |
| `udp` | 域摘要心跳、每轮接收预算与候选合并 |
| `members` | mDNS/UDP 观测合并，以及 discovered/seen/known 的 TTL、上限与驱逐 |
| `handshake` | 固定长度 PSK challenge/response/ack 与 per-connection session |
| `crypto` | domain id、HKDF/HMAC、控制/文件 AEAD 原语 |

### 数据面

| 子模块 | 职责 |
| --- | --- |
| `wire` | portable DTO、控制/raw frame codec、序号/AAD/大小验证 |
| `socket` / `udp_socket` | TCP 源地址、握手/传输 timeout 与 UDP 接口约束 |
| `sender` | 有界 peer 并发、认证、发送和失败 peer 报告 |
| `inbound` | 认证前连接配额、解帧、文件接收与入队 |
| `file_stream` | raw frame reader/writer、摘要、进度和取消点 |
| `flow` | 队列消费、写回、重试、过期与 staging 生命周期 |
| `watch` | 平台 change token/轮询、本机事件生成与基线 |
| `dedupe` / `marker` | 回环抑制、事件身份与单槽位最新值 |
| `item` / `display` / `metrics` | item 构造、UI 摘要与小型统计工具 |

`net/mod.rs` 只保留公开门面和 `SyncEngine / PresenceService` 协调，不再承载 codec、连接或发现细节。

## 线程与所有权

```text
Tauri main thread
  ├─ PresenceService worker
  └─ SyncEngine main worker
       ├─ clipboard watch worker
       ├─ inbound apply worker
       ├─ outbound priority worker
       ├─ outbound bulk worker
       └─ bounded incoming connection workers
```

关键规则：

- `RuntimeInner` 由 `Arc` 持有；禁止 detached raw pointer 或超出 owner 生命周期的引用。
- 每条 TCP 连接独占 `Session`，session key/sequence 不进入全局缓存，也不跨 peer 复用。
- 入站 socket 总计 `16`、单 IP `4`；同时文件接收 `2`；发送 peer 并发 `8`。
- incoming/outbound socket 都登记可关闭 clone。`stop()` 先置 stop flag 并 `Shutdown::Both`，再 join。
- worker 创建或 readiness 失败由 startup guard 回滚 `running/stop/worker` 状态。
- discovery 和 connect 都有绝对总期限；握手、帧读取和文件接收不能靠少量 trickle 流量无限续期；主循环的 UDP 消费有包数/时间预算。

## 发送数据流

```text
system clipboard change
  -> clipboard::read_snapshot
  -> item::build_item + semantic hash
  -> flow::enqueue_outbound_item
  -> queue lane selection
  -> collect current discovered peers
  -> no peer: bounded discovery retry
  -> bounded sender workers
  -> TCP connect -> client_handshake
  -> per-session encode/encrypt/write
  -> record only failed peers for retry
```

文件 payload 在握手后发送 start control frame，然后通过 no-follow 已打开句柄边生成可移植 tar、边计算逻辑文件树 hash、边发 raw frame。只有流式 hash 与复制时捕获的 `content_hash` 一致才 flush 尾帧并发送 end frame；源文件变化、特殊文件、链接或传输被新事件替代都会终止本次流。

## 接收数据流

```text
TCP accept
  -> global/per-IP connection quota
  -> fixed-size server_handshake
  -> dynamic authenticated frame limit
  -> portable DTO validation
  -> inbound queue
  -> clipboard apply worker
  -> system clipboard
```

文件 start frame 先取得 active receive permit，再创建本机 `ReceivedBundle`。raw reader 直接喂给受限 tar 解包；任何错误由 RAII 删除 bundle。完整性通过后才把本机路径装入 `FileBundleDir` 入队，成功写回后转为有限期保留，失败/过期/停止则删除。

## 配置事务

前端只提交 `SettingsUpdate`：大小字节数、配对密钥、local IP、语言、开机启动。device UUID、端口、内部轮询和强制加密不在 IPC 更新 DTO 中。

```text
validate update
  -> snapshot previous settings/runtime/cache
  -> apply next runtime if needed
  -> atomically persist settings
  -> publish in-memory settings + increment revision
  -> only explicit discovery-setting changes clear cache; routine scans merge observations
```

任何阶段失败都恢复 previous runtime；异步 discovery 写回前必须核对 revision。discovery 在进入 blocking 线程池前取得 busy-fast lease；已有扫描时直接返回“进行中”，不排队也不合并出第二个 mDNS 任务。

开机启动由操作系统插件持有，不属于后端 JSON 原子写。前端先变更 OS 状态、再提交后端；后端 IPC 报错时先回读全部用户字段判断是否已提交，确认未提交才回滚 OS 状态，回读失败则明确报告提交状态未知与回滚结果。

## 前端边界

- `src/main.ts`：启动、状态编排、保存/刷新互斥与 generation。
- `src/settingsForm.ts`：字节精确表单、配对密钥校验和静态文案。
- `src/deviceList.ts`：候选缓存、网络过滤与 DOM 渲染。
- `src/transferProgress.ts`：传输卡片与预览。
- `src/i18n.ts` / `src/types.ts`：双语键与 IPC DTO。
- `src/styles/`：按区域维护样式；交互状态必须有人类可见反馈和 aria live 状态。

前端缓存永远只是展示优化，不能决定认证或传输授权。`sync_status` 只读取已保存配置对应的实际运行状态；未保存的网络选择只影响手动 discovery 预览，配对密钥始终读取已保存后端设置。

## 持久化位置

- 设置：Tauri per-user app config directory。
- staging、运行日志：运行时推导的 per-user app cache directory。
- 仓库、Syncthing 目录、当前工作目录：不保存数据库、日志、缓存、密钥副本或接收文件。

代码和文档不得写入依赖本机用户名或盘符的长期绝对路径；测试使用进程号 + UUID 的隔离临时目录。
