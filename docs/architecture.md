# Architecture

## 目标

在同一局域网内，让 macOS 与 Windows 设备共享剪贴板；支持常用类型，并允许配置大小限制。

## 运行形态（推荐）

- **Tauri App（UI）**
  - 负责：共享域成员列表（本机作为根节点，展开后显示其他成员）、共享码配置、开关、大小限制配置、状态与日志展示
  - 不承载：协议与加密等核心逻辑（避免 TS 逻辑绑死协议演进）
  - 单实例：重复打开应用时聚焦已有窗口，避免多个进程争抢同一个同步监听端口
  - 当前前端入口只保留启动编排、Tauri 命令调度和定时刷新；传输进度、设备列表、设置表单、通用类型与 HTML 转义已经拆出，避免继续向 `src/main.ts` 堆积 UI 细节

- **Rust Core（库）**
  - 负责：协议模型、序列化、大小限制判断、去重策略、共享域发现、日志

- **Rust Daemon（可选，后续）**
  - 负责：常驻监听剪贴板、网络收发、重试与状态上报
  - UI 通过 IPC 控制 daemon；核心逻辑仍在 core

M0 阶段可先把 daemon 合并在 `src-tauri` 内运行，后续再拆分成独立进程（保持 API 边界不变）。

## 模块边界（src-tauri 现阶段）

- `settings`：大小限制与策略（含持久化）
- `clipboard`：剪贴板读写协调入口，按当前系统剪贴板内容选择文件、图片、富文本或纯文本链路
  - `clipboard::files`：文件列表读取/写入、`tar` bundle 流式归档、接收端解包、内部临时目录识别和文件列表内容哈希回写
  - `clipboard::fingerprint`：剪贴板内容语义指纹，负责文本/图片/富文本/文件列表/文件包的哈希与部分内容采样
  - `clipboard::image_payload`：图片读取/写入、PNG 快速编码、超限缩放和 Windows 原生 `PNG / DIB / Bitmap` 兼容
  - `clipboard::platform`：平台级剪贴板辅助能力，例如 Windows 子进程隐藏窗口
  - `clipboard::rich_text`：`HTML / RTF` 读取、写入、Windows `CF_HTML` 解析和纯文本 fallback
  - `clipboard::types`：剪贴板错误类型与写入结果类型
- `protocol`：消息模型（`ClipboardItem` 等）
- 前端模块：
  - `src/main.ts`：启动流程、定时刷新、Tauri IPC 调用和跨模块状态协调
  - `src/settingsForm.ts`：设置表单读写、语言/网络下拉渲染、共享码校验和静态文案刷新
  - `src/deviceList.ts`：共享域设备缓存、成员列表渲染、本机名称兜底和网络推荐依据
  - `src/transferProgress.ts`：传输卡片、速度统计、文本预览展开与滚动交互
  - `src/types.ts`：前端共享 DTO 类型
  - `src/html.ts`：前端 HTML 转义工具
  - `src/styles.css`：样式入口，只负责按顺序引入子样式模块
  - `src/styles/`：前端样式子模块，按基础变量、布局、设备列表、控件、传输进度、日志提示和响应式规则拆分
- `net`：网络层主协调入口（共享码心跳、设备发现、剪贴板变化监听、任务队列、连接、收发、日志、去重/回环抑制）
  - `net::crypto`：控制帧与文件 raw payload 的加密/解密、密钥派生
  - `net::dedupe`：内容指纹去重、已应用哈希 TTL、本机观察基线、在途指纹与近期事件缓存
  - `net::discovery`：mDNS 扫描、本机网卡选择、UDP 广播目标、设备可达性过滤与本机名称识别
  - `net::display`：传输进度里的人类可读类型、标题、摘要与文本预览
  - `net::domain`：共享域成员协调、发送目标收集、成员缓存清理、过期队列修剪和本机 IP 记忆
  - `net::file_stream`：文件 raw payload 的发送缓冲、接收 reader、进度节流与单帧写入耗时统计
  - `net::flow`：发送/接收队列消费、入队、防回环、失败降级和优先级流转
  - `net::inbound`：入站 TCP 连接处理、控制帧解码、文件流接收、接收侧传输进度和入站剪贴板事件入队
  - `net::item`：从平台剪贴板 payload 构造 `ClipboardItem`、生成设备 ID、计算待发送大小
  - `net::lifecycle`：同步主循环、TCP/UDP 监听、周期性发现和 worker 生命周期衔接
  - `net::logs`：运行日志、最近错误状态与调试日志文件追加
  - `net::marker`：单槽位最新值标记、事件时间戳比较、文件流标记和过期事件判断
  - `net::members`：共享域成员缓存、成员 TTL、发现结果合并/替换与已知成员记录
  - `net::metrics`：时间戳、耗时、百分比与吞吐格式化
  - `net::presence`：mDNS presence 注册、注册失败退避重试和 presence 运行配置
  - `net::queue`：发送/接收队列条目、优先级分层、就绪任务选取与短退避重试策略
  - `net::sender`：广播发送、普通 payload 发送、文件流发送、发送侧传输进度和吞吐 profiling
  - `net::socket`：TCP 超时、缓冲区调优、自身地址过滤和发送传输 ID 生成
  - `net::state`：运行状态 DTO、发现设备 DTO、网卡选项 DTO 与运行态共享状态容器
  - `net::transfers`：传输进度状态、完成/失败标记、活跃传输判断与历史保留
  - `net::udp`：UDP 共享域心跳发送、接收与心跳公告转设备缓存
  - `net::watch`：剪贴板观察 worker、本地复制识别、启动基线和本地观察去重缓存修剪
  - `net::wire`：`WireBody` 控制帧、长度前缀、raw payload frame 编解码
  - `net::workers`：入站写回 worker、出站调度 worker、主循环/队列空闲退避和 worker join

当前 `net/mod.rs` 已收敛为网络门面、公开命令入口和少量 `SyncEngine / PresenceService` 协调代码；状态容器、共享域纯逻辑、队列流转、presence 注册、剪贴板观察和 worker 调度已经下沉到独立子模块。后续结构治理的优先级不再是继续压 `net/mod.rs` 或 `lifecycle`，而是继续拆 `inbound`、`sender`、`discovery` 这些仍承担较多协议/连接/发现细节的模块。
当前 `clipboard/mod.rs` 已收敛为格式选择和重试协调入口；文件、图片、富文本、平台辅助、错误类型和内容指纹已经拆到独立模块，后续剪贴板格式扩展应优先进入对应子模块，避免重新把平台脚本和格式细节堆回入口文件。

## 当前并发结构（2026-05）

- `sync main loop`：负责 TCP 监听、UDP 发现、成员缓存维护和生命周期管理
- `clipboard watch worker`：负责监听本机剪贴板变化并产生命令
- `incoming connection worker`：每个 TCP 入站连接独立处理解帧与文件流聚合，不再阻塞主循环
- `inbound apply worker`：专门消费接收队列并写回系统剪贴板
- `outbound dispatch worker`：专门消费发送队列并执行网络发送

当前版本已经把“控制面”和“数据面”初步拆开；后续若继续追求大文件吞吐，会在 `outbound dispatch worker` 基础上继续演进 peer 并发与文件分片可让出调度。
当前文件发送已经改为更直接的分片数据面：bulk worker 以约 `16MB` raw payload frame 推进文件流，控制帧仍走 `WireBody`，文件体不再封装成 `FileStreamChunk` 或 `WireMessage` 大对象。
当前 bulk 数据面已经取消发送端完整归档准备层：文件/目录会边生成 `tar` bundle 边写入网络分帧，避免先落完整临时文件再从磁盘二次读取。
当前接收端文件包不再先落完整 archive；入站 worker 会把 raw payload 直接作为 `tar` reader 边接收边解包到内部目录，再交给 `inbound apply worker` 写回系统剪贴板。
当前 TCP 连接初始化统一走传输调优入口：发送端和接收端都会设置 `nodelay`、动态超时和约 `16MB` 的收发缓冲，优先把局域网场景下的吞吐瓶颈留给磁盘与系统剪贴板，而不是默认 socket 缓冲。
当前入站文件流在连接生命周期内由 `incoming connection worker` 独占聚合；如果对端中途断链，worker 会立即丢弃未完成流并标记失败，不把半包内容继续交给接收写回层。
当前结构已经比早期版本更接近可优化的数据面，但还不能等同于“稳定拉满带宽”的最终架构；如果吞吐成为核心目标，后续仍需继续优化更底层的零拷贝、多连接分片、系统剪贴板交互和连续大内容发送路径。

## 当前网络闭环（M0 已落地）

- 传输：`TCP`（长度前缀二进制帧；控制帧使用 `bincode`，文件体使用专用 raw payload frame，不再走 JSON/base64）
- 拓扑：每端既是服务端（监听）也是客户端（主动向共享域内其他成员推送）
- 触发：不再依赖主循环轮询读取剪贴板；Windows 走系统剪贴板变更消息，macOS 走变化监听器的轮询回调，统一转成“剪贴板任务”
- 发现：`mDNS`（`_lan-clipboard._tcp.local.`）+ UDP 广播心跳；相同 `shared_code` 的设备会写入共享域成员缓存
- 成员状态：UI 不再只依赖一次短暂 mDNS 扫描；运行时会持续接收 UDP 心跳、mDNS 结果和真实 TCP 收发信号，并按 TTL 清理离线成员
- 队列：本机变化进入发送队列，远端消息进入接收队列；接收仍优先于发送，发送队列由 `net::queue` 统一选取任务，规则为“新任务优先于旧重试、文本/富文本优先于图片、图片优先于文件、同层内较新的事件优先”
- 重试：发送队列在“当轮未发现可达成员”或“仅部分成员收到”时会短退避重试；接收队列在系统剪贴板短暂被占用时也会自动重试写入
- 延迟：剪贴板监听使用内部固定 `50ms` 检测间隔，主同步循环空闲间隔 `10ms`；图片读取优先复用 Windows 原生 `PNG` 格式，PNG 重编码使用快速压缩；图片等大 payload 的 TCP 写超时按内容大小动态放宽
- 大图策略：截图或大图片超出大小限制时，会在发送前自动等比缩小到限制范围内，而不是直接丢弃
- 文件策略：复制文件或目录时，会打包成 `tar` bundle 传输；远端在临时目录解包后，把这些路径重新写回系统剪贴板
- 图片策略：Windows 源端按 `PNG` 注册格式、`CF_DIBV5/CF_DIB`、`CF_BITMAP` 顺序读取；Windows 接收端写入原生 bitmap，减少图片到达后仍不可粘贴的等待
- 富文本策略：平台层会直接读取系统 `HTML / RTF` 剪贴板类型，并在纯文本之前参与同步；Windows 的 `HTML` 已切到原生格式读取，`RTF` 暂保留兼容读取路径
- 安全：默认由 `shared_code` 派生会话密钥；控制帧以 `AES-GCM-SIV` 加密，文件体帧以 `ChaCha20-Poly1305` 加密
- 配置：
  - `shared_code` 为共享域标识；同一局域网内填写相同共享码的设备自动互通
  - `poll_interval_ms` 仅保留为内部兼容字段，当前版本不再对用户暴露
- 去重策略：
  - `ClipboardItem.id` 使用事件级 `UUID`
  - `net::marker` 以 `created_at_us + source_device_id + id` 判断共享域单槽位最新值，旧事件会被视为过期
  - `net::dedupe` 以内容指纹阻止“远端写入后又被本机观察器当作本机复制再次转发”，并缓存近期事件 ID 与已应用哈希
  - `content_hash` 用于本地去抖
  - 接收远端写入后短时静默，避免 A->B->A 回环
