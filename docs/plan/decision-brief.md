# 早间决策速览（4 项）

用途：每项一分钟读完，够拍板；细节走链接，不在这里复述。

评估口径沿用 2026-11-04 裁定：**开发工作量不作为否决理由**，只看稳态成本、
可观测性/可审计性、接口面增长、可变更性。

---

## 1. Memory 远端与多机拓扑

**问题一句话**：多台机器/多个 agent 要共享同一份长期记忆时，权威源、合并与凭据怎么定？

**选项**
- A：不做，继续单机 worktree。
- B：远端裸仓库为权威源；每台机器本地 clone；每会话 linked worktree；显式 push/pull。
- C：共享文件系统/网盘直挂。

**各自代价**
- A：跨机不可用；换机/灾备靠手工拷贝；零新增接口与凭据风险。
- B：需要 remote 配置、push/pull、冲突回报、agenix 凭据与同步日志；接口面增加
  remote 字段，但 Git 语义外置，长期最可变更。
- C：弱锁与冲突不可控，不适合 Git 工作流；可观测性最差。

**推荐**：B。记忆本来就是 Git；裸仓库是现成权威源；冲突显式暴露、不自动合并；
凭据不进仓库。

**不决策会怎样**：跨机共享继续不可用；换机只能手工拷；worktree 拓扑问题会反复出现。

详见 [T9](dynamic-extensions-and-fixes.md)、[ADR-0017](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/docs/adr/0017-multi-agent-model.md)。

---

## 2. 扩展工厂与插件分发边界

**问题一句话**：ACB 要不要自己做插件 scaffold/build/install/市场/依赖安装/生产 HMR？

**选项**
- A：不做安装器/市场；只保留 `extensions_*` authoring + 控制台 reload/rollback。
- B：做本地扩展工厂（scaffold/validate/build/install/reload），不做市场。
- C：做市场、依赖安装、生产 HMR。

**各自代价**
- A：作者自己编译/部署；core 接口面最小；无供应链风险；作者体验靠文档和示例。
- B：要引入工具链、构建隔离、审批/审计、回滚；构建方式会被接口锁死；
  仍不解决分发与跨平台构建。
- C：供应链/安全边界不可控，和“平台适配器外置”冲突；明确不建议。

**推荐**：A（现在）。当前最大缺口是工具载体/动态能力，不是安装器；分发外置。
等 ABI 稳定后再评估 B。

**不决策会怎样**：作者各自写脚本；文档继续靠示例；现有插件不受影响，但外部作者
路径不会变短。

详见 [T10](dynamic-extensions-and-fixes.md)、[N7–N12](capability-gap-matrix.md)、
[插件开发指南](../08-plugin-development.md)。

---

## 3. 自动回滚与 ADR-0030 D3 措辞

**问题一句话**：插件/桥更新失败时，core 要不要自动回滚？D3“上一份已加载实现”
的措辞是否保留？

**选项**
- A：维持手动回滚；ADR 原文不改。
- B：自动回滚到上一 Nix generation + 插件 LKG，但先要有可信健康判据。
- C：只做插件 LKG 列表，不碰 Nix。

**各自代价**
- A：需要人工介入；手动路径已存在（Nix generation + 插件 LKG 列表）；零新增状态。
- B：需要区分“启动完成”与“启动成功但功能坏了”；判据不可信时会回滚正常慢启动；
  还要持久化、审计与清理；可变更性最差。
- C：范围小，但只解决插件，不解决桥二进制；仍要 LKG 存储与清理。

**推荐**：A。当前只有 `/healthz` + 进程存活，不足以判坏；自动回滚会制造新的不一致。
ADR-0030 D3 措辞保持原文，等有可信判据再改。

**不决策会怎样**：保持现状；手动回滚可用；自动回滚继续缺，但不会阻塞发布。

详见 [supervisor 设计](native-reload-and-supervisor.md)、
[回滚选项](native-reload-rollback-options.md)、
[ADR-0030](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/docs/adr/0030-unload-safe-native-plugin.md)。

---

## 4. 动态库 tool 载体 ABI

**问题一句话**：要不要把 `ToolProvider` 纳入动态库 ABI，让 `.so` 能热装工具？

**选项**
- A：暂不做；ToolProvider 只能 in-process + `ToolLoader` 边界卸载/重载。
- B：扩展动态库 ABI：manifest 声明 tools，invoke 增加 tools/tool_call 结果。
- C：只做外置进程 tool 载体（wire 协议），不做 `.so`。

**各自代价**
- A：代码仍需和宿主编译；生命周期已能边界卸载/重载；零 ABI 兼容负担。
- B：新增 wire 字段、版本兼容、FFI panic 规则、schema 传输、unload-safe 约束；
  接口面增长最大，但外部作者体验最好。
- C：进程隔离、崩溃恢复好；复用 process adapter/protocol；需要进程管理与监管。

**推荐**：短期 A，中期 B 或 C 选一个（不要两个同时做）。ToolLoader 已解决边界语义；
ABI 要等工具 schema/失败语义稳定。若要外部作者优先 C（隔离 + supervisor 已成熟）。

**不决策会怎样**：外部插件不能提供工具；作者只能用 in-process；ToolLoader 路径可用，
但不满足分发。

详见 [插件开发指南](../08-plugin-development.md)、
[扩展点目录](extension-point-catalog.md)、
[ADR-0029](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/docs/adr/0029-dynamic-extensions.md)。
