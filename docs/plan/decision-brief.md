# 早间决策速览（4 项）

用途：每项一分钟读完，够拍板；细节走链接，不在这里复述。

评估口径沿用 2026-11-04 裁定：**开发工作量不作为否决理由**，只看稳态成本、
可观测性/可审计性、接口面增长、可变更性。

---

## 1. Memory 远端与多机拓扑

**问题一句话**：多台机器/多个 agent 要共享同一份长期记忆时，权威源、合并与凭据怎么定？

**选项**
- A：不做，继续单机 worktree。
- B：远端**裸仓库**为权威源；远端可以是本地路径、ssh 地址，或另一台机器上的裸仓库
  （不要求 GitHub/Gitea 等托管服务）；每台机器本地 clone；每会话 linked worktree；
  显式 push/pull。
- C：共享文件系统/网盘直挂。

**各自代价**
- A：跨机不可用；换机/灾备靠手工拷贝；零新增接口与凭据风险。
- B：需要 remote 地址与 push/pull、冲突回报、同步日志；接口面增加 remote 字段。
  代价是 **ssh/文件系统权限与跨机可达性**（ssh key、目录 ACL、网络路由），不是
  托管平台 token 管理；Git 语义外置，长期最可变更。
- C：弱锁与冲突不可控，不适合 Git 工作流；可观测性最差。

**推荐**：B。记忆本来就是 Git；裸仓库是现成权威源；不绑定任何托管服务。
冲突显式暴露、不自动合并；凭据不进仓库。

**不决策会怎样**：跨机共享继续不可用；换机只能手工拷；worktree 拓扑问题会反复出现。

详见 [T9](dynamic-extensions-and-fixes.md)、[ADR-0017](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/docs/adr/0017-multi-agent-model.md)。

---

## 2. 扩展工厂与插件分发边界

**问题一句话**：ACB 要不要自己做插件 scaffold/build/install/市场/依赖安装/生产 HMR？

**选项（按 A → B → C 递进）**
- A：不做安装器/市场；只保留 `extensions_*` authoring + 控制台 reload。
- B：本地扩展工厂，验收线只有三条：**能装**（把扩展放进受管目录并登记）、
  **能验**（manifest/摘要固定，装完能核对未被替换）、**能卸**（移除登记与文件，
  不留半装状态）。B 不包含评分、搜索、自动更新、依赖市场。
- C：市场能力：评分/搜索/自动更新/依赖安装/生产 HMR。C 也不能回灌 B。

**各自代价**
- A：作者自己编译/部署；core 接口面最小；无供应链风险；作者体验靠文档和示例。
- B：要引入受管目录、摘要校验、卸载清理与审计；接口面中等；仍不解决跨平台构建。
  当前 B 草案只有 scaffold/validate/build/install/reload，**缺少“验（摘要固定）”
  与“卸”的验收线**，按本条修订后才算 B。
- C：供应链/安全边界不可控，和“平台适配器外置”冲突；明确不建议近期做。

**推荐**：A（现在）→ B（ABI/载体稳定后）→ C（只在确有分发需求且 B 的装/验/卸
稳定后）。当前最大缺口仍是工具载体/动态能力，不是安装器。

**不决策会怎样**：作者各自写脚本；文档继续靠示例；现有插件不受影响，但外部作者
路径不会变短。

详见 [T10](dynamic-extensions-and-fixes.md)、[N7–N12](capability-gap-matrix.md)、
[插件开发指南](../08-plugin-development.md)。

---

## 3. 自动回滚与 ADR-0030 D3 措辞

**问题一句话**：插件/桥更新失败时，core 要不要自动回滚？回滚状态归谁？
D3“上一份已加载实现”的措辞是否保留？

**选项（C 与 B 是层次，不是互斥）**
- A：维持手动回滚；零新增状态；ADR 原文不改。
- C（core 能力）：插件 LKG 作为桥自己的运行时状态。`plugins-lkg.toml` 记录每个
  扩展的版本、内容摘要、上次健康状态；回滚 = 改配置引用 + 下次 reload。
  **不 rebuild、不需要 root、与发行方式无关**，Nix/非 Nix 都可用。
- B（C + Nix 适配器）：在 C 之上再加 Nix generation 回滚，只负责桥二进制与基础
  配置；**不拥有** `plugins-lkg.toml`（最多生成默认值）。仅在 Nix 环境启用。

**各自代价**
- A：需要人工介入；手动路径已存在；零新增状态，但无法自动恢复。
- C：需要可信健康判据、状态文件持久化/清理、审计与回滚日志；跨平台、无 root，
  可变更性好。
- B：只覆盖 Nix 环境；需要区分“启动完成”与“启动成功但功能坏了”；且不能替代 C，
  否则非 Nix 用户无法回滚。

**Nix / 非 Nix 如何维护干净**
- 非 Nix：`state/plugins-lkg.toml` 由桥进程写（与 `state/acb.db` 同级）；升级/回滚
  只改 `config.toml` 的插件引用并触发 reload，不碰系统目录，不需要 root。
- Nix：二进制与基础配置由 generation 钉在只读 store；LKG 文件放在可写 state 目录，
  **不归 Nix 所有**；module 最多生成默认值。桥侧回滚只改配置并 reload；需要回滚
  二进制时，由 Nix 适配器读取 LKG 并提示/执行 generation switch（可选）。

**推荐**：**C 作为 core 能力先做**（跨平台、无 root）；**B 的 Nix generation 部分
作为 Nix 侧适配器后加**。两者是层次关系，不是顺序选择题；真正阻塞是**可信健康
判据**，不是 C/B 的先后。ADR-0030 D3 的措辞保持原文：运行时 LKG 不改变“上一份
已加载实现”的表述，只新增一份可回退的插件状态。

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

**推荐**：短期 A，中期以 **C（外置进程）** 作为首个跨语言载体；B（`.so`）作为后续
可选 in-process 载体。载体层可以并存，但首期不要同时做两个。ToolLoader 已解决边界
语义；ABI 要等工具 schema/失败语义稳定；若外部作者优先，C 的隔离与 supervisor 更成熟。

> 注：本节的 B/C 是 **tool 载体**（`.so` vs 外置进程），与第 3 节回滚的 B/C 不是
> 同一组选项；载体可以分层并存，回滚的 C/B 也是层次。

**不决策会怎样**：外部插件不能提供工具；作者只能用 in-process；ToolLoader 路径可用，
但不满足分发。

详见 [插件开发指南](../08-plugin-development.md)、
[扩展点目录](extension-point-catalog.md)、
[ADR-0029](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/docs/adr/0029-dynamic-extensions.md)。
