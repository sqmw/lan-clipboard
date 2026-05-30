# TODO

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

- [ ] 托盘 + 开机自启 + 后台常驻策略
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
