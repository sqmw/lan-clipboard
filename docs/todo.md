# TODO

## 当前主线：M6 流式图片与统一大小限制

目标：消除用户不可见的 `8MiB` 图片整体限制，让“最大同步内容大小”成为唯一用户可配置的总量上限；PNG 仍保留 UI 可见的 `80MiB` 解码安全边界。图片和文件复用认证 raw 分片、完整性校验、取消与失败收敛路径。该变更会升级线协议，所有共享域设备必须同步升级。

活跃任务：

- [done] P0：实现 v5 图片流式传输：图片发送 `start -> raw chunks -> end`，接收端按 declared size 收集、校验 SHA-256 后再写入系统剪贴板；保留连接关闭/超时的失败收敛。
- [done] P0：将设置统一为“最大同步内容大小”，移除用户不可见的图片 `8MiB` 总量限制；PNG/文件不再受控制帧总量限制，PNG 的 `80MiB` 解码安全边界已在 UI 与文档明确展示。
- [done] P0：补充 codec、流式图片和大小边界测试；macOS Rust `99 passed` 与前端生产构建通过。Windows 同版本 smoke 仍待执行。
- [pending] P1：实机 macOS ↔ Windows 用大于 `8MiB`、小于配置上限的 PNG 做双向复制与粘贴 smoke；两端需同时更新到 v5。
- [pending] P1：建立 macOS ↔ Windows 的裸 TCP、加密 raw 流、文件流和 PNG 流分阶段吞吐基准；比较 `256KiB` / `1MiB` / `4MiB` 分片后，再决定是否调整分片、调度让出或多流并行。当前唯一历史样本为 v4 文件接收 `3.97MiB/s`，不能视为链路上限。

回退边界：v5 与 v4 不互通。若升级后的验证失败，必须让整个共享域回到同一 v4 构建与同一配对密钥，不能混用版本。

## 历史主线：M5 后续防御纵深与发布验证

M5 的 P0/P1 安全收敛已于 `2026-07-15` 完成，最终独立 review 未发现未修复的安全 P0/P1；同日发现的同步可靠性 P1 已在源码和双平台 Rust 验证中修复。候选验证仍受实际 macOS `1.0.3/v3` 应用未升级、尚未重新配对及真实双端粘贴 smoke 阻塞。

活跃任务：

- [ ] P1（待用户授权）：将实际运行的 macOS `1.0.3/v3` 应用升级为 `2.0.0/v4`，在不混用协议的前提下重新配对为同一 26 位密钥共享域，并完成双向真实粘贴 smoke。替换正在运行的应用会中断同步，不能擅自执行。
- [ ] P2：从群组 PSK 迁移到逐设备身份、成员撤销与前向保密；这会改变成员模型和配对流程，落地前需先做架构决策。
- [ ] P2：决定无 peer 首发是否需要独立的 `30s` 发现宽限期。当前 `24` 次线性退避约 `8.1s` 后终止，已覆盖正常 `3s/500ms` 发现节奏；延长会改变失败反馈与离线重试行为，需产品确认。
- [ ] P2：把失败 peer 重试从瞬时 `host:port` 演进为稳定设备身份再解析，避免同一设备 IP 变化后旧 copy 只能等待下一次复制；这会调整队列条目模型，需先做设计决策。
- [ ] P2：为“先无 peer、后发现 peer、最终送达”和“无 peer 重试耗尽后释放 inflight”补充可注入 sender 或 loopback 端到端测试；当前单测已覆盖队列保留与成员缓存，但不直接驱动真实发送成功路径。
- [ ] P2：为 inbound/outbound queue 增加独立容量常量；当前依赖 latest-marker、连接数和 worker 数形成实际边界。
- [ ] P2：区分活动传输注册表与 24 条 UI 历史，避免极端并发时旧活动卡片被截断。
- [ ] P2：定义 NFC/NFD 与完整平台 case-fold 的文件名碰撞策略；迁移前需说明重命名/拒绝语义。
- [ ] P2：profile `16MiB` TCP buffer 与 16 条入站连接的内存峰值，再决定是否降低缓冲，避免无数据支持的吞吐回归。
- [ ] P2：在用户停止当前 Windows Tauri/Node/pnpm 进程后，把损坏的可再生 `node_modules` 无损移出同步目录并重跑 `pnpm install --frozen-lockfile && pnpm build`；本轮未擅自结束进程或清理状态。
- [ ] P2：真实 macOS ↔ Windows 重新配对，并 smoke Finder/Explorer、Office/浏览器、autostart、mDNS 降级和多网卡源地址。
- [ ] P3：生成、签名、安装并验证双平台 bundle；当前 CI 只做源码编译/测试与依赖审计。
- [ ] P3：经用户授权后单独清理 `docs/syncthing.md` 中仍残留的历史连接脚本/示例；该文件与 `.stignore` 是独立用户改动，不纳入本轮提交。

回退边界：`v4` 与 `v3` 不互通。若候选验证失败，只能让同一共享域所有设备整体回到旧应用和 legacy 配置备份，不能单机降级后继续混用。

## Done Log

### M6 流式图片与统一大小限制（2026-07-16）

- 结果：PNG 不再序列化为控制帧；发送端使用 `ImageStreamRawStart -> raw chunks -> PayloadStreamEnd`，接收端验证声明大小、连续 chunk、流 SHA-256、结束帧和 `content_hash` 后才入队写回。文件和图片复用同一认证 raw 通道、取消点、进度和断链失败收敛。
- 配置与 UI：`max_item_bytes` 的用户名称改为“最大同步内容大小（MB）”。文件只受此总量限制；PNG 另有 UI 可见的 `80MiB` 解码安全边界。文本/HTML/RTF 仍有内部 `8MiB` 控制帧边界，避免大对象单帧分配。
- 兼容性：wire、握手 magic/version、session key 与 discovery domain context 全部升级为 `v5`；v4 设备不会发现或认证为 v5 peer，必须整体更新。
- 验证：`cargo fmt --check`、Rust 全目标测试 `99 passed`、`pnpm build` 通过；新增“图片必须走 raw 流”、`27MiB+1B` 图片 start codec 和图片解码安全边界覆盖。
- 遗留：待在 macOS 和 Windows 都更新至同一 v5 构建后，执行双向大于 `8MiB` 且小于配置上限的 PNG 复制、粘贴和中途取消 smoke。

### 发现连续性与首发发送回归修复（2026-07-15）

- 结果：后台和手动 mDNS 都改为非权威观测合并；空/局部扫描不会抹除 TTL `30s` 内的 UDP/mDNS 成员，只有显式共享域/网络变更会权威清空。成员快照不再清空 outbound 队列、取消发送或中止健康接收 TCP 流。
- 发送：首次没有 peer 时保留 inflight 并按现有有界退避重试，后续重新收集成员；不会把空快照固化为失败目标。当前 `24` 次线性退避约 `8.1s` 耗尽，`30s` 仅是年龄上限，已在专题文档中明确。
- 验证：macOS `make verify` 通过（前端构建、严格 Clippy、Rust `96 passed`、依赖审计）；Windows 实际同步工作区源码 SHA-256 一致，Rust fmt/严格 Clippy 与 `90 passed` 通过。
- Review：最终独立 review 无新增 P0/P1；保留的 P2 是端到端 sender 注入测试、无 peer 更长宽限期决策和 IP 变化后的失败 peer 重解析。
- 文档：`protocol`、`status`、`architecture`、`dev` 和本 TODO 已同步。文档结构复审：Markdown `12` 个 / 代码 `63` 个（`19.0%`，触发文件数阈值），文档 `1241` 行 / 代码 `16073` 行（`7.7%`）；本轮只更新既有专题与唯一主 TODO，未增加重复任务系统，保留当前分层。
- 回退/恢复：显式共享域或网络配置变更仍会清空成员缓存；停止同步仍会清理运行队列。实际 macOS 旧版升级与重新配对未执行，保留为上方待授权 P1。

### M5 安全边界收敛（2026-07-15）

- 结果：线上 DTO 与本机路径分离；固定长度 PSK 握手和每连接 session；控制/raw AEAD 绑定；高熵配对密钥与可见迁移；no-follow 文件访问、受限 tar/staging；连接、帧、文件、发现、日志、成员与去重状态均有硬边界；设置/autostart/discovery 事务和跨平台剪贴板路径收敛。
- macOS 验证：`pnpm build`、`pnpm audit`、Rust `check/test/clippy --locked --all-targets --all-features`、`pnpm exec tauri info`、`git diff --check`；Rust `91 passed`。
- Windows 验证：在实际 Windows Syncthing 工作区核对关键 SHA-256 与 macOS 一致；Rust fmt/check/test/严格 Clippy 通过，Windows 条件集 `85 passed`。前端冻结安装被正在使用的旧 `node_modules` 缺文件阻断，未为验证而结束用户进程或清理目录。
- Review：最终独立认证、路径、资源、设置事务、平台与构建 review 未发现剩余 P0/P1；后续项全部回写本 TODO。
- 文档：协议、安全、架构、状态、开发与本 TODO 已同步。结构复审统计为 Markdown `12` 个 / 源码 `61` 个（`19.7%`，触发文件数阈值），文档 `1226` 行 / 源码 `15877` 行（`7.7%`）；双语入口、专题职责与唯一主 TODO 没有形成重复任务系统，因此保留当前分层，不做机械删减。
- 相关提交：`2fe1c65`（Make 入口）、`7a38060`（v4 核心与边界）、`dd6b7a6`（前端设置事务）、`17ae69e`（双平台验证基线）；协议、安全与验证细节由本 Done Log 及专题文档索引。
- 遗留风险：群组成员冒充/无前向保密、队列防御性容量、活动 UI 历史、Unicode 名称等价、socket 内存峰值与真实应用互操作。

## 里程碑 M0（可运行的最小闭环）

- [x] 共享码房间模型：同一 `shared_code` 的设备可自动互通
- [x] 设备发现（mDNS + UDP 心跳）：能看到同一共享码下的“在线设备列表”
- [x] 共享码加密：默认使用 `shared_code` 派生密钥
- [x] 协议闭环：两端通过长度前缀二进制帧互发 `ClipboardItem`
- [x] 大小限制可配置：UI 可设置并落盘（默认值 + 示例）
- [x] 基础日志：UI 可查看核心日志并支持清空

## 里程碑 M1（跨平台“常用格式”完善）

- [x] `text/plain`
- [x] `image/png`（Windows 源端已覆盖 `PNG` / `CF_DIBV5` / `CF_DIB` / `CF_BITMAP`，Windows 接收端直接写原生 bitmap）
- [x] 文件列表 / 文件目录打包传输（M0.5）
- [x] `text/html`
- [x] `text/rtf`

## 里程碑 M2（体验与可靠性）

- [x] 托盘 + 开机自启 + 后台常驻策略
- [x] 冲突/回环去重（防止 A->B->A 无限回写，事件驱动队列版）
- [x] 共享域成员缓存：持续心跳识别成员，避免单次扫描空结果导致界面误显示 1
- [x] 网络断连重试与队列退避：部分送达、剪贴板短时忙碌时自动补重试；共享域只有本机时不保留空转发送任务
- [x] 低延迟传输内核：`50ms` 监听、`10ms` 同步循环、raw bytes 二进制帧，去掉 JSON/base64 内容传输膨胀
- [x] 单实例保护：避免开发版/打包版或多个窗口同时抢占 `32910` 导致当前窗口实际停止同步
- [ ] 失败原因更细粒度展示（区分“未发现成员”“网络失败”“远端剪贴板忙”）
- [ ] 历史记录（可选，需明确隐私与落盘策略）

## 里程碑 M3（大文件吞吐）

- [x] 发送端边生成 `tar` bundle 边写入网络分帧，避免完整 outbound archive 落盘后二次读取
- [x] 接收端边接收 raw payload 边解包到内部目录，避免完整 inbound archive 落盘和二次读取
- [x] 分阶段 profiling 日志：覆盖剪贴板读取、文件指纹、发送、接收、系统剪贴板写回
- [x] 发送端归档读取缓冲：大文件发送时使用 `1MB` 源文件读取缓冲，减少默认小块读取造成的吞吐损耗
- [x] 发送端细分 profiling：记录文件流帧数、累计 socket 写入耗时和单帧最大写入耗时，避免只看到整体 `stream_ms`
- [ ] 发送端更细分 profiling：继续拆出文件读取与 `tar` 编码时间
- [ ] 多连接 / 分片并行传输：用于继续逼近局域网带宽上限，需先明确协议版本、失败恢复和单槽位最新值语义
- [ ] 纯网络 benchmark：绕过剪贴板和 tar，用于测当前 TCP + 加密数据面的理论上限

## 里程碑 M4（结构治理）

- [x] 第一轮保行为拆分：从 `net/mod.rs` 拆出 `crypto`、`display`、`metrics`、`wire`、`file_stream`、`discovery`、`members`、`queue`、`transfers`、`logs`、`marker`、`dedupe`、`item`、`socket`、`udp`、`sender`、`inbound`，从 `clipboard/mod.rs` 拆出 `fingerprint`
- [x] 第二轮剪贴板拆分：从 `clipboard/mod.rs` 拆出 `files`、`image_payload`、`rich_text`、`platform`、`types`，入口文件只保留格式选择、大小限制分发和剪贴板 IO 重试
- [x] 第三轮运行层拆分：从 `net/mod.rs` 拆出 `lifecycle`，集中管理同步主循环、剪贴板监听、presence、入站/出站 worker 和周期性发现心跳
- [x] 第一轮前端拆分：从 `src/main.ts` 拆出 `transferProgress`，集中管理传输卡片、速度统计、文本预览展开和滚动交互
- [x] 第二轮前端拆分：从 `src/main.ts` 拆出 `settingsForm`、`deviceList`、`types`、`html`，入口文件只保留启动编排、IPC 调用和定时刷新
- [x] 第四轮网络拆分：从 `net/mod.rs` 拆出 `state`、`domain`、`flow`，入口文件不再承载运行态容器、共享域目标收集和队列流转细节
- [x] 第三轮前端拆分：从 `src/styles.css` 拆出 `src/styles/` 样式子模块，入口样式文件只保留导入顺序
- [x] 第五轮网络拆分：从 `net/lifecycle.rs` 拆出 `presence`、`watch`、`workers`，生命周期文件只保留监听主循环和周期性发现衔接
- [ ] 继续拆网络运行细节：优先拆 `inbound`、`sender`、`discovery` 中仍偏重的协议、连接和发现逻辑
- [ ] 继续拆 `clipboard` 平台细节：如 Windows 图片 DIB 转换、macOS Swift 脚本、Windows 富文本 PowerShell fallback 继续向平台子模块下沉
- [ ] 继续拆前端入口：把 `src/main.ts` 的日志和 API 调用继续拆到独立模块

## 风险清单（需要早决策）

- “任何类型”跨 OS 的真实含义：macOS UTI 与 Windows clipboard formats 不可一一映射；建议定义“跨平台支持的格式集合”，其余类型降级或跳过（详见 `docs/protocol.md`）。
- 图片/文件体积较大：需要默认阈值、流式传输与 UI 侧明确提示。
- 图片链路已切成二进制帧；若后续继续追求超大图/大文件秒传，应继续切到分片或流式传输。
- Windows `RTF` 当前仍保留兼容读取路径；若后续发现慢或不稳定，应继续切到原生注册格式读取。
