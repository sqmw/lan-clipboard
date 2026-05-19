# 文档索引

<p>
  <b>🇨🇳 中文</b> | <a href="README.md"><b>🇬🇧 English</b></a>
</p>

本目录只做两件事：
- 给人看时，快速找到“当前支持什么 / 怎么开发 / 怎么排障”
- 给 AI / Agent 看时，快速定位“协议 / 架构 / 状态 / TODO / 环境事实”

## 推荐阅读顺序

### 想快速了解项目现状

1. `docs/status.md`：当前能力、边界、已知限制
2. `docs/architecture.md`：当前运行结构与模块边界
3. `docs/protocol.md`：线协议、调度、传输与限制

### 想开发 / 联调 / 回归

1. `docs/dev.md`：启动、验证、常见日志与排障
2. `docs/todo.md`：里程碑、风险与待办
3. `docs/status.md`：确认当前承诺边界，避免测错方向

### 想看安全与环境

- `docs/security.md`：共享码与加密策略
- `docs/syncthing.md`：当前这套 macOS ↔ Windows 环境的 Syncthing 映射事实

## 文档职责分工

- `docs/status.md`
  - 回答“当前支持了什么、没支持什么、哪些问题已知”
  - 面向产品确认、测试确认、联调前对齐

- `docs/architecture.md`
  - 回答“模块怎么分、线程怎么分、控制面和数据面怎么走”
  - 不堆排障细节，不代替 `dev.md`

- `docs/protocol.md`
  - 回答“线协议怎么组织、格式集合是什么、调度和传输规则是什么”
  - 不写界面说明，不写环境步骤

- `docs/dev.md`
  - 回答“怎么跑、怎么验、日志怎么看、异常怎么定位”
  - 只保留开发和排障需要的操作性内容

- `docs/todo.md`
  - 项目进度主入口
  - 只记录里程碑、未完成项、风险项

## 当前建议

- 对外先看 `docs/status.md`
- 改网络/同步逻辑先看 `docs/architecture.md` + `docs/protocol.md`
- 做联调先看 `docs/dev.md`
