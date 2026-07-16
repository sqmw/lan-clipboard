# Status

## 当前代码线

- 应用：`2.0.1`
- TCP 协议：`v5`（与 `v4` 不兼容）
- 平台：macOS、Windows
- 模型：同一配对密钥形成一个共享域；全域共享“单槽位最新值”
- 状态：M5 安全边界收敛完成；验证、残余风险与提交索引见 `docs/todo.md`

从 `1.x`/`v4` 升级时，所有设备必须一起升级。`v4` 可保留同一 26 位配对密钥；应用会在覆盖旧设置前保留 legacy/invalid 原始备份；不会让 `v5` 降级连接旧协议。

## 已支持内容

- 纯文本：`text/plain`
- 图片：PNG；Windows 兼容原生 PNG 与 DIB 读写
- 文件/目录：流式 tar，不落完整 archive
- 富文本：HTML / RTF；两端写入 rich format 与纯文本 fallback

“任意格式”不属于当前承诺。应用私有对象、复杂嵌入和没有跨系统映射的 UTI/clipboard format 会跳过或降级。

## 安全与发现

- 26 位随机 Base32 配对密钥，拒绝明显弱模式。
- mDNS/UDP 只广播 `domain_id` 摘要和有界展示信息，不广播配对密钥。
- 发现结果只是候选；固定长度 HMAC challenge/response/ack 成功后才读取应用帧。
- HKDF 为每条连接派生不同的双向控制/文件密钥和 session id。
- 控制帧强制 AES-256-GCM-SIV，文件帧强制 ChaCha20-Poly1305。
- session、控制序号、transfer UUID、chunk index 和来源设备 UUID都有绑定校验。
- 当前是群组级认证：持有配对密钥的合法成员仍可冒充同组其他 UUID，不是逐设备证书体系。

详细威胁模型与上限见 `docs/security.md`，线格式见 `docs/protocol.md`。

## 发现与成员体验

- mDNS 扫描 + UDP 心跳。
- 多网卡可选择一个已分配 IPv4；TCP 显式绑定源地址，macOS/Windows UDP 通过接口索引约束，自动模式由运行时选择。
- RFC1918 仅用于推荐预筛，不伪造 `/24` 或宣称掌握真实子网掩码。
- UI 手动刷新可以使用未保存的网络选择做预览，但配对密钥始终使用已保存设置；实际运行状态只反映已保存设置。
- discovery 使用 busy-fast singleflight；已有扫描时立即返回进行中，不向 blocking 线程池排队；旧 generation/revision 结果不能覆盖新设置。
- 主动扫描失败后仍可显示同一 generation 的后台缓存，不会每 `1.8s` 无限重扫。
- 后台和手动 mDNS 结果只合并发现观测；空/局部扫描不会把 TTL `30s` 内的 UDP 或 mDNS 成员误显示为“只剩本机”。只有显式变更共享域/网络配置才清空成员缓存。
- 候选字段、数量、探测并发和 UDP 每轮处理时间都有硬限制。

## 可靠性

- 本地 clipboard change token/轮询只生成真实变化事件；启动先建立基线。
- inbound apply、priority outbound、bulk outbound 分 worker 运行。
- 新任务优先旧重试；文本/富文本优先图片，图片优先文件。
- peer 广播最多 8 并发，只重试失败 peer，不重复发送给已成功 peer。
- 暂时没有可用 peer 时，待发送项保留并按有界退避等待发现；成员发现的空快照不会取消已建立的接收传输。
- incoming/outbound socket 都可在 stop 时主动 shutdown，避免设置保存或退出卡在长写超时。
- 文件发送端提前结束或连接异常时，接收端在 EOF/reset/结束帧缺失路径将对应传输标记为失败，避免 UI 永久显示“接收中”；该语义按每条点对点传输连接生效，不代表域级广播。
- 入站总连接 16、单 IP 4、同时大载荷（PNG 或文件）接收 2。
- 文件校验总字节数、chunk 数、完整 SHA-256 和 tar 路径/条目限制。
- 文件源通过 no-follow 句柄读取，并在归档时重算逻辑文件树 hash；Windows tar 路径固定使用 `/`，元数据使用可移植最小值。
- 握手、帧和整段文件接收都有绝对总期限；trickle 流量不能无限占用连接。
- 最近事件 UUID / 已应用 hash 分别限制为 `4096 / 1024` 条，重复 key 不扩张 FIFO。
- 未完成/过期 staging 自动删除；成功写入剪贴板的 bundle 最多保留 8 份且最长 24h。

## 配置与 UI

- 同步与加密是 `v5` 不变量；UI 用只读状态徽标展示“已启用”，不再伪装成可操作的关闭开关。
- IPC 只接受最小 `SettingsUpdate`，不允许前端覆盖内部设备 ID、端口或轮询参数。
- 设置同目录原子替换；运行时应用或持久化失败会回滚旧状态。
- 配对密钥由后端 CSPRNG 生成；UI 提供生成/轮换动作，生成后必须显式保存并同步到其他可信设备。
- autostart 与后端设置按事务协调；IPC 响应丢失时回读核验是否已提交，再决定是否回滚 OS 状态。
- 后端运行时启动失败不会关闭 UI；错误保留在可见状态区，便于修正失效网卡或查看 mDNS 降级原因。
- 默认大小 `256KiB` 可以在 UI 中精确往返，不会因无关保存变成 `1MiB`。
- 表单、状态、扫描和传输反馈使用 aria live/可见提示；保存和扫描都有互斥。
- 开机启动仍由 Tauri autostart plugin 管理。

## 后台与桌面

- 应用启动后默认后台运行，只创建托盘/菜单栏。
- 主窗口按需创建，关闭按钮只隐藏，不退出同步。
- 单实例启动不会创建第二个同步引擎。
- `sync_status` 是纯状态读取，不隐式发起 discovery。
- 同步关闭或运行时重配时 presence 也会停止/更新，不继续泄露旧域广播。

## 关键参数

| 参数 | 值 |
| --- | ---: |
| clipboard 活跃轮询 | `50ms`，空闲退避至 `500ms` |
| 主循环 | 活跃约 `15ms`，空闲约 `80ms` |
| UDP announcement | `500ms` |
| 后台 mDNS refresh | `3000ms` |
| 成员发现 TTL | `30000ms` |
| 前端状态/成员刷新 | `1800ms` |
| 传输进度刷新 | `500ms` |
| 握手读写超时 | `2s` |
| connect timeout | `2s` |
| 帧读取期限 | idle `30s`；总计 `8s..120s` |
| 文件接收期限 | idle `30s`；总计 `31s..30min` |
| discovery 总预算 | 后台 `900ms`；手动 `2200ms` |
| 队列退避 | `30ms..500ms`，最大 `24` 次/`30s` |
| 控制帧内的文本 / HTML / RTF payload | `8MiB` |
| file / image raw frame | `1MiB` |
| 文件读缓冲 | `1MiB` |
| 配置大小范围 | `1 byte..1000MiB` |
| PNG 编码输入安全边界 | `80MiB`（UI 可见；防止解码内存耗尽） |
| 运行日志 | `800` 条；单条 `8KiB`；文件 `2MiB` 轮转 |

时间、频率和阈值的完整生产值/测试方式见 `docs/protocol.md`、`docs/security.md` 与 `docs/dev.md`。

## 吞吐现状与判断边界

- 当前没有纯网络吞吐基线，也没有 v5 PNG 端到端基准；因此不能宣称已经跑满局域网带宽。
- 已保留的一条 v4 文件接收 profile 为 `95,242,752 bytes / 22,878ms = 3.97MiB/s`。它可作为历史观察样本，但不能代表 v5 PNG、当前网络介质或链路上限。
- 数据面使用单 peer 单 TCP 流、`16MiB` socket 收发缓冲和 `1MiB` 认证 raw 分片；分片大小是内存、取消响应和吞吐之间的当前取舍，尚未以基准确定最优值。
- 后续优化必须先拆分测量裸 TCP、加密 raw 流、文件/PNG 读取与写入剪贴板的耗时，再决定是否调整分片大小、调度让出频率或引入多流并行。

## 数据位置

- settings：Tauri per-user app config directory。
- staging、runtime log：运行时推导的 per-user app cache directory。
- 仓库/Syncthing 工作区：不存运行数据库、缓存、日志、密钥副本或下载包。

路径不依赖本机用户名、盘符或长期绝对路径。测试使用独立随机临时目录。

## 已知边界

- 配对密钥泄露后需要在所有设备上轮换，目前没有单成员吊销。
- 群组 PSK 不提供逐设备身份或前向保密；合法/失陷成员可冒充同组 UUID。
- discovery 元数据对局域网可见且可伪造，但伪造候选不能越过 PSK 握手。
- PNG 和文件已经使用受认证的 raw 分片流；“最大同步内容大小”是它们唯一的用户可配总量限制。PNG 另有 UI 可见的 `80MiB` 编码输入安全边界；文本/HTML/RTF 仍为控制帧内存模型，硬上限 8MiB。
- 文件名碰撞尚未覆盖 NFC/NFD 与完整平台 case-fold；队列尚无独立防御性容量常量。
- 16 条入站连接与较大 socket buffer 可形成有界内存峰值，后续调优需同时评估吞吐回归。
- 文件数据面虽已流式化，但不承诺稳定吃满局域网带宽。
- Windows RTF 仍使用有超时和输出上限的 PowerShell compatibility helper；HTML 已走原生 CF_HTML。
- 自动测试不代替 Finder/Explorer、Office/浏览器的真实粘贴 smoke test。

## 后续方向

1. 逐设备密钥、成员撤销与前向保密。
2. 纯网络 benchmark 与数据面分阶段 profiling，再基于实测调整分片/并行策略。
3. 失败原因进一步面向用户细分。
4. 发布前真实双端剪贴板互操作矩阵。
