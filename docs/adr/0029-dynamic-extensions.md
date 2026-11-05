# ADR-0029 动态扩展与扩展工厂：AI 可编写、安装、重载并观察的扩展闭环

状态：**提议（Proposed）**。本文记录 2026-11-03 的新愿景；标注「用户裁定」的句子来自
用户明确要求，其余为实现默认或待决问题，不视为已定稿行为。

来源：用户裁定（2026-11-03：动态逻辑应能由 AI 完成并热加载；执行结果 AI 要能看到，
且结果可动态决定是否唤醒 AI；策略插件、native 插件也应由 AI 开发；AI 要能获取插件信息；
可参考 DeepSeek Harness 的插件模型）。

## 背景

ADR-0026 已经落地了扩展宿主：RootManager 固定，Provider 通过 namespace 注册，Lua /
动态库 / 外置进程各有载体；策略扩展点已覆盖 wake、access、delivery、send、turn_error、
clock、receive，另有 route 与平台适配器。

但这套宿主目前解决的是「**人写插件、桥启动时装载**」，不是「**AI 写插件、运行中自举**」。
现有基线存在四个断点：

1. **AI 没有 authoring 入口**。工具面只有 `memory_*` 能写文件，且作用域被锁在记忆
   worktree；没有扩展目录读写、校验、安装、重载的工具，也没有配置写入工具（这是
   ADR-0009 的刻意约束）。
2. **热重载未接线**。`PluginManager::reload_managers` 只被测试调用；`LuaManager::reload`
   还会在 provider 集合变化时拒绝（要求重启）。脚本内容变更、文件增删都不能在运行中生效。
3. **任务无结果面**。`route` 的 `task` 目前只有 native `exec` handler；任务执行完直接
   `Send(...)`，没有任务 id、状态、结果、错误记录，AI 无法查询，也无法据此决定是否开轮。
4. **扩展信息不完整**。`describe_plugins` / `/api/plugins` 能看到 descriptor 级信息，
   但 Lua 只有 manager 级通用 metadata；单脚本的 summary、加载错误、mtime、引用关系、
   调用统计不可见。

因此，当前能力可以表达「Lua 匹配 → `exec` 跑外部程序 → 发回产物」（例如 b23 下载），
但无法表达「AI 自己写匹配逻辑和 worker、安装后在下一轮生效、失败可回滚、结果可观察」。

## 决定

### D1. 动态扩展是一等对象

「扩展模块」是可以由 AI 或人编写、安装、升级、回滚的最小单位；它至少包含：

- `id`：全局唯一，位于 `namespace.name` 形式；
- `extension`：扩展点（route / wake / access / delivery / send / turn_error / clock /
  receive / task / platform_adapter）；
- `meta`：标题、摘要、effects、版本、文档链接；
- `capabilities`：需要被 core 强制执行的权限范围；
- 入口函数或进程描述；
- 源码位置、内容哈希、mtime。

模块的安装状态、启用范围、健康状态、最近调用与失败计数都属于 core 可查询的元数据。

### D2. 载体按延迟与隔离需求分层

| 载体 | 适用 | 约束 |
|------|------|------|
| 声明式模块 | 规则匹配、动作选择（send / drop / task / wake） | 不执行任意代码；由 core 解释 |
| 进程内 Lua | route / 策略判定，必须快 | 50ms 上限；不暴露 Store、Bridge、配置写入、网络、进程 |
| 外置进程 | 下载、HTTP、转码、长驻 watcher、重依赖工具 | JSON-lines 协议；core 负责生命周期、重启、限时、隔离 |
| Native（Rust/C ABI） | 性能敏感或需要 core 内部能力的受信扩展 | 人工审批；版本化 ABI；崩溃会影响桥进程；可卸载路径见 ADR-0030 |

**默认规则**：AI 编写的逻辑优先落在「声明式」或「Lua + 外置进程」；native 只作为受信
载体，经扩展工厂与人工审批后安装。

### D3. 配置只引用稳定分派器，不随模块增删而变

新增一个稳定 provider（命名示例：`dyn.routes` / `dyn.wake`），负责扫描扩展目录、按顺序
调用模块。配置只在首次启用该分派器时写一次；模块的新增、删除、排序由目录与 manifest
决定，不需要改 `config.toml`。这样 AI 安装模块不需要配置写入权限。

需要按会话/连接隔离时，分派器接受 `scope` 参数：全局、按连接、按会话。默认全局。

### D4. 热重载在轮次边界完成，校验失败不替换线上版本

生命周期：

```text
write → 语法/清单/能力校验 → 旁路编译 → 暂存（staged）
      → 下一轮开始前整体替换 dynamic namespace
      → 旧轮次继续使用旧快照
      → 任务 worker 按已入队参数跑完，不切换实现
```

- 支持新增 / 删除模块：替换整个 dynamic namespace，而不是只替换同一集合内的实现。
- 校验失败只进入 health 与目录错误列表，不替换线上版本。
- 可选文件 watcher 只负责标脏；真正替换仍发生在轮次边界，避免一轮执行中途行为变化。
- 显式 `extensions_reload` 工具用于调试，语义等同「立即标脏 + 下一个安全点替换」。
- 回滚以内容哈希 / git 提交为单位，不依赖人工记忆。

### D5. 任务处理器可扩展，任务结果是一等数据

- 增加 `task` 扩展点：`exec`、`process` 以及第三方 task handler 都通过注册表解析，
  不再写死 `match handler { "exec" => ... }`。
- 任务记录包含：`task_id`、origin room、handler、参数摘要、状态、开始/结束时间、
  结果摘要、错误、产物。至少可被 `tasks`（op=list/get）查询。
- 任务完成可以产生一条合成事件（`kind = task`，带 tags / payload / summary），
  由现有 wake / clock 策略决定是否开轮；`wake_on = "never" | "error" | "always"`
  是常用条件的快捷方式，不是绕过策略的直通开关。
- 合成事件带来源标记；同一条 route 不得再次消费自己产生的 task 事件，避免
  「失败 → 唤醒 → 再次匹配 → 再次失败」的循环。

### D6. 能力范围由 core 强制执行，而不是由 manifest 自我声明

扩展模块声明的 `capabilities` 至少覆盖：

- 房间：读取、发送、列出、跨房间发送；
- 唤醒：请求开轮、请求延迟；
- 任务：允许的 handler、允许的外部程序、超时、并发；
- 文件：可读/可写路径前缀；
- 网络：域名或地址范围；
- 配置：不允许（默认永远不给）。

执行点在动作边界：发送、唤醒、任务启动、文件访问、网络访问。现有
`sessions.access` 与连接 `allow_destructive` 仍是独立闸门，扩展能力不能替代它们。

### D7. 信任层级与审批

| 层级 | 能力 | 安装方式 |
|------|------|----------|
| T0 声明式 | 只能返回规则与动作 | AI 可直接写、直接重载 |
| T1 沙箱代码 | Lua 纯函数；外置进程带能力白名单、清空环境、限制文件/网络 | AI 可写；首次启用或能力变更需审批 |
| T2 受信 native | 进程内、全部扩展点 | 人工审批；扩展工厂构建、测试、签名后安装 |

外置进程默认：清空继承环境（尤其不能带 agenix 密钥）、独立工作目录、资源上限、
超时与自动重启；高风险能力（任意 `exec`、写系统路径、访问任意网络）默认拒绝。

审批可以是控制台确认、一次性 token 或管理员会话的显式工具调用；审批记录进入审计日志。

### D8. 可观察性是准入条件

每个模块至少暴露：id、extension、summary、effects、capabilities、source（声明式 /
Lua / process / native）、内容哈希、mtime、加载错误、health、被哪些连接/会话引用、
最近调用时间、失败计数。

对外入口：

- AI 工具：`extensions_list`、`extensions_read`、`extensions_write`、
  `extensions_reload`、`tasks`（op=list/get）；
- `describe_plugins`：按会话 / global 展示模块级信息，而非只有 manager 一条；
- 控制台：`/api/extensions`（目录、健康、审批待办），与 `/api/plugins` 并列。

写入类工具默认只对管理员会话开放；路径锁在扩展目录内，有大小上限与编译校验。

### D9. 已确认的部署选择（2026-11-03 用户裁定）

1. **信任层级**：声明式模块（T0）可由 AI 自动生效；Lua / 外置进程（T1）首次启用或
   扩权需审批；native（T2）必须人工审批。
2. **作用域**：模块 manifest 声明支持的 scope（全局 / 连接 / 会话），部署时选择，
   不强制所有模块全局或全部按会话。
3. **源码与生效点**：扩展源码放独立 git 仓库；AI 在会话 worktree 编写，通过
   `promote` 合入 active checkout，校验后在轮次边界替换；不直接加载任意会话 worktree。
4. **任务结果与唤醒**：默认写合成事件交给现有 wake / clock 策略；同时允许高信任
   模块直接请求唤醒。两条路径都记录审计，合成事件必须跳过会再次触发同一 route 的路径。
5. **native 生命周期**：执行 [ADR-0030](0030-unload-safe-native-plugin.md) 的
   unload-safe ABI v2；v1 native 加载后常驻，升级靠重启桥；默认仍优先 process 载体。

## 扩展工厂

「扩展工厂」指把「规格 → 代码 → 校验 → 构建 → 安装 → 重载」串起来的机制。建议：

- **core 提供机制，不把工厂本身做成 core 内的插件**。工厂依赖扩展目录、loader
  能力、审批与审计；把它做成普通插件会形成循环信任，也很难在 loader 变化时保持稳定。
- 工厂是一套 **authoring tools + loader 适配器**，可由一个专用 agent 会话/preset 使用：
  - `extension_scaffold(kind, extension, spec)`：生成模块骨架与 manifest；
  - `extension_validate`：语法、manifest、能力、路径、测试；
  - `extension_build`：按载体调用工具链（Lua 无需构建；process 产出可执行入口；
    native 走 cargo / C ABI 校验）；
  - `extension_install` / `extension_reload` / `extension_rollback`。
- **loader 绑定的是工具链，不是工厂本身**：LuaLoader 提供校验与重载；ProcessLoader
  提供进程协议校验与 supervisor 配置；NativeLoader 提供 ABI 版本、符号表、构建与签名。
  工厂按 `extension` 与 `carrier` 选择对应工具链。
- 原生插件由 AI 编写时，默认先以 process 形态验证行为，再在人工审批下转 native；
  native 的卸载安全由 [ADR-0030](0030-unload-safe-native-plugin.md) 约束，v1 常驻、
  v2 仅对声明 `UNLOAD_SAFE` 的插件开放。

## 与记忆 / git 的关系

扩展源码建议存放在一个独立的 git 仓库（或记忆仓库的子目录）：

- AI 在会话 worktree 中编写与提交；
- 桥只从「active checkout」加载，避免不同会话的未合并修改直接影响线上行为；
- 通过 `promote`（合并到 active 分支 + 校验 + 重载）让修改生效；
- 历史、diff、blame、回滚直接复用 git，不需要另造审计存储。

这与 ADR-0017 的「记忆是 git 仓库、会话持有 worktree」是同一套机制，区别是扩展仓库
需要单点 active checkout，而不是每个会话各自生效。

## 示例：`/music search` / `/music download`

目标：`/music search 126 xxx` 返回搜索结果；`/music download 126 <id>` 下载并发回文件。

在当前架构下，即使没有扩展工厂，这个功能也需要两部分：

1. **route 模块**（Lua 或声明式）：解析命令、校验管理员/房间范围，返回
   `{ action = "task", handler = "exec", params = { command = ["python3", ".../music.py", "search", ...], send = "text" } }`。
2. **worker**：一个外部程序（Python / Node / Go 均可），负责调用音乐 API、维护
   「上次搜索结果」状态（文件或 SQLite）、下载文件并输出产物路径。

worker **不是 native 插件**：当前 native 的是 `exec` 这个 handler（core 里跑
`Command`），worker 代码是它启动的外部程序。AI 可以写这两部分，但当前缺少安装与重载
入口，因此「AI 自己完成并生效」不成立。

若要支持 worker 常驻、并发、复用状态，应增加 `process` task handler 或外置进程
worker 协议；这属于 D5 / D2 的落地范围，不是重写 core。

## 当前基线 vs 目标

| 能力 | 当前 | 目标 |
|------|------|------|
| route 匹配 + exec 任务 | 已实现 | 保留 |
| Lua 新增/删除 provider | 拒绝，要求重启 | 运行时替换 dynamic namespace |
| 脚本内容热重载 | 代码存在，生产未接线 | 轮次边界接线 + 显式 reload |
| AI 写扩展 | 无 | extensions_* 工具 + 路径/能力约束 |
| task 结果查询 | 无 | task 记录 + `tasks` op=list/get |
| task 结果唤醒 | 无 | 合成事件 + wake 策略 + wake_on |
| 模块级信息 | 仅 manager 级 | manifest + health + usage + mtime/hash |
| 外置进程做 route/task | 不支持 | process carrier 扩展 |
| native 插件由 AI 开发 | 无 | 工厂 + 审批 + 构建/回滚 |

## 落地阶段

1. **P1 动态扩展闭环**：扩展目录、manifest、稳定分派器、dynamic namespace 替换、
   `extensions_*` 工具、模块级目录信息。
2. **P2 任务结果闭环**：task 注册表、task 记录、`tasks` op=list/get、合成事件与
   `wake_on`。
3. **P3 外置进程扩展**：process carrier 支持 route / task / 常驻 worker，包含生命周期、
   重启、限时与能力白名单。
4. **P4 扩展工厂**：scaffold / validate / build / install / rollback，先 Lua 与 process，
   native 最后；审批与审计接入控制台。

## 待决问题

1. 热重载触发：只靠显式工具，还是加文件 watcher 标脏？
2. active checkout 由谁 promote：管理员、维护 agent，还是两者都可？
3. 外置进程协议：沿用平台适配器的 JSON-lines，还是单独定义 task/worker 协议？
4. 现有 v1 `.so` 的迁移策略：保持常驻直到下一次重启，还是提供显式“升级并重启”流程？

## 参考

- 扩展宿主：[ADR-0026](0026-plugin-host.md)
- 平台适配器注册：[ADR-0027](0027-platform-adapter-registration.md)
- unload-safe native 插件：[ADR-0030](0030-unload-safe-native-plugin.md)
- 会话与记忆：[ADR-0017](0017-multi-agent-model.md)
- 配置管理：[ADR-0009](0009-config-management.md)
- 插件开发文档：[08-plugin-development.md](../08-plugin-development.md)
- 执行清单：[动态扩展与反馈修复](../plan/dynamic-extensions-and-fixes.md)
