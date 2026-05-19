# Dev Guide

## 启动

- 安装依赖：`pnpm install`
- 启动开发版：`pnpm tauri dev`
- Windows 若 `pnpm tauri dev` 跑不起来，先确认已执行过 `pnpm install`
- 若代码是从 macOS 手动同步到 Windows，必须避免把 `._*` 资源副文件带过去；这类文件会污染 `src-tauri/capabilities/` 并导致 `tauri build` 在 Windows 上报 `stream did not contain valid UTF-8`
- Windows 开发态如果出现 `WebView2 error ... 无效的窗口句柄`，优先确认当前版本是否仍在原生建窗阶段直接隐藏主窗口；当前实现已改为“前端启动完成后再隐藏窗口”

## 打包发行

- 打包命令：`pnpm tauri build`
- 发行配置入口：`src-tauri/tauri.conf.json`
  - `productName`：应用名
  - `version`：发行版本号
  - `identifier`：应用标识（macOS bundle id / Windows 标识）
  - `bundle.icon`：打包图标
  - `build.beforeBuildCommand` / `build.frontendDist`：前端构建与产物目录

建议对齐：

- `src-tauri/tauri.conf.json` 的 `version`
- `src-tauri/Cargo.toml` 的 `version`
- `package.json` 的 `version`

## 开发前先看

- 当前能力边界：`docs/status.md`
- 当前架构：`docs/architecture.md`
- 当前协议与调度：`docs/protocol.md`

## 最小回归清单

### 启动与配置

- macOS / Windows 都能启动
- 同一端只保留一个实例
- 启动后默认后台运行，不主动显示主窗口
- 关闭主窗口后不会退出，只会隐藏到菜单栏 / 系统托盘
- 修改共享码、网络、大小限制后，点击“保存配置”能生效并持久化
- 多网卡时可手动选择正确局域网 IP
- 可在设置里切换 UI 语言（中文 / English），保存后作为默认语言
- 可在设置里切换“开机启动”，并在下次启动后保持一致

### 共享域与成员

- 相同 `shared_code` 的设备能进入同一共享域
- 成员列表默认显示本机，展开后能看到其他在线设备
- “刷新”会立即按当前下拉框选中的网络补充扫描；这里不要求先点“保存配置”
- 切换“使用网络”下拉框后，会自动触发一次轻量刷新，减少手动再点一次“刷新”
- 未保存的网络切换不会再被状态轮询自动改回；下拉框会保持当前草稿选择，直到你再次切换或保存配置
- 日常成员状态主要由后台缓存持续维护，缓存展示也会按当前选择网络过滤

### 内容同步

- 文本可双向同步
- 图片可双向同步
- 文件/目录可双向同步，并在对端保持为可粘贴文件列表
- `HTML / RTF` 优先于纯文本参与同步

### 可靠性

- 远端写回后不会触发 A→B→A 回环
- 部分送达、剪贴板被占用时会自动补重试
- 若共享域当前只有本机，本地复制事件会直接丢弃，不再挂成“等待成员发现”的发送任务
- 文件发送过程中若出现新的文本/图片任务，高优先级任务不会被长期饿死
- 文件接收过程中若发送方中途下线，未完成文件流会被直接丢弃并标记失败

## 当前实现要点

- 剪贴板变化采用事件驱动监听，不再依赖旧版轮询读取逻辑
- 发送队列调度规则：`新任务 > 旧重试`，`文本/富文本 > 图片 > 文件`
- 网络层已拆成主调度循环、入站连接 worker、接收写回 worker、发送 worker
- 文件广播发送前只打一次包，再复用给多个 peer
- 发送/接收进度会展示类型、大小、方向和失败信息
- 高级加密开关已改为 switch 形式，但行为仍然只是控制 `encryption_enabled`
- 当前传输链路还没有把局域网带宽稳定吃满；如果回归目标包含高吞吐大文件传输，需要把“速度未拉满带宽”当成已知限制，而不是回归失败

## 调试入口

### 运行日志

- 应用内可查看最近日志
- 运行日志会落盘到系统临时目录下的 `lan-clipboard/runtime.log`

### 重点日志关键字

- `outbound item ... pending peers`
  - 当前复制事件正在等待成员发现或补重试

- `drop outbound item ... because shared domain only contains self`
  - 当前共享域只有本机；本次本地复制事件已被直接丢弃，不会再反复重试

- `apply retry queued`
  - 接收端系统剪贴板暂时忙，已进入自动重试

- `write peer failed ... timeout_ms=...`
  - 发送端 TCP 写入未完成；优先检查网络、防火墙、内容大小

- `discard incomplete inbound file ...`
  - 接收文件时发送方中途断链；当前实现会直接丢弃半包

- `detected local clipboard kind=...`
  - 本机已检测到一次剪贴板事件；可用来判断源应用到底提供了什么格式

- `received item ...` 但没有 `applied item ...`
  - 网络已经到达，问题更可能在接收端写剪贴板

## 常见问题

### 显示“已停止”

- 优先看是否端口被旧实例占用
- 开发态先结束旧的 `pnpm tauri dev` 或旧 App 进程

### 复制无反应

- 先确认双方共享码一致
- 再确认双方都已启动应用
- 再确认防火墙已放行 TCP `32910` 与 UDP `32911`

### 成员扫描不到

- 优先确认局域网、组播 / 广播和防火墙
- 多网卡环境确认“使用网络”选择的是实际局域网 IP
- 当前刷新过滤按 IPv4 `/24` 视角执行；如果两台机器不在同一前三段子网，界面会主动视为不同网络
- 当前版本已移除手动兜底地址；如果自动发现失败，需要先修复网络发现链路本身

### 图片不同步

- 先看日志里是否出现 `detected local clipboard kind=image_png`
- 如果没有，说明源应用可能没有提供当前已覆盖的系统图片格式

### 文件只同步成名字或路径文本

- 说明这次复制没有被系统识别成文件列表
- 优先确认是从 Finder / Explorer 文件面板直接复制，而不是复制路径文本

### 富文本退化成纯文本

- 先确认源应用确实写入了 `HTML` 或 `RTF`
- 某些应用只暴露私有格式时，当前版本会回退成纯文本

## 建议联调顺序

1. 先测文本
2. 再测图片
3. 再测小文件
4. 最后测大文件与断链场景

## 资源

- App 图标源文件：`docs/image/lan-clipboard-logo.svg`
