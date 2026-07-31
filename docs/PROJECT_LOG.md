# Codex-X 项目维护日志

本文件记录面向维护者的根因、设计决策、验证依据和遗留风险。它与
`CHANGELOG.md` 分工如下：

- `CHANGELOG.md` 面向用户，记录每个发布版本可感知的新增、调整和修复。
- 本文件面向开发维护，解释为什么修改、哪些约束不能破坏，以及如何验证。

日志不得包含 Token、认证文件内容、用户数据或仅适用于某台电脑的隐私路径。

## 2026-07-30：live 配置并发与失败回滚

### 已确认根因

Codex-X 的供应商、提示词、备份恢复和状态刷新曾各自读写 `config.toml`。即使每次
写出的内容都是合法 TOML，只要 Codex、CC Switch 或另一条 Codex-X 操作在“读取旧值”和
“整文件替换”之间更新配置，后写入者就会覆盖前一个新值。多文件操作在中途失败时还可能
留下 config、auth、AGENTS 或应用数据库互不对应的混合状态。

### 约束

- 所有 Codex live 配置写入共用跨进程文件锁；已经持锁的内部函数使用 `_locked` 入口，
  禁止重复获取非重入锁。
- 写入前保存原始字节快照，原子替换前再次核对当前内容；快照已过期时拒绝写入并要求刷新。
- 回滚只能覆盖本次操作实际写出的期望值。多文件回滚先统一预检，再逆序恢复，不能覆盖
  Codex 或 CC Switch 在失败后写入的新内容。
- 活动供应商编辑从身份识别到数据库更新、live 热更新和最终状态构造都处在同一配置锁
  边界内。
- 备份恢复只接受备份根目录下的单个普通目录名和普通文件；元数据 ID、`CODEX_HOME` 与
  `had_config` / `had_auth` / `had_agents` 声明必须和实际载荷完全一致。损坏或缺失元数据、
  跨目录恢复、路径穿越、额外/缺失载荷和符号链接均在 live 写入前拒绝。旧提示词结构先在
  内存迁移，再作为一次可回滚写入落盘。
- 状态刷新在同一锁内完成旧配置迁移、官方认证捕获和状态构造；认证快照失败必须返回错误，
  不得吞掉后显示成功状态。

## 2026-07-30：关闭主窗口后驻留系统托盘

### 问题

Codex-X 原先没有系统托盘和窗口关闭事件处理。用户点击主窗口关闭按钮后，
Tauri 事件循环随最后一个窗口关闭而结束，后台管理能力也随之退出。

首版修复只调用了 `window.hide()`。这可以保留进程和窗口状态，但 macOS 应用仍是
`ActivationPolicy::Regular`，因此 Dock 图标不会消失，尚未真正进入仅菜单栏驻留状态。

### 决策

- 仅拦截标签为 `main` 的主窗口关闭请求，将窗口隐藏而不是销毁或结束进程。
- macOS 关闭时同时隐藏 Dock 并切换为 `ActivationPolicy::Accessory`；恢复窗口时切回
  `Regular`。这两项都执行，避免不同 macOS 版本残留 Dock 图标。
- Windows 关闭时显式启用 `skip_taskbar`，恢复时关闭，避免隐藏窗口仍占用任务栏。
- macOS 左键点击顶部栏图标打开菜单；Windows 左键点击托盘图标直接恢复窗口。
- 选择“显示 Codex-X”会恢复、取消最小化并聚焦原主窗口。
- 托盘菜单始终提供“退出 Codex-X”，用于明确结束后台进程。
- macOS 再次点击 Dock 图标时也恢复已经隐藏的主窗口。
- 不拦截应用级退出或更新器的显式重启，保证退出菜单、`Cmd+Q` 和安装更新仍可结束进程。

### 参考实现

对照 CC Switch `56fb46c09310ff52dabefd2b32f0e799e8357d9e` 的
`src-tauri/src/lib.rs` 和 `src-tauri/src/tray.rs`：其 Windows 路径使用
`set_skip_taskbar`，macOS 路径同时使用 `set_dock_visibility` 和
`set_activation_policy`。Codex-X 复用相同的平台生命周期规则，但保留自己的简化托盘菜单。

### 验证

- `cargo fmt --all -- --check`、`cargo check --locked --lib` 和 `git diff --check` 通过。
- `cargo test --locked --features windows-runtime-check` 全量通过。
- `pnpm --dir apps/desktop typecheck` 和 `pnpm --dir apps/desktop build:renderer` 通过。
- macOS 本机包实测：窗口显示时 Launch Services 类型为 `Foreground`；点击关闭按钮后
  主窗口消失、PID 保持不变，类型切换为 `UIElement`。这证明应用已从 Dock 模式进入仅
  顶部栏驻留模式；再次激活后同一 PID 恢复为 `Foreground`，配置数据正常加载。
- `Cmd+H` 后可以恢复同一进程；`Cmd+Q` 和托盘“退出 Codex-X”仍用于真正结束进程。
- 本机 Windows 交叉编译在第三方依赖 `ring` 阶段因缺少 MSVC C 头文件停止，尚未进入
  项目代码；发布前仍须以 GitHub Windows Actions 的原生构建结果为准。

## 2026-07-29：供应商切换与配置完整性

### 已确认根因

旧实现会把检测到的整份历史 `config.toml` 保存到供应商记录，并在后续切换时覆盖
当前文件。该文件仍可能是合法 TOML，但新增加的项目、插件、MCP 和功能设置会丢失，
表现为 Codex 要求重新设置或配置被“破坏”。

### 约束

- 供应商记录只拥有供应商相关字段，不拥有整份用户配置。
- 激活供应商必须基于最新 live 文档合并目标 provider 表，不回放历史整文件快照。
- 官方认证快照与中转认证严格隔离，供应商切换不能覆盖可信官方认证。
- 自动捕获官方认证只接受明确的 ChatGPT 登录模式及 access/refresh/id token；代理路由下的
  API key 属于不可信来源，不能晋升为官方快照。
- 用户明确从已确认的官方 live 路由切换到第三方时，允许保存非空的官方 API Key 快照，
  以保证“官方 API Key → 第三方 → 官方”往返不会丢失认证；只读状态刷新仍不自动采信 API Key。
- 第三方密钥只写入活动 provider 表的 `experimental_bearer_token`，不写入 `auth.json`。
- 官方与第三方路由都使用稳定的 `custom` 会话分桶；官方 provider 不设置 `base_url`，继续
  使用真实 OpenAI 后端和独立保存的官方 `auth.json`。
- 编辑活动供应商必须原子更新原记录，不能通过“保存后再切换”制造副本。
- 切换只影响后续新建会话，不强制重启 Codex 客户端。

## 2026-07-29：会话供应商同步

### 决策

- 统一使用 Codex 可识别的 `custom` 共享分桶。
- 活动会话的唯一权威来源是当前存储根目录中最高版本的可读 `state_N.sqlite`；旧 SQLite、
  单独存在的 JSONL 和 JSONL 行数都不能当作会话数量。
- 扫描 JSONL 前先按活动 thread ID 和 SQLite 的 `rollout_path` 建立候选集。标准命名孤立
  文件和未被引用的非常规文件不读取、不计入活动 rollout，也不会因损坏而阻断同步。
- SQLite 一旦为线程提供 `rollout_path`，该路径就是唯一权威文件；不得再按 UUID 文件名
  回退并修改同 ID 的旧副本。权威路径缺失、不可读或越界时必须标记扫描失败并阻止同步。
- `rollout_path` 指向的文件必须包含与该 SQLite 记录相同的 `session_meta.id`；缺少元数据、
  ID 串线或多个线程引用同一个文件时必须阻止同步，不能只更新 SQLite 后报告成功。
- SQLite 明确引用的非常规 rollout 仅在 `sessions` / `archived_sessions` 内、且为非符号链接
  普通 JSONL 时处理；活动候选无法读取或解析时保持失败关闭，不能显示“已同步”。
- 只修改 rollout 元数据和活动 SQLite 线程索引中的 provider 字段。
- 不修改会话正文、工作目录、用户事件标记或全局界面状态。
- 备份和回滚只覆盖本次实际修改的会话存储。

### 来源说明

当前 `b-nnett/codex-plusplus` 源码没有 rollout JSONL/SQLite 分桶同步实现，不能把这部分
描述为对其逐行复刻；`custom` 共享分桶语义来自对 CC Switch 可验证实现和本地 Codex
存储行为的交叉检查。
