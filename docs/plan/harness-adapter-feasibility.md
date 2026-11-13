# Harness 适配器可行性：Trae / WorkBuddy / LobsterAI

> 状态：定稿（2026-11-06，含三条补充调查）；同日追加实现状态：**WorkBuddy** 与
> **LobsterAI** 的适配器协议层已落地，**Trae 本轮不做**。**LobsterAI 的协议层已对真实
> gateway 验证（2026-11-13：握手 / `chat.send` / MCP `tools/list` / 错误轮次 `turn_end`），
> 但账号免费额度已用完且 app 未运行，模型真回话未验证；槽位当前仍由 WorkBuddy instance 1
> 顶替。** 协议层通过不等于端到端通过，未验证项见第 7 节。
> 证据等级：**已验证** = 本机命令输出、仓库/安装包文件内容或官方文档直接支持；**推断** = 由已验证事实推导、尚未端到端跑通；**未验证** = 只有间接线索或没有证据。
> 范围：只判断「能否为 ACB 写 harness 适配器」，不判断 ACB 的平台侧（QQ/KOOK 等）适配器。

## 0. 结论摘要

| 产品 | 结论分级 | 一句话 | 主要卡点 | 粗估工作量 |
|---|---|---|---|---|
| **Trae** | **需要额外桥梁（高风险）；本轮不做** | VSIX/扩展宿主已验证可用；工具面和 prompt 注入有公开接法，但稳定 API 没有轮次完成信号 | 扩展 API 缺 `ChatSession`/轮次事件；私有 socket/DB 需逆向 | 工具面约 0.3× dsh；完整适配约 2–4× dsh，且不确定性高 |
| **WorkBuddy** | **可直接做；协议层已实现；支持多实例** | CLI 四件事齐全：`-p` + `stream-json` + `--mcp-config` + `--append-system-prompt` + 会话续接；实例用独立 HOME/state/端口/会话前缀隔离 | 厂商 CLI 版本漂移、登录/权限、GUI 与 CLI 会话分离；缺 full bundle 是上游 WorkBuddy 发行物，不是 Nix 裁剪 | 约 1× dsh；约 1.5–2× opencode |
| **LobsterAI** | **可直接做；协议层已实现并经真 gateway 验证；额度/模型回话待恢复** | 内置 OpenClaw 运行时，gateway/CLI/MCP/ACP/agents 全部开放，且有 `openclaw.user.json` 覆盖层 | 应用状态目录/网关 token 的发现、应用生命周期、锁定 OpenClaw 版本；额度必须走 LobsterAI gateway/token proxy，裸 OpenClaw 不走 | 约 1.2–1.5× dsh；约 2× opencode |

**一句话选型**：要最快闭环先做 WorkBuddy；要用 LobsterAI 免费额度/长期复用先做 LobsterAI（必须接 app 的 gateway/token proxy，不是裸 OpenClaw）；Trae 本轮不做。

---

## 1. 基准：一个 harness 要被 ACB 接进来，最低需要哪些面

ACB 现有两套接入的权威说明在 [`docs/06-harness-adapters.md`](../06-harness-adapters.md)：适配器要干两件互相独立的事——**工具面**和**轮次驱动**；只做工具面的叫**半接**（能发不能收）。

把这两件事拆成四个最低面：

| 最低面 | dsh 怎么满足 | opencode 怎么满足 | 对第三方的硬要求 |
|---|---|---|---|
| **工具调用通道** | `dsh-mcp-client` 以 `streamable-http` 连 driver 的 MCP server，工具名 `mcp__acb__*` | 插件用 `ctx.tool.transform` 注册工具，模型经 `execute` 调 `tools.X` | 能让模型看见并调用 ACB 工具；调用能变成带当前 ticket 的 `tool_call` |
| **系统提示 / 人格注入点** | 每轮 prompt 文本最前面注入人格卡（签名去重） | 同上，`renderPersona` 放在本轮 prompt 最前面 | 能把人格、规则、插件上下文放进模型上下文；不要求真正的 system prompt API |
| **会话 / 消息注入点** | 进程外调 dsh webui `session/prompt`，`session/follow` 等 `turn/end` | 进程内 `ctx.session.prompt` 排话，`ctx.session.wait` 等这一轮说完 | 能向一个已有会话投一条消息，并且能判定这一轮何时结束 |
| **进程模型** | 常驻 driver + streamable-http MCP（生产）；stdio 只用于兼容/测试 | 常驻 opencode serve + 进程内插件 | 常驻或每次拉起都可以，但必须能复用会话、能串行 |

两条硬约束，任何第三方适配器都绕不开：

1. **ticket 必须和工具调用同源**。dsh 的注释写得很直白：`tool_call` 必须带当前轮次的 ticket，而 ticket 从 `turn_start` 拿；MCP server 和轮次驱动分成两个进程时，ticket 没法共享，所以 dsh 把三件事合成一个进程（[`driver.mjs`](../../harnesses/dsh/src/driver.mjs#L1-L31)）。第三方 harness 如果 MCP server 和轮次驱动是分开的，必须补一个本地 IPC/控制通道，否则工具调用会 `unauthorized`。
2. **必须有可判定的 `turn_end`**。把 prompt 投出去不等于这一轮跑完；opencode 用 `session.wait`，dsh 用 `session/follow` 等 `turn/end`。没有完成信号，适配器只能靠超时猜，桥会把「没回复」当成功或把「还在跑」当失败。

> 结论：**四件事缺一不可**。只有工具面 = 半接；只有轮次驱动 = 模型看不见 ACB 工具；两者都缺 = 不可行。

### 1.1 dsh 与 opencode 的可复用资产

| 资产 | 位置 | 可复用性 |
|---|---|---|
| 桥协议客户端 | [`harnesses/opencode/plugin/acb-wire.mjs`](../../harnesses/opencode/plugin/acb-wire.mjs) | 可直接复用：握手、ticket、`tool_call`、`turn_end`、重连 |
| 工具定义与 surface 选择 | [`harnesses/shared/tools.mjs`](../../harnesses/shared/tools.mjs) | 可直接复用：工具名、描述、JSON Schema、常驻/按需分层 |
| 最小 MCP server（stdio） | [`harnesses/dsh/src/mcp-server.mjs`](../../harnesses/dsh/src/mcp-server.mjs) | 可直接复用 |
| streamable-http MCP server | [`harnesses/dsh/src/mcp-http-server.mjs`](../../harnesses/dsh/src/mcp-http-server.mjs) | 可直接复用；新适配器优先用它解决 ticket 共享 |
| 轮次渲染 / 人格渲染 | `acb-wire.mjs` 的 `renderTurn` / `renderPersona` | 可直接复用 |
| 每会话一个 preset、每 preset 挂自己的 MCP | [`harnesses/dsh/dsh-mcp.patch.yml`](../../harnesses/dsh/dsh-mcp.patch.yml) | 可借鉴「一个 ACB 会话只看见自己那套工具」的隔离思路 |

---

## 2. 三列对照表（产品 × 最低要求 × 现状）

| 产品 | 最低要求 | 现状 |
|---|---|---|
| **Trae** | 工具调用通道 | **有（已验证）**：MCP client；用户级 `mcp.json` / 项目 `.trae/mcp.json`；stdio 与 HTTP 都支持；CLI 有 `--add-mcp` |
| **Trae** | 系统提示注入点 | **有（已验证）**：User Rules / Project Rules、`.trae/rules/*.md`、`AGENTS.md`、`CLAUDE.md`、`SKILL.md`；无 per-session system prompt API |
| **Trae** | 会话/消息注入点 | **部分（已验证）**：`trae-cn chat -m agent "<prompt>"` 可向当前工作目录的 GUI 会话注入 prompt；**没有 `--wait` / 输出格式 / 完成信号** |
| **Trae** | 进程模型 | **常驻 GUI（已验证）**：无 headless agent；`chat` 子命令是 GUI 入口，不是无头运行器 |
| **WorkBuddy** | 工具调用通道 | **有（已验证）**：CLI `--mcp-config`（stdio / SSE / HTTP）；`codebuddy mcp add -t stdio / sse / http`；GUI 也有 MCP 配置 |
| **WorkBuddy** | 系统提示注入点 | **有（已验证）**：`--system-prompt`、`--system-prompt-file`、`--append-system-prompt` |
| **WorkBuddy** | 会话/消息注入点 | **有（已验证）**：`-p/--print` + `--output-format stream-json` + `--session-id` / `-r/--resume` / `-c/--continue` |
| **WorkBuddy** | 进程模型 | **每次拉起或常驻（已验证）**：默认 `-p` 一次；也可 `--serve`（HTTP/Web UI/REST）或 `--acp`（stdio/streamable-http） |
| **LobsterAI** | 工具调用通道 | **有（已验证）**：OpenClaw MCP client（`mcp.servers`）；LobsterAI GUI 的 MCP connectors；本地 DB `mcp_servers` |
| **LobsterAI** | 系统提示注入点 | **有（已验证）**：LobsterAI `agents.system_prompt`；OpenClaw agent 配置；`SKILL.md`；`SYSTEM_PROMPT.md` |
| **LobsterAI** | 会话/消息注入点 | **有（已验证）**：gateway `chat.send` / `sessions.create` / `sessions.messages.subscribe`；CLI `openclaw agent --session-key/--session-id --message --json` |
| **LobsterAI** | 进程模型 | **常驻 gateway + 每轮 CLI（已验证）**：gateway `ws://127.0.0.1:18789`；`openclaw agent` 走 gateway 或 `--local` 嵌入式 |

> 这张表只回答「有没有面」。能不能端到端跑通，见每节的「差距」和「未验证」清单。

---

## 3. Trae

### 3.1 产品与版本

- 产品：**Trae CN**，字节跳动的 AI-native IDE（VS Code fork）；本机还有 Trae International（`~/.trae`），本次只核对 CN。
- 安装包：`/nix/store/6rb52m8wmbc3r03mx1wqbxzy4hyj5amg-trae-cn-2.3.99936`；`/run/current-system/sw/bin/trae-cn` 指向它。
- 正在运行的实例：进程命令行仍是 `trae-cn-2.3.99396`（旧于安装包；版本漂移的第一个证据）。
- CLI 自报：`trae-cn --version` → `1.107.1`（VS Code base）；`trae-cn --help` → `Trae CN 3.4.14`。
- 用户数据：`~/.trae-cn/`（扩展存储、插件、MCP 缓存）；`~/.config/Trae CN/User/`（settings、globalStorage、日志）；`~/.local/state/multi-instance/trae-cn/1/data/`（当前实例数据）。
- 公开文档：[MCP](https://docs.trae.ai/ide/add-mcp-servers)、[Rules](https://docs.trae.ai/ide/rules)、[Skills](https://docs.trae.ai/ide/skills)、[扩展管理/VSIX](https://docs.trae.ai/ide/manage-extensions)、[SOLO Agent](https://docs.trae.ai/ide/solo-coder)。

### 3.2 扩展面清单

| 扩展面 | 有 / 无 / 未知 | 来源与证据 |
|---|---|---|
| MCP client | **有（已验证）** | 官方文档明确支持 stdio 与 HTTP MCP；项目级配置在 `.trae/mcp.json`，用户级在 `~/.config/Trae CN/User/mcp.json`（应用代码 `joinPath(userSettingsResource, "..", "mcp.json")`）；`trae-cn --help` 有 `--add-mcp <json>` |
| MCP server | **无公开用途（已验证）** | 没有找到 Trae 对外暴露 MCP server 的入口；ACB 要做的是 MCP server，不需要 Trae 当 server |
| 插件 API | **部分（已验证 + 未验证）** | `~/.trae-cn/installed-plugins.json` 的 `manifest_json` 有 `skills` / `connector` / `env` / `mcpServers` / `binaries` / `instructions` / `environments`；官方 lark 插件是 skills + connector，没有遇到使用 `mcpServers` 的官方插件。`libai_agent.so` 字符串里有 `.trae/installed-plugins.json`、`.trae/plugin-config.json`、`.trae/plugins`、`.trae-plugin/plugin.json`、`.codex-plugin/plugin.json`、`.claude/plugins/cache`、`.codex/plugins/cache`；**本地自定义插件能否被加载、`mcpServers` 是否生效，未验证** |
| CLI / headless | **部分（已验证）** | `trae-cn chat [prompt]` 存在，但选项是 `--mode`、`--add-file`、`--maximize`、`--reuse-window`、`--new-window`、`--profile`；没有 `--wait`、没有 `--output-format`、没有 session id。它是 GUI 会话入口，不是无头 agent |
| 可注入配置 | **有（已验证）** | `.trae/mcp.json`（项目级，需在设置里启用「项目级 MCP」）；用户 `mcp.json`；`--add-mcp`；`.trae/rules/*.md`、`project_rules.md`、`user_rules.md`、`AGENTS.md`、`CLAUDE.md`；`SKILL.md` |
| 本地 HTTP / 服务 | **无公开 API（已验证）** | `resources/app/modules/ai-agent/meta.json` 写 `connections: [{socket:{port:40005}}]`；`agent-tool-host` 启动日志写「Listening on Unix domain socket (IPC)」「external TCP/gRPC/HTTP listeners are disabled」。没有文档化的本地 HTTP agent API |
| 系统提示 / 人格 | **有（已验证）** | Rules 与 Skills 是公开文档化能力；`libai_agent.so` 字符串 `.trae/rules/`、`user_rules.md`、`project_rules.md`；但没有 per-session system prompt 接口 |
| 会话 / 消息注入 | **部分（已验证）** | `trae-cn chat` 可注入一条 prompt；`workbench.action.chat.open` 命令在应用代码里存在（VS Code 内置命令，带 `mode`/`query`）；但没有公开的完成事件 |
| 进程模型 | **常驻 GUI（已验证）** | 桌面 Electron 常驻；`chat` CLI 打开/复用窗口；无 `--headless`/`serve` agent 入口；`serve-web`/`tunnel` 只是 VS Code 的浏览器/隧道子命令 |

### 3.3 与基准的差距

| 最低面 | 差距 | 能否外部补齐 |
|---|---|---|
| 工具调用通道 | 无缺口：MCP client 直接可用 | 直接把 ACB 做成 HTTP MCP server（优先 streamable-http；具体方言未验证），写进 `mcp.json` 或 `--add-mcp` |
| 系统提示 | 无缺口：rules/skills 可用；只是没有 per-session API | 人格可以像 dsh 一样放在本轮 prompt 最前面；规则可以写进 `.trae/rules` |
| 会话/消息注入 | **有注入、无完成判定**：`trae-cn chat` 不等待、不输出、不回 session id | 需要额外桥：VS Code 扩展监听 chat 状态，或轮询内部 DB/socket；两条都未验证 |
| 进程模型 | **无 headless agent**：只能驱动 GUI；需要登录、需要窗口 | 可以接受「常驻 GUI」，但适配器必须和 GUI 生命周期绑定；不适合无桌面服务器 |

**关键结论**：Trae 不是「产品封闭」——它有 MCP、rules、skills、VSIX。但**完整适配器**卡在「轮次结束」：公开 CLI 只负责把 prompt 送进 GUI，不告诉你什么时候跑完；内部 `ai-agent` socket（40005）和 `agent-tool-host` Unix IPC 都没有公开协议；`ModularData/ai-agent/database.db` 不能直接用 `sqlite3` 读（本机 719 MB，报 `file is not a database`），不能简单轮询。`vscode.chat` / `vscode.lm` API 在 `vscode.d.ts` 里存在；应用代码还引用了 `chatSessionsProvider` 这个 proposed API。但 Trae 是否注册了可用的 language model provider、是否真的开放这些 chat session 事件，都没有验证。

### 3.4 适配器形态建议

| 形态 | 要动哪里 | 能做到哪一步 |
|---|---|---|
| **MCP server（工具面）** | 复用 `mcp-http-server.mjs` + `acb-wire.mjs`；用 `trae-cn --add-mcp` 或写 `.trae/mcp.json` / 用户 `mcp.json` 注册。Trae 文档只写「HTTP 类型」，具体是 streamable-http 还是 SSE 方言未逐字验证 | 模型能调用 ACB 工具；**半接**（能发不能收，或只能靠人工触发轮次） |
| **VS Code 扩展（轮次驱动）** | 从 VSIX 安装（Trae 支持 `Install from VSIX`）；扩展执行 `workbench.action.chat.open`，并尝试 `vscode.chat` / `chatSessionsProvider` 观察状态 | 理论上可补「注入 + 完成判定」；**未验证**，且 `chatSessionsProvider` 是 proposed API，版本一变就可能失效 |
| **GUI 自动化 / 私有 socket** | 直连 40005 或解析内部 DB | 需要逆向；**不推荐**，版本漂移和审计风险都高 |
| **配置 patch** | 写 `.trae/rules/*.md`、`AGENTS.md`、`mcp.json` | 补人格/规则/工具注册；不能补轮次驱动 |

**Ticket 共享**：MCP server 和 VS Code 扩展是两个进程时，扩展必须通过本地 HTTP/IPC 把当前 ticket 传给 MCP server，或者让 MCP server 成为唯一的 ACB 连接、扩展只负责注入和收工。前者需要自己造协议；后者要求扩展能可靠判定轮次结束——这正是当前缺口。

### 3.5 结论分级

**需要额外桥梁（高风险）**。

- **VSIX 安装 + 扩展宿主**：已在临时 profile 验证可安装；当前登录实例的扩展宿主在运行。**这不是卡点。**
- **工具面 + 规则/人格**：现在就能做，属于「半接」。
- **完整轮次闭环**：稳定扩展 API 没有内置 Trae chat 的「开始/结束」事件，也没有读取最终回答的接口。要补只能走 proposed `chatSessionsProvider` 或私有 socket/DB；前者未验证且版本脆弱，后者要逆向。按**稳定 API 目前不可行**处理。
- 不可行的类型是「**需要逆向 / 只能人类操作**」，不是「产品封闭」。

### 3.6 主要风险

- **版本漂移**：安装包 2.3.99936、运行实例 2.3.99396、CLI 3.4.14 / VS Code 1.107.1；内部 API、`chat` 命令、插件 schema 都可能随版本变。
- **私有 API**：40005 socket、toolhost Unix IPC、内部 DB 都没有公开协议；一旦依赖就是逆向。
- **必须 GUI 登录**：`trae-cn chat` 需要窗口和登录态；不适合无桌面部署。
- **无完成信号**：没有 `--wait`/JSON 输出/session 事件；轮次失败和成功无法可靠区分。
- **MCP ticket 共享**：MCP server 独立进程时，工具调用可能全部 `unauthorized`（dsh 已经踩过这个坑）。

### 3.7 工作量粗估

| 范围 | 相对 dsh | 相对 opencode | 说明 |
|---|---|---|---|
| 只做 MCP 工具面 | ≈ 0.3× | ≈ 0.5× | 复用 dsh 的 MCP server + `mcp.json`/`--add-mcp` |
| 完整适配（VS Code 扩展 + 完成判定 + ticket 共享） | ≈ 2–4× | ≈ 3–5× | 不确定性主要来自完成判定和 proposed API |
| 依赖私有 socket/DB | 不可估 | 不可估 | 属于逆向，不建议 |

### 3.8 插件扩展 spike：VSIX / 扩展宿主 / API 能力（2026-11-06）

| 检查项 | 结果 | 证据 |
|---|---|---|
| **VSIX 安装** | **已定：可安装** | 临时 profile 执行 `trae-cn --user-data-dir=/tmp/trae-spike-data --extensions-dir=/tmp/trae-spike-ext --install-extension /tmp/acb-spike.vsix --force`，输出 `Extension 'acb-spike.vsix' was successfully installed.`；`--list-extensions` 返回 `acb.acb-spike`；扩展目录出现 `acb.acb-spike-0.0.1/` |
| **扩展宿主** | **已定：可用** | 当前登录实例有多个 `--vscode-crash-reporter-process-type=extensionHost` 进程；`resources/app/out/vs/workbench/api/node/extensionHostProcess.js` 与 `out/vscode-dts/vscode.d.ts` 存在；`~/.trae-cn/extensions` 存在（本次 `extensions.json` 为 `[]`，没有用户扩展） |
| **轮次开始/结束信号** | **已定：稳定 API 没有** | `vscode.d.ts` 只有 `chat.createChatParticipant` / `ChatParticipant` / `ChatRequest` / `ChatResponseStream` 和 `lm.*`；grep 没有 `ChatSession` / `chatSessions` / `onDidChangeChat` 观察接口。`chatSessionsProvider` 只在应用代码的 proposed API 注册表里出现，没有随包提供 proposed d.ts |
| **注入 prompt** | **部分：可以送，但没有完成句柄** | `workbench.action.chat.open` 命令存在（带 `mode`/`query`）；`trae-cn chat` 也能把 prompt 送进 GUI。但 CLI 没有 `--wait` / 输出格式 / session id，命令也不返回「这一轮完成」 |
| **拿最终回答文本** | **已定：稳定 API 不能** | 没有读取内置 Trae chat 回复的 API；`ChatResponseStream` 只属于扩展自己的 chat participant，不是观察内置 agent |
| **`vscode.lm` 模型** | **未验证（强推断无）** | `vscode.d.ts` 有 `lm.selectChatModels` / `registerTool`；但 `provideLanguageModelChatInformation` 只出现在 extensionHost 基座代码和 d.ts，workbench/main 没有 Trae 侧 provider 注册。临时 profile 的激活探测没跑起来（见下一行） |
| **临时 profile 激活** | **未验证** | 临时 profile 能安装扩展并扫描 `file:///tmp/trae-spike-ext`，但新 profile 看起来没有登录/没走完引导，扩展宿主没有启动，所以没有生成激活标记。当前登录实例有扩展宿主，但在那里安装需要 reload，本次未做 |

**结论（插件扩展路线）**：**卡在扩展 API 不全/缺事件，不是卡在 VSIX 安装或扩展宿主。**

- 能安装、能注入 prompt，但不能可靠拿到「轮次开始/结束」和最终回答文本，因此不能做完整轮次驱动。
- `workbench.action.chat.open` 的公开语义是打开 chat，不提供完成句柄；`trae-cn chat` 也没有 `--wait`/输出。
- 唯一可能的补法是 proposed `chatSessionsProvider` 或在扩展里直连私有 socket；前者没有 shipped d.ts、版本一变就失效，后者要逆向。

**最小 spike（如果还要试）**：

1. 扩展 `package.json`：`activationEvents: ["*"]`、`main: "./extension.js"`、一个 `acb.spike` 命令。
2. `activate` 里只做观测：`await vscode.lm.selectChatModels({})`、`vscode.commands.getCommands(true)` 过滤 `chat|agent|solo|mcp`、`Object.keys(vscode.chat || {})`、`vscode.extensions.all`，写到 `/tmp`。
3. 命令里执行 `vscode.commands.executeCommand('workbench.action.chat.open', { mode: 'agent', query })`，观察是否有任何可订阅的 session 事件；没有就是不可行。
4. 用 `trae-cn --install-extension` 安装，reload 后在已登录 profile 里跑；必须验证：`vscode.lm` 是否有模型、chat 是否有完成事件、`chatSessionsProvider` 是否可用、agent 回复是否能读到。

**退路与风险**：

- **私有 socket 40005**：`meta.json` 声明 `connections:[{socket:{port:40005}}]`，但本次 `ss` 没有看到监听；`agent-tool-host` 用 Unix IPC 且 external TCP 关闭。协议未知，属逆向；版本漂移和审计风险高。
- **内部 DB**：`ModularData/ai-agent/database.db` 的 `file` 类型是 `data`，不是可直接 `sqlite3` 读的库；没有公开 schema，不能作为稳定完成信号。

---

## 4. WorkBuddy

### 4.1 产品与版本

- 产品：**WorkBuddy**，腾讯的 AI agent 桌面应用（Nix 注释原文：Tencent's AI agent desktop app, unfree vendor binary, Electron）；同时存在 WorkBuddy International（`workbuddy-ai`，同版本 5.6.2.40046327，本次未单独验证）。
- 安装位置：`/run/current-system/sw/bin/workbuddy` → `/nix/store/2056lvy4j6n0hv6bwyl9h8fff1nxw5rm-workbuddy-5.6.2.40046327/bin/workbuddy`；应用在 `.../opt/WorkBuddy/`。
- 应用版本：**5.6.2.40046327**；官方 Changelog 有 5.6.2（<https://www.codebuddy.ai/docs/workbuddy/Changelog>）。
- 内置 CLI：`.../resources/app.asar.unpacked/cli/dist/codebuddy-headless.js`，`--version` 报 **2.147.0**；CLI 文档在 <https://www.workbuddy.ai/docs/cli/cli-reference>。
- 状态目录：GUI 默认用 `~/.workbuddy/`（`workbuddy.db`、`plugins/`、`settings.json`、`mcp-tool-list.json`、`logs/`）；CLI 默认用 `~/.codebuddy/`（`local_storage/`、`sessions/` 等）。**本项目不直接启动 app**：GUI 走 `multi-instance workbuddy 1`，实例 1 状态在 `~/.local/state/multi-instance/workbuddy/1`，真实 `~/.workbuddy` / `~/.codebuddy` 不会被写入；首次为空 profile，要在实例里重新登录。
- **waypipe 说明**：nix-config 里没有 WorkBuddy 专用的 waypipe 代码；`waypipe` 只出现在通用远程桌面模块（`modules/nixos/desktop/networking/remote-desktop.nix`）和 `hosts/wsl-work/packages.nix`。它是远程显示 Wayland 应用的工具，不是 WorkBuddy 的扩展点。WorkBuddy 的 Nix 包装是 GTK/Electron wrapper（`makeCWrapper` 设置 `GIO_EXTRA_MODULES`、`GDK_PIXBUF_MODULE_FILE` 等），没有额外注入点。

### 4.2 扩展面清单

| 扩展面 | 有 / 无 / 未知 | 来源与证据 |
|---|---|---|
| MCP client | **有（已验证）** | CLI `--mcp-config <fileOrString>`、`--strict-mcp-config`；`codebuddy mcp add -t stdio / sse / http`；官方 MCP 文档列出 STDIO/SSE/HTTP 三种 transport 与非交互模式的审批方式（`--settings '{"enableAllProjectMcpServers": true}'` / `enabledMcpjsonServers`） |
| MCP server | **未知 / 内部有 HTTP host（未验证）** | `~/.workbuddy/logs/mcp-apps-diag.log`：`host.create ... architecture=daemon hasHttpServer=true hasAccessTokenProvider=true runtime=McpManager`；没有公开的「WorkBuddy 当 MCP server」文档。本任务不需要它 |
| 插件 API | **有（已验证）** | GUI 插件：`~/.workbuddy/plugins/installed_plugins.json`、`.workbuddy-plugin/plugin.json`（含 `agents`、`workbuddy.dependencies` 的 `type: "mcp"`）；CLI：`--plugin-dir <dirs...>`、`--agents <json>`；官方 Sub-Agents 文档说明插件在 `agents/` 目录提供子代理 |
| CLI / headless | **有（已验证）** | `codebuddy-headless.js --help`：`-p/--print`、`--output-format text / json / stream-json`、`--input-format stream-json`、`--session-id`、`-r/--resume`、`-c/--continue`、`--max-turns`、`--bg` |
| 可注入配置 | **有（已验证）** | `--mcp-config`、`--settings`、`--system-prompt`、`--system-prompt-file`、`--append-system-prompt`、`--agents`、`--plugin-dir`、`--setting-sources` |
| 本地 HTTP / 服务 | **有（已验证）** | CLI `--serve`（HTTP server：Web UI、REST API、ACP over HTTP SSE）、`--port`、`--host`、`--auth`；`codebuddy daemon start --port 8080`；官方 CLI Reference 列出 daemon 命令 |
| 系统提示 / 人格 | **有（已验证）** | `--system-prompt` 替换；`--append-system-prompt` 追加；`--system-prompt-file` 从文件读 |
| 会话 / 消息注入 | **有（已验证）** | `-p` 传 prompt；`--session-id` 固定会话；`-r/--resume` 续接；`--output-format stream-json` 输出实时事件；`--input-format stream-json` 支持流式输入 |
| 进程模型 | **每次拉起或常驻（已验证）** | 默认 `-p` 一次进程；`--serve` / `--acp` 常驻；`--bg` 后台 |

### 4.3 与基准的差距

| 最低面 | 差距 | 说明 |
|---|---|---|
| 工具调用通道 | **无缺口** | `--mcp-config` + HTTP MCP；非交互模式用 `--settings` 预批准项目 MCP |
| 系统提示 | **无缺口** | 两个 system prompt flag 直接可用 |
| 会话/消息注入 | **无缺口** | `-p` + `stream-json` + `--session-id`/`--resume`；进程退出即这一轮结束 |
| 进程模型 | **无缺口** | 每次拉起最简单；需要常驻时用 `--serve`/`--acp` |

**唯一需要自己补的是适配器本体**：一个进程同时做 ACB 客户端 + streamable-http MCP server + CLI 进程管理。工具调用的 ticket 由适配器持有；CLI 通过 `--mcp-config` 连适配器的 MCP 端口，模型调工具时自然带当前 ticket。这个形状和 dsh 的 streamable-http 模式同构，不需要新的桥。

**打包注意（已验证）**：本包只带 `cli/dist/codebuddy-headless.js` 与 `cli/dist/codebuddy-lite-wb.mjs` 两个 bundle，没有 `cli/dist/codebuddy`；因此 `bin/codebuddy` 这个 launcher 在需要加载主 bundle 的命令（如 `mcp list`）上会报 `Cannot find module '../dist/codebuddy'`。`codebuddy-headless.js` 直接运行正常（`--version` 2.147.0、`mcp list` 成功）。适配器应直接调用 headless bundle。**这不是 Nix 打包缺陷，归属见 4.8。**

### 4.4 适配器形态建议

**推荐：进程外 wrapper + streamable-http MCP server。** 每次 ACB 开轮：

1. 复用 `acb-wire.mjs` 接桥，拿 ticket；
2. 复用 `mcp-http-server.mjs` 暴露当前 surface 的工具；
3. 拉起 `node .../cli/dist/codebuddy-headless.js -p "<renderTurn(delivery)>"`，参数：
   - `--output-format stream-json`
   - `--session-id acb-<session>`（或 `-r` 续接）
   - `--mcp-config '{"mcpServers":{"acb":{"type":"http","url":"http://127.0.0.1:<port>/mcp"}}}'` + `--strict-mcp-config`
   - `--append-system-prompt "<persona + 规则>"`（或把人格放在 prompt 前面）
   - `--settings '{"enableAllProjectMcpServers": true}'` 或显式 `enabledMcpjsonServers`
   - 按需要 `--permission-mode` / `--allowedTools`
4. 解析 `stream-json`，把 assistant 文本和工具调用记日志；进程退出（或收到最终 result）即 `turn_end`；
5. 失败/超时走 `link.endTurn(ticket, error, failure)`。

**不推荐**：直接操作 `~/.workbuddy/workbuddy.db` 或 GUI daemon。CLI 是公开文档化的；GUI 的 daemon/HTTP 服务没有外部控制文档。

### 4.5 结论分级

**可直接做**。

四件事都有公开、可验证的接法；本地 CLI 的 `--help`、`--version`、`mcp list` 已经跑通。端到端模型轮次没有在本机跑（需要登录/额度），所以「可直接做」的剩余风险是**认证与厂商 CLI 行为**，不是扩展面缺失。缺 full TUI bundle 是上游 WorkBuddy 发行物的取舍，不是 Nix 打包问题；适配器用 headless bundle 即可，不需要修包。

**实现状态（2026-11-06，已被 2026-11-13 的 ACP 方案取代）**：当时的 `harnesses/workbuddy/`
是进程外驱动 + MCP 工具面 + 多实例，22 个用例覆盖完整一轮、ticket、`-r` 复用、resume 重试、
result error、MCP 未连接、超时、reset 与两实例并行；另用真实 `codebuddy-headless.js`（未登录）
验证了 `--mcp-config` 连接、工具名与 auth 错误形状。**该实现已删除**：`codebuddy-headless.js`
的 `-p --output-format stream-json` 是厂商私有面，而 `codebuddy-acp` 提供了标准 ACP 面，
所以改成 `harnesses/acp/` 的通用 ACP 适配器（启动命令声明化，CodeBuddy 用
`codebuddy-acp --acp` 接）。下面 4.6/4.8 关于「`dist/codebuddy` 缺失」「stream-json 漂移」
的记录仍保留，作为当时为什么走 headless 的证据。

### 4.6 主要风险

- **厂商 CLI 版本漂移**：CLI 2.147.0 与桌面 app 5.6.2 是两套版本号；文档是 CodeBuddy CLI 文档，WorkBuddy 打包可能滞后或裁剪。
- **登录/权限**：headless 需要已有登录态；`-p` 非交互模式可能需要 `--permission-mode`、`--settings` 预批准；`--dangerously-skip-permissions` 不建议默认开。
- **GUI 与 CLI 会话分离**：GUI 用 `~/.workbuddy/workbuddy.db`，CLI 用 `~/.codebuddy/`；ACB 的会话不会自动出现在 WorkBuddy GUI 里。这是产品体验问题，不是协议阻塞。
- **包内没有全量 bundle**：这是上游 WorkBuddy 发行物的取舍（只带 headless / lite-wb），Nix 安装阶段没有删 CLI 文件；`bin/codebuddy` 的默认入口在部分命令上会失败，必须用 headless bundle。详见 4.8。
- **输出 schema 漂移**：`stream-json` 的事件形状没有像 MCP 那样的稳定标准，需要适配器做容错解析。

### 4.7 工作量粗估

| 范围 | 相对 dsh | 相对 opencode | 说明 |
|---|---|---|---|
| 完整适配（wrapper + MCP + CLI） | ≈ 1× | ≈ 1.5–2× | 比 dsh 少一个 webui HTTP/WS 客户端；多一个 CLI 进程和 stream-json 解析 |
| 只做工具面 | ≈ 0.3× | ≈ 0.5× | 复用 dsh MCP server；CLI 仍需要人手动跑 |

### 4.8 缺失 `dist/codebuddy` 的归属：上游 WorkBuddy 发行物，不是 Nix 裁剪（2026-11-06）

| 检查项 | 结果 | 证据 |
|---|---|---|
| **上游 CodeBuddy npm** | **有 full bundle** | `@tencent-ai/codebuddy-code@2.147.0` 的 npm tarball 含 `package/dist/codebuddy.js`、`codebuddy-headless.js`、`codebuddy-lite-wb.mjs`、`codebuddy-broker.js`；`cli/package.json` 的 `publishConfig.customPackage` 指向这个包 |
| **WorkBuddy 安装包 asar** | **只有 headless / lite-wb** | `/nix/store/2056.../opt/WorkBuddy/resources/app.asar` 的 `cli/dist/` 只有 `codebuddy-headless.js` 与 `codebuddy-lite-wb.mjs`，没有 `codebuddy.js`；`app.asar.unpacked/cli/dist/` 同样 |
| **Nix installPhase** | **没有删 CLI 文件** | `structuredAttrs.installPhase`：`cp -r opt/WorkBuddy $out/opt/WorkBuddy`，只删 koffi 非 linux_x64 目录、patch safe-bin shims、调用 `repack-asar.py`；没有 `rm cli/dist/*` |
| **asar repack** | **只改 `main/index.js`** | `repack-asar.py` 只替换 `main/index.js` 的 Wayland show() 补丁；`rebuild()` 保留全部 entry 集合和物理顺序，`verify_types()` 还校验 entry set 不变 |
| **`bin/codebuddy` 入口** | **需要 full bundle** | `bin/codebuddy` 的 `isHeadless` 只对 `-p` / `--acp` / `--a2a` / `--input-format` / `--output-format` / `--version` / `--help` / daemon/background 为真；`mcp list` 不在列表里，于是走 `require('../dist/codebuddy')`，本包没有这个文件 → `MODULE_NOT_FOUND` |
| **WorkBuddy 宿主预期** | **走 headless** | `app.asar` 里有 `CODEBUDDY_FORCE_HEADLESS_BUNDLE` 与 `CODEBUDDY_FORCE_LITE_WB_BUNDLE` 字符串；launcher 注释说明 WorkBuddy 的 sidecar/prewarm 会设置这些 env，直接命中 headless/lite-wb |

**结论**：**上游 WorkBuddy 发行物就没有 full TUI bundle；Nix 打包没有裁掉它。**

- 这是 WorkBuddy 桌面构建的取舍（只带 headless/lite-wb），不是 Nix 打包缺陷；不需要修包。
- 适配器应直接调用 `cli/dist/codebuddy-headless.js`，或给 `bin/codebuddy` 设 `CODEBUDDY_FORCE_HEADLESS_BUNDLE=1`。
- 如果一定要让 `bin/codebuddy` 的 full TUI 入口工作，才需要从 `@tencent-ai/codebuddy-code` 补 `dist/codebuddy.js`；这是可选的打包增强，不是 ACB 适配器的前置条件。

### 4.9 真实 headless CLI 未登录探测：形状与坑（2026-11-06）

用本机 WorkBuddy 5.6.2.40046327 的 `cli/dist/codebuddy-headless.js`（CLI 2.147.0）跑，
不登录、不消耗额度：

| 探测 | 实测结果 | 对适配器的影响 |
|---|---|---|
| `--mcp-config '{"mcpServers":{"acb":{"type":"http","url":"http://127.0.0.1:<port>/mcp"}}}'` + `--strict-mcp-config` | `system/init` 的 `mcp_servers` 是 `[{name:"acb",status:"connected"}]`，`tools` 里有 `mcp__acb__send_message` | MCP 配置形状和工具命名确认；ticket 仍由同进程 MCP server 持有 |
| 未登录 `-p` | `assistant` 文本 `Authentication required. Please use /login command…`；`result` 是 `subtype:"error_during_execution"`、`is_error:true`、`errors:["Authentication required…"]`；**进程 exit 0** | 不能只看 exit code；适配器先看 `result.is_error` / `errors`，再判 `model/auth_failed` |
| `-r` 指向已存在的会话 | init 直接用该 `session_id` 继续，后续照常走 auth 错误 | resume 成功路径可用；第二轮会复用同一 CLI 会话 |
| `-r` 指向不存在的会话 | 只发 `{"type":"error","error":"No conversation found with session ID: …"}`，没有 `result`，exit 0 | 适配器必须把 `type:"error"` 且无 result 的情况判失败，并识别为 resume 缺失后改用 `--session-id` |
| `--allowedTools` 后直接跟 prompt | prompt 被当成 allowedTools 的可变参数吞掉，CLI 没有真正拿到提问 | 适配器把 prompt 放在 `--session-id`/`-r` 之后，避免被可变参数吞 |
| `--tools ""` | init 的 `tools` 仍有 `Bash`/`Edit`/`Read`/`WebFetch` 等内置工具 | `--tools ""` 不能关内置工具；适配器默认不传，并在 system prompt 里禁止用 shell/HTTP 绕桥 |
| `--session-id acb-probe-1` | 接受非 UUID 的 id（字母/数字/连字符/下划线/冒号） | 适配器可以把 ACB 会话 id 归一化成 `acb-<id>` |

**结论**：WorkBuddy 的 CLI 接入口径已经打到「登录墙」；剩下的端到端只差登录态和额度，
不是协议面缺失。`--tools` 无法关闭内置工具这一点要写进风险，不能假设模型只用 MCP 工具。

**最小验证命令**（适配器自带）：

```bash
node --test harnesses/workbuddy/test/*.test.mjs
```

### 4.10 多实例与内置工具风险（2026-11-06）

**多实例映射**：

| 维度 | 配置 | 效果 |
|---|---|---|
| 实例标识 | `ACB_WB_INSTANCE=workbuddy-1` | 会话 id 前缀 `acb-workbuddy-1-…` |
| 桥 harness | `ACB_HARNESS=workbuddy-1` | 桥串行键；与实例名不一致时配置阶段拒绝 |
| state 根 | `ACB_WB_STATE_DIR=.../workbuddy-1` | 子进程 `HOME`/`USERPROFILE`、`CODEBUDDY_CONFIG_DIR`、`CODEBUDDY_DATA_ROOT` 全指向它 |
| MCP 端口 | `ACB_MCP_PORT=7718` | 回调 URL `http://127.0.0.1:7718/mcp`，实例间不共用 |
| 会话 id | `acb-<instance>-<acb-session>` | `-r` 只续本实例自己的会话 |

具体样例：`harnesses/workbuddy/instance-1.env.example`（7718）与
`instance-2.env.example`（7719）；桥侧样例在 `acb-config.example.toml` 里演示
「用 workbuddy-1 顶替 lobsterai 槽位」和切回方式（只改 `harnesses`/`harness`，不改代码）。

**GUI 侧启动方式**：不要直接启动 app，走 multi-instance wrapper：

```bash
multi-instance workbuddy 1   # instance 2 用 multi-instance workbuddy 2
```

- 入口 `/run/current-system/sw/bin/multi-instance`，签名
  `multi-instance <app> <instance> [args...]`，两个位置参数都必填；可用 app 含
  `codebuddy workbuddy workbuddy-ai lobsterai trae trae-cn zcode zcode-cn codearts`。
- bwrap 把实例状态目录 bind-mount 到 `$HOME` 下固定路径；instance 1 的 GUI 状态目录是
  `~/.local/state/multi-instance/workbuddy/1`。首次为空 profile，必须在实例里重新登录。
- driver 的 `ACB_WB_STATE_DIR` 默认 `~/.local/state/acb/workbuddy/<instance>`，与 GUI
  实例目录是两套。不推荐指到 GUI 目录共用登录态（同一个 `.codebuddy` 双写有锁/DB 竞争
  风险）；确需复用按受控步骤：先在实例里登录、退出 GUI，再让单个 driver 独占试一次。

**已验证（假 CLI）**：两个实例并行一轮，MCP URL 不同、state/HOME 不同、会话 id 前缀不同，
第二轮各自 `-r` 不串；见 `harnesses/workbuddy/test/multi-instance.test.mjs`。

**已验证（真实 CLI，未登录）**：`HOME=<实例 state>`、`CODEBUDDY_CONFIG_DIR=<实例>/.codebuddy`
时，CLI 的 `local_storage/`、`user-state.json` 写在实例目录内，不再落到真实 `~/.codebuddy`。
**未验证**：如果某个 CLI 版本把登录 token 放进 OS 钥匙串，`HOME` 隔离可能只隔离会话记录，
不隔离登录态；这要等真实登录后确认。

**内置工具关不掉的取舍**（`--tools ""` 实测无效，`--allowedTools` 只是预批准不是限制）：

| 方案 | 取舍 |
|---|---|
| 允许清单 | 最干净，但依赖 CLI 提供真正的工具白名单/禁用语义；当前 `--tools` 不满足 |
| 钩子（hooks） | 能在工具执行前 fail closed，真正阻止 Bash/WebFetch 绕桥；代价是维护 hook、跟版本绑定、自身故障必须拒绝 |
| 审计告警 | 从 stream-json 事件发现内置工具调用并告警；只能发现不能阻止，适合做兜底 |

建议短期「钩子拦截 + 审计兜底」，允许清单等上游稳定语义；本轮不实施。详见
`docs/06-harness-adapters.md` 的 WorkBuddy「内置工具关不掉」一节。

---

## 5. LobsterAI

### 5.1 产品确认与版本

先确认「LobsterAI 具体指哪个产品」：

- **LobsterAI（有道龙虾）** = 网易有道（NetEase Youdao）出品的开源桌面级 AI Agent，MIT 许可；官网 <https://lobsterai.youdao.com> 标注最新版本 **2026.10.30**；GitHub 仓库 <https://github.com/netease-youdao/LobsterAI>。不是同名的小工具，也不是 OpenClaw 本身。
- 本机 Nix 包：`lobsterai-wrapper-2026.10.30` → `lobsterai-unwrapped-2026.10.30`；`/run/current-system/sw/bin/lobsterai`。
- 应用 `package.json`：`version: 2026.10.30`；`openclaw.version: v2026.8.1`；`license: MIT`。
- 内置运行时：`resources/cfmind/package.json` 是 **OpenClaw 2026.8.1**（`name: "openclaw"`，repository `openclaw/openclaw`）；`node openclaw.mjs --version` → `OpenClaw 2026.8.1 (512bddc)`。
- Electron 版本文件：`resources/app/version` → `43.6.0`（这是 Electron，不是产品版本）。
- 状态目录：`~/.config/LobsterAI`（userData；本机目前为空，应用未运行）；`~/lobsterai/project`（默认工作目录）；恢复库 `~/lobsterai/recovery-20261022/lobsterai.sqlite`。
- **启动方式**：GUI 走 `multi-instance lobsterai 1`，**不要直接启动 app**。wrapper 用 bwrap 把实例状态挂到 `$HOME` 下固定路径，真实 `~/.config/LobsterAI` 不会被写入；首次为空 profile，必须在实例里登录 LobsterAI 并等它生成 `openclaw/state`（`openclaw.json`、`gateway-port.json`、`gateway-token`）。
- driver 的 `ACB_LA_STATE_DIR` 必须是 `~/.config/LobsterAI/openclaw/state`（或该 multi-instance 实例里等价的 `openclaw/state`），不能指 `~/.openclaw`；MCP 端口固定 `7717`。
- 架构（官方 README）：Electron renderer + Main process（Cowork 会话、权限、SQLite、skills、MCP、IM、scheduler）+ **内置 OpenClaw gateway**；OpenClaw 是 agent runtime，LobsterAI 生成它的配置并把事件转成 Cowork 消息流。

### 5.2 扩展面清单

| 扩展面 | 有 / 无 / 未知 | 来源与证据 |
|---|---|---|
| MCP client | **有（已验证）** | OpenClaw `mcp.servers`（`docs/tools/mcp.md`：Streamable HTTP / SSE / Stdio；`openclaw mcp add/doctor/probe`）；LobsterAI GUI「MCP connectors」；本地 DB `mcp_servers` 表（`transport_type`、`config_json`） |
| MCP server | **有（已验证）** | `openclaw mcp serve` 把 OpenClaw channel conversations 暴露成 MCP（`docs/cli/mcp.md`）；ACB 不需要它，但说明运行时双向开放 |
| 插件 API | **有（已验证）** | OpenClaw plugin SDK（`resources/cfmind/dist/plugin-sdk/`、`docs/plugins/`）；LobsterAI `user_plugins` 表；官方 README 的 Kits/Skills/Connectors |
| CLI / headless | **有（已验证）** | `openclaw.mjs` 可用；`openclaw agent --help` 列出 `--session-key`、`--session-id`、`--message`、`--json`、`--local`；`openclaw agent exec` 是 CI/headless 入口（`docs/cli/agent.md`）；`openclaw acp` 提供 ACP bridge（`docs/cli/acp.md`） |
| 可注入配置 | **有（已验证）** | OpenClaw `openclaw.json` + `openclaw.user.json`：应用源码映射写明「LobsterAI regenerates openclaw.json on every sync; `openclaw.user.json` next to it is merged on top so manual changes survive」；agents 有 `system_prompt`、`model`、`skill_ids`；MCP 有 `mcp.servers` |
| 本地 HTTP / 服务 | **有（已验证）** | Gateway WebSocket `ws://127.0.0.1:18789`（历史日志）；`docs/gateway/protocol.md` 定义 `connect`、`req/res/event` 帧、`chat.send`、`sessions.*`；`openclaw acp` 是 ACP over stdio，`openclaw mcp serve` 是 stdio MCP |
| 系统提示 / 人格 | **有（已验证）** | `agents` 表 `system_prompt`；`resources/app/resources/SYSTEM_PROMPT.md`；OpenClaw agent 配置；`SKILL.md` |
| 会话 / 消息注入 | **有（已验证）** | `chat.send`（`operator.write` scope）；`sessions.create`、`sessions.subscribe`、`sessions.messages.subscribe`；`openclaw agent --session-key ... --message ... --json` 一次一轮 |
| 进程模型 | **常驻 gateway + 每轮 CLI（已验证）** | Gateway 常驻 18789；`openclaw agent` 可走 Gateway 或 `--local` 嵌入式；LobsterAI 自己管理 Gateway 生命周期 |

### 5.3 与基准的差距

| 最低面 | 差距 | 说明 |
|---|---|---|
| 工具调用通道 | **无缺口** | `mcp.servers` + Streamable HTTP；ACB 作为 MCP server 注册进去即可 |
| 系统提示 | **无缺口** | `agents.system_prompt` / OpenClaw agent 配置 / `SYSTEM_PROMPT.md` / skills |
| 会话/消息注入 | **无缺口** | Gateway `chat.send` 或 `openclaw agent`；`--json` 有 `status`/`final`/`sessionId` |
| 进程模型 | **无缺口** | 常驻 gateway 或每轮 CLI；两种都能复用会话 |

**需要补的工程**：

1. **配置注入**：把 ACB MCP server 写进 OpenClaw 配置。优先走 `openclaw.user.json`（应用每次同步重新生成 `openclaw.json`，但用户覆盖文件会合并在上面）；也可以 `openclaw mcp add`，但要确认应用下一次 sync 不会覆盖。
2. **状态/网关发现**：LobsterAI 生成 `openclaw.json` 的位置、gateway token、agent id/默认 agent，都由应用管理。**已验（2026-11-13）**：实例状态在 `~/2/.config/LobsterAI/openclaw/state`（`~/2` 是独立 HOME），`gateway-port.json` = `18789`、`gateway-token` 可读，`config.mjs` 对这份真实文件解析出 `ws://127.0.0.1:18789` 和 token；`loopback-state.json` 里 token proxy = `41853`。仍然要靠 `ACB_LA_STATE_DIR` 指到对的实例。
3. **轮次驱动（额度相关）**：**优先 gateway `chat.send`**，因为这就是 LobsterAI 自己的 gateway，模型走 `lobsterai-server` provider → 本地 token proxy → LobsterAI server，消耗 LobsterAI 额度。`openclaw agent --session-key ... --json` 也能走额度，但它是 Gateway client，默认读 `~/.openclaw`；必须把 `OPENCLAW_STATE_DIR`/`OPENCLAW_CONFIG_PATH` 指到 `~/.config/LobsterAI/openclaw/state`（或显式配 gateway remote url/token），否则是裸 OpenClaw，不走额度。`openclaw agent --local` 只有在使用 LobsterAI 生成的 `openclaw.json` + `LOBSTER_PROXY_TOKEN` + 运行中的 token proxy 时才走额度。**端到端未验证**。
4. **避免双通道**：LobsterAI 自带 QQ/微信/飞书等 IM 通道（`@tencent-connect/openclaw-qqbot` 等插件）；ACB 接管 QQ 时，必须把对应 agent 的 QQ 通道关掉，或使用一个只被 ACB 驱动的专用 agent，避免同一条消息被两套桥重复回复。

### 5.4 适配器形态建议

**推荐：进程外 driver + streamable-http MCP server + OpenClaw CLI/gateway。**

```
ACB 桥 ──turn_start/tool_call──> LobsterAI driver（单进程）
                                   ├─ BridgeLink 接桥（复用 acb-wire.mjs）
                                   ├─ MCP streamable-http server（复用 mcp-http-server.mjs）
                                   └─ OpenClaw 客户端：
                                       - MVP：spawn `openclaw agent --session-key ... --message ... --json`
                                       - 进阶：gateway WS `chat.send` + `sessions.messages.subscribe`
```

具体落点：

- **工具面**：在 `openclaw.user.json` 的 `mcp.servers.acb` 写 `{ transport: "streamable-http", url: "http://127.0.0.1:<port>/mcp", enabled: true }`；或在 LobsterAI GUI 的 MCP connectors 里加同一个 server。
- **人格/规则**：在 OpenClaw agent 配置或 `openclaw.user.json` 里设置 `system_prompt`；每轮也可以像 dsh 一样把 `renderPersona` 放在 prompt 最前面。技能可以放 `SKILL.md`。
- **会话**：`--session-key agent:main:acb-<session>` 固定映射；或 gateway `sessions.create` + `chat.send`。
- **额度入口**：这个 driver 必须接 LobsterAI 自己启动的 gateway（`<userData>/openclaw/state/gateway-port.json` + `gateway-token`）和 token proxy；不要另起一个裸 OpenClaw。裸 OpenClaw 用自己的 provider 不会消耗 LobsterAI 的免费额度。
- **完成判定**：`openclaw agent` 的退出码 + `--json` 的 `status`（`ok`/`error`/`timeout`）就是这一轮的结束；gateway 模式则等 `chat` 事件/会话空闲。
- **ticket 共享**：driver 自己持有 ACB ticket，MCP server 在同一个进程里，CLI 通过 HTTP MCP 连它。和 dsh 的 streamable-http 模式一致。

### 5.5 结论分级

**可直接做**。

LobsterAI 不是「只有一个 GUI」的产品：它的底层是 OpenClaw，Gateway、CLI、MCP、ACP、agents、skills 都是公开文档化的；LobsterAI 额外提供了 `openclaw.user.json` 覆盖层。四件事都有可验证的接法。剩下的工作是**应用状态发现**和**端到端联调**，不是扩展面缺失。额度路径也已在机制层确认：必须接 LobsterAI 自己的 gateway/token proxy；裸 OpenClaw 不走额度。实际扣费未做端到端验证，见 5.8。

**实现状态（2026-11-06）**：`harnesses/lobsterai/` 已完成进程外驱动 + MCP 工具面 +
OpenClaw gateway WS 客户端。`node --test harnesses/lobsterai/test/*.test.mjs` 16 个用例
用假 gateway 覆盖完整一轮、ticket、chat.send 拒绝、run error、gateway 断开、超时
`chat.abort` 和 reset；配置测试覆盖 state 目录缺失、裸 `~/.openclaw`、quota 链路缺失、
缺 gateway-port/token。

**真 gateway 实测（2026-11-13）**：用 LobsterAI 自带的 OpenClaw 2026.8.1
（`resources/cfmind/openclaw.mjs`）按真实 state 起 gateway，适配器连上去跑了一轮：
握手成功（protocol 4）、`chat.send` 被接受、真 agent 主动连适配器 MCP server 并
`tools/list`、模型失败时也回 `turn_end`。**mock 掩盖了两个真实协议不符，已修**：
`connect.client.id/mode` 必须是注册表内的值（现为 `gateway-client` + `backend`；
原来 `acb-lobsterai` + `operator` 被真 gateway 拒绝），`sessions.messages.subscribe`
的参数名是 `key`（原来发 `sessionKey`）。假 gateway 现在按真 schema 校验这两处。
模型没回话的原因不是协议：app 未运行导致 token proxy 不在（`LLM request failed:
network connection error`），且日志显示账号免费额度已用完。

**当前处置（2026-11-13）**：**LobsterAI 仍由 WorkBuddy instance 1 顶替槽位，代码保留、不回退**。
启用前提：额度恢复 + app 在运行（token proxy 起来）。步骤：GUI 侧
`multi-instance lobsterai 1` 启动并在实例里登录，driver 侧
`ACB_LA_STATE_DIR=~/.config/LobsterAI/openclaw/state`、`ACB_MCP_PORT=7717`（或该实例里
等价的 `openclaw/state`），然后把桥配置的 `[[sessions]] harness` 切回 `lobsterai`；
不需要改代码。

### 5.6 主要风险

- **应用每次 sync 重新生成 `openclaw.json`**：必须走 `openclaw.user.json` 覆盖，或确认 `openclaw mcp add` 的写入位置不会被覆盖。
- **锁定 OpenClaw 2026.8.1**：LobsterAI 打包的是固定版本；OpenClaw 上游 API 仍在演进，升级 LobsterAI 可能带来协议差异。
- **状态/网关 token 由应用管理**：`~/.config/LobsterAI` 本机为空；gateway token/agent id/默认 agent 的获取路径没有官方文档，需要从应用配置或 `openclaw config` 读。
- **应用生命周期**：Gateway 由 LobsterAI 启停；ACB driver 要处理「应用没启动 / gateway 重启 / token 轮换」。
- **IM 双通道**：LobsterAI 自带 QQ/微信/飞书等通道；ACB 接管 QQ 时必须禁用对应通道或使用专用 agent。

### 5.7 工作量粗估

| 范围 | 相对 dsh | 相对 opencode | 说明 |
|---|---|---|---|
| 完整适配（driver + MCP + `openclaw agent`） | ≈ 1.2–1.5× | ≈ 2× | 比 dsh 多状态/配置发现；比 WorkBuddy 多应用生命周期处理 |
| 只做工具面 | ≈ 0.4× | ≈ 0.6× | 只需 MCP server + `openclaw.user.json` |

### 5.8 额度归属与入口：LobsterAI 的免费额度怎么走（2026-11-06）

| 问题 | 结论 | 证据 |
|---|---|---|
| **模型调用链路** | **LobsterAI 的 `lobsterai-server` provider → 本地 token proxy → `lobsterai-server.youdao.com`** | `shared/providers/constants.js`：`ProviderName.LobsteraiServer = 'lobsterai-server'`、`OpenClawProviderId.LobsteraiServer = 'lobsterai-server'`；`openclawConfigSync.js` 的 `PROVIDER_REGISTRY[LobsteraiServer]`：`normalizeBaseUrl` 返回 `http://127.0.0.1:<tokenProxyPort>/v1`，`resolveApiKey` 返回 `${LOBSTER_PROXY_TOKEN}` |
| **本地入口** | **OpenClaw Token Proxy** | `openclawTokenProxy.js`：`http.createServer` 监听 `127.0.0.1`；`tokenGetter` / `serverBaseUrlGetter` / `tokenRefresher` 注入 LobsterAI 账号 token 与 server base URL；日志 `[OpenClawTokenProxy] started on 127.0.0.1:45573` |
| **额度身份** | **LobsterAI 账号 / 官方 keyfrom** | `keyfrom.json`：`{"keyfrom":"official"}`；`endpoints`：portal login / pricing / `creditsDetail` / `turnUsage`；日志 `[Auth:getPricingCatalog] https://lobsterai-server.youdao.com/api/models/pricing-catalog`；`app_config.defaultModelProvider = "lobsterai-server"`；agent `main` 的 model 是 `lobsterai-server/deepseek-flash` |
| **状态/配置路径** | **`<userData>/openclaw/state/`** | `openclawEngineManager.js`：`stateDir = path.join(baseDir, 'state')`，`baseDir = path.join(userDataPath, 'openclaw')`；`configPath = stateDir/openclaw.json`；`gatewayTokenPath = stateDir/gateway-token`；`gatewayPortPath = stateDir/gateway-port.json`；gateway 进程 env `OPENCLAW_STATE_DIR=stateDir`、`LOBSTER_PROXY_TOKEN` |
| **`chat.send` 到 app gateway** | **能走额度（机制已验证）** | app gateway 用生成的 `openclaw.json`，provider 是 `lobsterai-server` → token proxy；`chat.send` 是 gateway 的普通写入路径 |
| **`openclaw agent --session-key`** | **能走额度，但前提是接到 LobsterAI 的 gateway/state** | `openclaw agent` 是 Gateway client；默认读 `~/.openclaw`（OpenClaw 默认），不会自动继承 LobsterAI 的 `<userData>/openclaw/state`（推断）。必须设 `OPENCLAW_STATE_DIR`/`OPENCLAW_CONFIG_PATH` 到该 state，或设置 gateway remote url/token；否则是裸 OpenClaw，不走额度 |
| **`openclaw agent --local`** | **不一定** | `--local` 是嵌入式 agent；只有使用 LobsterAI 生成的 `openclaw.json` + `LOBSTER_PROXY_TOKEN` + 运行中的 token proxy 时才走额度；裸配置不走 |
| **免费额度实际扣减** | **未验证；当前账号额度已用完** | 机制、endpoint、账号身份都已验证。2026-11-13 的实例日志（16:45）明确写「免费额度已用完，请升级套餐」，所以现在连一次成功模型调用都做不了；额度恢复后再对账 credits/usage |

**入口结论**：适配器要接的是 **LobsterAI 自己启动的 OpenClaw gateway + token proxy**，不是裸 OpenClaw：

- **最直接**：读 `<userData>/openclaw/state/gateway-port.json` 与 `gateway-token`，连 `ws://127.0.0.1:<port>`，用 `chat.send`；gateway 会用 app 生成的 `openclaw.json` 和 `lobsterai-server` provider。
- **次直接**：把 `OPENCLAW_STATE_DIR` 指到 `<userData>/openclaw/state`，跑 `openclaw agent --session-key ... --json`；但必须保证 gateway 在运行、`LOBSTER_PROXY_TOKEN` 在环境里，且 agent 的默认模型是 `lobsterai-server/*`。
- **裸 OpenClaw / 自带 provider 的 `openclaw agent`**：**不走 LobsterAI 额度**。

**结论分级**：**能走额度（机制已验证；实际扣费未验证）**。风险是 app 生命周期、token 读取、状态目录发现；不是扩展面缺失。当前额度已用完、app 未运行，**适配器仍由 WorkBuddy instance 1 顶替槽位**，LobsterAI 代码保留待启用。

---

## 6. 推荐实现顺序及理由

### 顺序

- **默认工程顺序（未知最少）**：1. WorkBuddy → 2. LobsterAI。Trae 本轮不做。
- **如果目标是 LobsterAI 免费额度**：1. LobsterAI（接 app gateway/token proxy）→ 2. WorkBuddy。Trae 本轮不做。
- **当前执行状态（2026-11-06）**：LobsterAI 待 app 登录后启用；先用 WorkBuddy instance 1 顶替它的槽位，登录后切回只改配置。

### 理由

**WorkBuddy：默认工程顺序第一（未知最少、最快闭环）。**

- 四件事全部有公开文档 + 本地可执行证据：`-p`、`stream-json`、`--mcp-config`、`--append-system-prompt`、`--session-id`/`--resume`。
- 适配器是「wrapper + MCP server」，与 dsh 的 streamable-http 模式同构；`acb-wire.mjs`、`mcp-http-server.mjs`、`shared/tools.mjs` 都能直接复用。
- 不需要依赖 GUI 的 daemon/token/数据库；CLI 是独立入口（注意用 headless bundle，不要用 `bin/codebuddy`）。
- 缺 full TUI bundle 是上游 WorkBuddy 发行物的取舍，不是 Nix 打包缺陷；适配器用 headless 即可，不需要修包（见 4.8）。
- 主要风险是厂商 CLI 版本漂移和登录/权限，属于可控的工程风险。

**LobsterAI：额度目标第一；默认工程顺序第二（开源、可复用，但状态发现多一些）。**

- 底层是 OpenClaw 2026.8.1，Gateway/CLI/MCP/ACP 都有文档；`chat.send` 是直接走额度的每轮入口；`openclaw agent --session-key ... --json` 也能用，但必须接 app state/gateway（见 5.8）。
- `openclaw.user.json` 覆盖层解决了「应用重新生成配置」的问题；MCP server 注册和 agent `system_prompt` 都有落地位置。
- 适配器可以做成「通用 OpenClaw 适配器」，不只服务 LobsterAI；长期收益最大。
- 额度入口是 LobsterAI 自己的 gateway/token proxy：`chat.send` 直接走额度；`openclaw agent` 必须把 `OPENCLAW_STATE_DIR` 指到 `<userData>/openclaw/state` 或显式配 gateway，否则是裸 OpenClaw，不走额度（见 5.8）。
- 比 WorkBuddy 多的是应用状态目录、gateway token、生命周期和 IM 双通道处理，所以排第二。

**Trae：本轮不做。**

- 工具面（MCP）和规则/人格（rules/skills）现在就能做；但完整适配卡在「轮次结束」。
- 插件扩展 spike 已经做了：VSIX 安装和扩展宿主都可用；但稳定 `vscode.d.ts` 没有内置 chat 的 `ChatSession`/完成事件，也没有读取最终回答的接口。卡点是 **API 不全/缺事件**，不是安装或宿主。
- `trae-cn chat` 只负责把 prompt 送进 GUI；没有 `--wait`/输出/session 事件。内部 40005 socket、toolhost IPC、内部 DB 都是私有面；`vscode.chat`/`chatSessionsProvider` 是 proposed API。
- 本轮明确放弃；如果以后重启这条线，先做一个只验证「完成信号」的 spike，拿不到就停在半接（只提供 MCP 工具）。

### 如果只能投入一条线

- **要最快可用**：WorkBuddy。
- **要开源 + 长期复用**：LobsterAI（OpenClaw 适配器）。

### 范围调整建议（2026-11-06 落定）

- **做完整轮次闭环**：WorkBuddy + LobsterAI；两者协议层已实现。
- **Trae 不做**：稳定 API 缺轮次完成信号，半接的性价比不值得本轮投入。
- **要通用 OpenClaw 生态**：先做 LobsterAI，再把同一 driver 复用到其他 OpenClaw 发行版。

---

## 7. 未验证清单（不要当结论用）

### Trae

- `trae-cn chat` 是否会在进程内等待模型回复；目前 help 没有 `--wait`/输出格式，倾向「打开 GUI 后退出」。
- **临时 profile 的激活/API 探测没有完成**：能安装 VSIX、扩展扫描器也看到了 `/tmp/trae-spike-ext`，但新 profile 看起来没有登录/没走完引导，扩展宿主没有启动，没生成激活标记。要在已登录 profile reload 才能实测 `vscode.lm` 与 chat 事件。
- `vscode.chat` / `chatSessionsProvider` 在 Trae 中是否可用、是否能观察内置 chat 的完成。稳定 d.ts 没有 `ChatSession`/`chatSessions`；proposed `chatSessionsProvider` 没随包提供 d.ts。
- Trae 是否注册了 `LanguageModelChatProvider`（`vscode.lm.selectChatModels` 是否可用）。`vscode.d.ts` 有 API，但没有找到 Trae 侧注册实现；强推断无，未实测。
- 本地自定义插件（`.trae/plugins`、`.trae-plugin/plugin.json`）是否真的被加载，`mcpServers` 字段是否生效。
- 内部 `ai-agent` socket 40005 本次没有看到监听；`agent-tool-host` Unix IPC 的协议未知。
- 内部 DB `ModularData/ai-agent/database.db` 的 `file` 类型是 `data`，不是可直接 `sqlite3` 读的库；没有公开 schema。
- Trae 的「HTTP 类型 MCP」具体是 streamable-http 还是 SSE 方言；未逐字验证。
- MCP streamable-http 的 ticket 共享能否在 Trae 的 MCP client 下稳定工作。

### WorkBuddy

**已从「未验证」转「已验证」（2026-11-06，未登录探测）**：

- `--mcp-config` + `--strict-mcp-config`：init 的 `mcp_servers` 是
  `[{name:"acb",status:"connected"}]`，工具表里有 `mcp__acb__send_message`。
- 未登录 `-p`：result 是 `error_during_execution` / `is_error:true` /
  `errors:["Authentication required…"]`，exit 0；适配器判成 `model/auth_failed`。
- `-r` 指向不存在会话：只发 `{"type":"error","error":"No conversation found…"}`，
  没有 result，exit 0；适配器识别并改用 `--session-id` 重建。
- `--tools ""` 不能关闭内置工具（Bash/Edit/Read/WebFetch 仍在 init 的 `tools` 里）。
- 多实例：两个假 CLI 实例并行，MCP URL、state/HOME、会话 id 前缀、`-r` 续接都不共享。
- 真实 CLI（未登录）在 `HOME` + `CODEBUDDY_CONFIG_DIR` 指向实例目录时，`local_storage/`
  和 `user-state.json` 落在实例目录内。

**仍未验证**：

- 已登录模型真的回话、真的调用 `mcp__acb__send_message`，以及额度扣减。
- 真实 ticket 共享的完整端到端（当前只有假 CLI + 假桥覆盖）。
- `--permission-mode` / `--allowedTools` 在真实 MCP 工具调用下是否完全不弹权限。
- CLI 与 GUI 是否共享会话/agent 配置；目前看是 `~/.codebuddy` vs `~/.workbuddy` 两套状态。
- 多实例在真实登录态下：如果 CLI 把 token 放进 OS 钥匙串，`HOME` 隔离可能只隔离
  会话记录、不隔离登录态。
- `--serve` / `--acp` 外部驱动的鉴权、事件 schema、会话映射。
- `CODEBUDDY_FORCE_HEADLESS_BUNDLE`/`CODEBUDDY_FORCE_LITE_WB_BUNDLE` 在宿主 sidecar 里的实际行为。
- 本包没有 full bundle 是**上游 WorkBuddy 发行物**，不是 Nix 裁剪；这条已经核对清楚，
  保留在此只是提醒「不要依赖 `bin/codebuddy`」。上游 npm 全量包名是
  `@tencent-ai/codebuddy-code`，不是 `@genie/agent-cli`。

### LobsterAI

**当前处置（2026-11-13）**：实例状态已在 `~/2`（独立 HOME）确认并已登录，但**免费额度用完
且 app 未运行**，LobsterAI 仍由 WorkBuddy instance 1 顶替槽位（代码保留、不回退）。额度
恢复后按 5.5 的步骤启用，不需要改代码。

**已从「未验证」转「协议层已验证」（2026-11-06，假 gateway）**：

- `chat.send` → MCP `send_message` → `agent.wait` ok → `turn_end` 的完整一轮。
- ticket 校验、chat.send 拒绝、run error、gateway 断开、超时 `chat.abort`、reset。
- 配置守卫：state 目录缺失、裸 `~/.openclaw`、quota 链路缺失、缺 gateway-port/token。

**已从「未验证」转「真 gateway 已验证」（2026-11-13）**：

- 本机实例状态在 `~/2/.config/LobsterAI/openclaw/state`：`gateway-port.json`=18789、
  `gateway-token`、`openclaw.json`（provider `lobsterai-server`、apiKey `${LOBSTER_PROXY_TOKEN}`）、
  `loopback-state.json`（token proxy 41853）；账号已登录。
- 适配器的 state/gateway 发现对真实文件解析正确（`ws://127.0.0.1:18789` + token）。
- 真 gateway 握手成功（protocol 4 / OpenClaw 2026.8.1），`chat.send` 被接受，
  真 agent 连上适配器 MCP server 并 `tools/list`，失败时也回 `turn_end`。
- 真 gateway 暴露并已修两个 mock 掩盖的协议差异：`client.id/mode` 注册表校验、
  `sessions.messages.subscribe` 用 `key`。

**仍未验证**：

- **模型真回话**：app 未运行 → token proxy 不在（`LLM request failed: network connection
  error`）；且实例日志（2026-11-13 16:45）显示「免费额度已用完，请升级套餐」。
- 模型真实调用 `mcp__acb__send_message`、成功轮次的 `turn_end`、`sessions.messages.subscribe`
  流式事件。
- **额度实际扣减未验证**：机制、endpoint、账号身份都已核对，但没有成功模型调用可对账。
- `openclaw.user.json` 覆盖 `mcp.servers` 后，应用下一次 sync 是否真的保留。
- 直接用 state 目录副本跑会触发 `WorkspaceVanishedError`（workspace 绝对路径 +
  attestation）；只能用真 state 或改 workspace。
- 应用生命周期（gateway 重启/token 轮换）与 LobsterAI 自带 QQ/微信/飞书通道的重复回复。

---

## 8. 参考来源

### 仓库内基准

- [`docs/06-harness-adapters.md`](../06-harness-adapters.md) — 两半能力、四个问题、插件 vs 进程外。
- [`harnesses/dsh/src/driver.mjs`](../../harnesses/dsh/src/driver.mjs) — 单进程三件事、ticket 共享、streamable-http/stdio。
- [`harnesses/dsh/dsh-mcp.patch.yml`](../../harnesses/dsh/dsh-mcp.patch.yml) — 每会话一个 preset、每 preset 挂自己的 MCP。
- [`harnesses/opencode/plugin/index.js`](../../harnesses/opencode/plugin/index.js) — 进程内插件、`ctx.tool.transform`、`session.prompt` + `wait`。
- [`harnesses/opencode/plugin/acb-wire.mjs`](../../harnesses/opencode/plugin/acb-wire.mjs) — 桥协议客户端。
- [`harnesses/shared/tools.mjs`](../../harnesses/shared/tools.mjs) — 工具名/描述/Schema。
- [`harnesses/shared/bridge-tools.mjs`](../../harnesses/shared/bridge-tools.mjs) — 新适配器共用的工具面选择与 `callBridge`。
- [`harnesses/shared/mini-ws.mjs`](../../harnesses/shared/mini-ws.mjs) — 手写 WebSocket 客户端（LobsterAI gateway 用）。
- [`harnesses/dsh/src/mcp-http-server.mjs`](../../harnesses/dsh/src/mcp-http-server.mjs)、[`mcp-server.mjs`](../../harnesses/dsh/src/mcp-server.mjs) — 可复用 MCP server。
- [`harnesses/workbuddy/src/driver.mjs`](../../harnesses/workbuddy/src/driver.mjs)、[`config.mjs`](../../harnesses/workbuddy/src/config.mjs)、[`codebuddy-client.mjs`](../../harnesses/workbuddy/src/codebuddy-client.mjs) — WorkBuddy 适配器实现。
- [`harnesses/workbuddy/test/multi-instance.test.mjs`](../../harnesses/workbuddy/test/multi-instance.test.mjs) — 两实例并行、端口/state/会话前缀隔离。
- [`harnesses/workbuddy/instance-1.env.example`](../../harnesses/workbuddy/instance-1.env.example)、[`instance-2.env.example`](../../harnesses/workbuddy/instance-2.env.example) — 多实例配置样例。
- [`harnesses/lobsterai/src/driver.mjs`](../../harnesses/lobsterai/src/driver.mjs)、[`config.mjs`](../../harnesses/lobsterai/src/config.mjs)、[`openclaw-gateway.mjs`](../../harnesses/lobsterai/src/openclaw-gateway.mjs) — LobsterAI 适配器实现。

### Trae

- 本机：`~/.trae-cn/installed-plugins.json`、`~/.trae-cn/plugins/trae-remote-official/lark/1.0.7/`、`~/.trae-cn/mcps/`、`~/.config/Trae CN/User/settings.json`、`~/.config/Trae CN/User/globalStorage/state.vscdb`、`~/.config/Trae CN/logs/20261106T015955/mcp-servers-main.log`。
- 应用代码：`resources/app/out/vs/workbench/workbench.desktop.main.js`（`.trae/mcp.json`、`.trae/rules/`、`workbench.action.chat.open`）；`resources/app/modules/ai-agent/meta.json`（socket 40005）；`resources/app/modules/ai-agent/libai_agent.so`（插件路径字符串）；`agent-tool-host` 启动日志（Unix IPC、external TCP disabled）。
- CLI：`trae-cn --version`、`trae-cn --help`、`trae-cn chat --help`。
- 插件 spike：临时 profile 执行 `trae-cn --user-data-dir=... --extensions-dir=... --install-extension /tmp/acb-spike.vsix --force` → `Extension 'acb-spike.vsix' was successfully installed.`；`--list-extensions` → `acb.acb-spike`。扩展宿主证据：`resources/app/out/vs/workbench/api/node/extensionHostProcess.js`、`out/vscode-dts/vscode.d.ts`、进程列表里的 `--vscode-crash-reporter-process-type=extensionHost`。
- 内部 API：`resources/app/modules/ai-agent/meta.json`（socket 40005）；`ModularData/ai-agent/database.db`（`file` 类型 `data`）；`~/.trae-cn/extensions/extensions.json`（当前 `[]`）。
- 文档：<https://docs.trae.ai/ide/add-mcp-servers>、<https://docs.trae.ai/ide/rules>、<https://docs.trae.ai/ide/skills>、<https://docs.trae.ai/ide/manage-extensions>、<https://docs.trae.ai/ide/solo-coder>。

### WorkBuddy

- 本机：`/nix/store/2056lvy4j6n0hv6bwyl9h8fff1nxw5rm-workbuddy-5.6.2.40046327/opt/WorkBuddy/resources/app.asar.unpacked/cli/dist/codebuddy-headless.js`；`~/.workbuddy/plugins/installed_plugins.json`；`~/.workbuddy/plugins/cache/workbuddy-builtin/*/.workbuddy-plugin/plugin.json`；`~/.workbuddy/settings.json`；`~/.workbuddy/mcp-tool-list.json`；`~/.workbuddy/logs/mcp-apps-diag.log`；`~/.workbuddy/logs/daemon.log`。
- Nix 模块：`~/nix-config/modules/nixos/base/custom/ai/workbuddy.nix`；`workbuddy-ai.nix`；`waypipe` 只在 `modules/nixos/desktop/networking/remote-desktop.nix` 与 `hosts/wsl-work/packages.nix`。
- CLI：`codebuddy-headless.js --version` → `2.147.0`；`--help`；`mcp list`；`mcp add --help`（`-t stdio / sse / http`）。
- 打包证据：`nix derivation show /nix/store/2495...-workbuddy-5.6.2.40046327.drv` 的 `structuredAttrs.installPhase`（`cp -r opt/WorkBuddy`，只删 koffi 非 linux_x64、patch shims、调用 repack）；`/nix/store/gmqv...-repack-asar.py`（只替换 `main/index.js`，`verify_types` 校验 entry set）；Nix 输出 `app.asar` 的 `cli/dist/` 只有 `codebuddy-headless.js`/`codebuddy-lite-wb.mjs`。
- npm 证据：`@tencent-ai/codebuddy-code@2.147.0` tarball 列表含 `package/dist/codebuddy.js`、`codebuddy-headless.js`、`codebuddy-lite-wb.mjs`、`codebuddy-broker.js`；`@genie/agent-cli` 在 npm 上 404。
- 文档：<https://www.workbuddy.ai/docs/cli/cli-reference>、<https://www.workbuddy.ai/docs/cli/mcp>、<https://www.workbuddy.ai/docs/cli/sub-agents>、<https://www.codebuddy.ai/docs/workbuddy/Changelog>、<https://www.workbuddy.cn/docs/workbuddy/From-Beginner-to-Expert-Guide/Function-Description/MCP-Guide>。

### LobsterAI

- 官网：<https://lobsterai.youdao.com>；GitHub：<https://github.com/netease-youdao/LobsterAI>。
- 本机：`/nix/store/4800zrg9yhwi5mrwd12amcqfckz7f1lq-lobsterai-unwrapped-2026.10.30/opt/lobsterai/resources/app/package.json`（2026.10.30 / OpenClaw v2026.8.1）；`resources/cfmind/package.json`（OpenClaw 2026.8.1）；`resources/cfmind/openclaw.mjs`；`resources/cfmind/docs/gateway/protocol.md`、`docs/gateway/clients.md`、`docs/cli/agent.md`、`docs/cli/acp.md`、`docs/tools/mcp.md`、`docs/cli/mcp.md`。
- 本机状态/日志：`~/lobsterai/recovery-20261022/lobsterai.sqlite`（`agents`、`mcp_servers`、`im_session_mappings`、`user_plugins` schema）；`~/.cache/wb-debug/oracle/lobsterai.log`（gateway `ws://127.0.0.1:18789`、`mcp.servers.lobster-word/excel/ppt`）。
- 应用源码映射（构建产物）：`resources/app/dist/assets/main-*.js.map` 中的 `openclaw.user.json` 覆盖逻辑。
- 额度链路证据：`dist-electron/shared/providers/constants.js`（`ProviderName.LobsteraiServer` / `OpenClawProviderId.LobsteraiServer`）；`dist-electron/main/libs/openclawConfigSync.js`（`PROVIDER_REGISTRY[LobsteraiServer]`：`normalizeBaseUrl` → `http://127.0.0.1:<proxyPort>/v1`，`resolveApiKey` → `${LOBSTER_PROXY_TOKEN}`；`lobster-model-routing` 插件 config `proxyUrl`/`token`；`LOBSTER_PROXY_TOKEN` env）；`dist-electron/main/libs/openclawTokenProxy.js`（本地 HTTP token proxy，`getAuthTokens`/`getServerBaseUrl`/`refreshToken`）；`dist-electron/main/libs/openclawEngineManager.js`（`stateDir=<userData>/openclaw/state`、`openclaw.json`、`gateway-token`、`gateway-port.json`、`OPENCLAW_STATE_DIR`）；`dist-electron/shared/cowork/llmTurnUsage.js`、`resources/app/dist/assets/endpoints-j7tS0TwR.js`（portal login/pricing/credits/usage）；`resources/keyfrom/keyfrom.json`（`official`）。
- CLI：`node .../cfmind/openclaw.mjs --version` → `OpenClaw 2026.8.1 (512bddc)`；`openclaw agent --help`。
