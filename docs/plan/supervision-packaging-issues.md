# Supervisor、打包、回滚与工单：已定决策与演进路径

状态：2026-11-04。**只记录决策与演进，不代表已实现。**
本文件用于防止后续遗忘「方案与如何演进」；每条固定写：决策内容 / 理由 / 当前状态 / 演进路径。

状态取值：`已定` = 用户与实现方已同意；`待定` = 尚未拍板；`待核实` = 方向已定，但事实或实现细节尚未验证。

## 1. Supervisor 平台无关

**决策内容**

- ACB 自带 `acb-supervisor`：进程管理、健康探测、回滚决策都放在跨平台代码里，不绑定 Linux、不绑定 systemd。
- Linux 上 systemd 降级为**可选后端**：supervisor 仍做决策，systemd 只承担拉起/停止/日志等平台能力。
- Windows 侧用服务包装器拉起**同一个 supervisor**；平台差异留在包装器，不复制决策逻辑。
- 现有 systemd drop-in（`acb-bridge.service.d/supervisor.conf` 的 restart/退出码/drain 约定）是**过渡形态**。

**理由**

- 决策逻辑只写一遍，避免 systemd 单元、Windows 服务、容器入口各维护一套。
- 平台后端只负责「启动/停止/报告」，supervisor 负责「什么时候重启、什么时候回滚」。
- 现有 systemd 约定（退出码 75/78、drain、StartLimit）可以保留为 Linux 后端行为，不进入核心抽象。

**当前状态**

已定（架构方向）。实现尚未开始；当前生产仍是 systemd drop-in 过渡形态。

**演进路径**

1. 抽出 supervisor 核心：输入健康/失败/启动状态，输出 restart/hold/rollback 决策；systemd 作为第一个后端。
2. Linux 保留 systemd 后端，去掉 drop-in 里的决策逻辑，只留拉起/停止/日志。
3. Windows 增加服务包装器，调用同一个 supervisor 二进制/库。
4. 容器/无 systemd 环境使用 supervisor 自拉起；平台后端可替换，不改变决策接口。

## 2. 打包与回滚基于 Nix profile，不依赖 NixOS generation

**决策内容**

- 为 ACB 建专用 Nix profile：
  - 用户级：`~/.local/share/acb/profile`；
  - 系统级：`/var/lib/acb/profile`。
- **profile generation 即回滚单位**；LKG = 上一个 generation 的 store path，并建立 GC root。
- 不装进用户默认 profile、不装进 Home Manager 主 profile，避免污染日常环境。
- `nix shell` / `nix run` 明确不适合守护进程：没有 generation、可能被 GC。
- Windows 原生不走 Nix；supervisor 改读一个 `current-path` 指针文件。

**理由**

- profile generation 天然提供「上一份可用版本」的回滚单位，不需要自建版本目录。
- 专用 profile 让 ACB 的升级/回滚与用户日常环境解耦；GC root 防止旧 generation 被清掉。
- `nix shell/run` 的临时性不适合常驻服务，必须显式排除。
- Windows 没有 Nix profile 语义，用指针文件承载同一套「当前/LKG」概念。

**当前状态**

已定。当前生产仍由 systemd 单元直接指向 store path；尚未切到专用 profile。

**演进路径**

1. 建 profile 与 GC root；把 bridge/drivers/notifier 作为一个包集合安装。
2. supervisor 读取 profile generation，记录 current/LKG store path。
3. 回滚 = 切到上一个 generation + 重启；失败则保留 current 并报告。
4. Windows 增加 `current-path` 指针文件的读写与校验，语义与 Nix generation 对齐。
5. 迁移时保留 state/config 在 profile 外，避免升级/回滚覆盖运行数据。

## 3. 测试矩阵

**决策内容**

- 容器集成：podman 跑 Ubuntu / Debian / Fedora，执行「启动 → 断依赖 → 恢复 → 回滚」剧本，使用假 harness。
- CI 原生三平台：Linux / macOS / Windows 跑 `cargo test`。
- 非 NixOS 分两档：「有 Nix」与「无 Nix」。
- 无 systemd 发行版（Alpine / Void 类）最后做，但必须验证 supervisor 能自行拉起进程；该能力 Windows 也要用。
- 已知必须覆盖的坑：
  - Windows SQLite 文件锁与单写者假设；
  - Windows 路径分隔符与临时目录语义。

**理由**

- 容器能低成本覆盖发行版差异；原生 CI 覆盖编译与平台 API 差异。
- 「有 Nix / 无 Nix」分开，避免把 profile/回滚能力当成所有 Linux 的默认前提。
- 无 systemd 场景是 supervisor 自拉起能力的最强验证，和 Windows 共用同一能力。
- SQLite/路径问题只能在真实平台暴露，必须进矩阵而不是靠推断。

**当前状态**

已定（矩阵分层）。集成剧本与 CI 矩阵尚未落地。

**演进路径**

1. 先做 Linux 容器剧本 + 原生 `cargo test`，把「断依赖/恢复/回滚」变成可重复夹具。
2. 加「有 Nix / 无 Nix」两档，验证 profile 与指针文件两种部署形态。
3. 加 Windows 原生 CI，覆盖 SQLite 锁、路径、临时目录。
4. 最后做无 systemd 发行版，验证 supervisor 自拉起；此能力与 Windows 包装器共用。
5. 已知坑逐项变成回归测试，不靠人工记忆。

## 4. 回滚判据放在 supervisor 侧策略

**决策内容**

- 内置策略必须开箱够用：
  - 启动宽限期；
  - 连续失败 N 次；
  - 窗口内累计失败；
  - 按错误类型升级（panic 类一次回滚，provider 解析失败走累计）；
  - 健康探针持续不健康 N 秒；
  - 人工强制回滚。
- 插件层只回答「fail / hold / rollback + 理由」；不直接执行回滚。
- 外部进程插件与 WASM 插件并存；官方内置策略是第三种实现。
- **关键约束：决策接口必须与数据源无关**——输入是状态快照，输出是决策；两种插件只是传输方式。

**理由**

- 把「判据」和「执行」分开，才能独立测试、替换载体、审计理由。
- 不同失败类型的严重度不同；panic 与 provider 解析失败不能共用同一阈值。
- 外部进程/WASM/内置策略共用同一快照与决策形状，避免每种载体发明一套策略。

**当前状态**

已定（策略接口与内置策略方向）。
待核实（WASM 运行时选型与跨平台构建约束；基础设施可行性已核实，见下；双载体参考实现要求见第 8 节）。

**WASM 现状核实（2026-11-04）**

- 仓库当前没有 `wasmtime` / `wasmer` / 其它 wasm 运行时依赖（根 `Cargo.toml`、`Cargo.lock`、各 crate 均无）。
- 现有设施允许新增独立 `acb-plugin-wasm` host crate，以 `ManagerPlugin`（推荐）或 `PluginLoader` 接入：
  - `PluginManager::install_manager` / `install_loader` 已支持任意树外 manager/loader；
  - manager 可通过 descriptor 提供 wake/access/tool/adapter 等 provider；
  - 平台适配器按 `connection.kind` 通过 `create_adapter` 分发，manager 提供的 adapter kind 会登记到 `adapter_providers`。
- core 不需要为此新增 host function；WASM 的策略/tool provider 只需要 JSON 数据输入输出；平台适配器可使用现有 `EventQueue` / `Outbound` / `AdapterHost`。文件、进程、网络能力由 host crate 自行决定并沙箱化，core 不提供通用 host function。
- 需要新增的最小面：`crates/plugin-wasm`（运行时依赖 + 模块 ABI）、`crates/bridge/Cargo.toml` 依赖、`main.rs` 安装调用；配置可用 env 或新增配置段；catalog 若要求准确来源，需给 `LoaderKind` / `PluginSource` 增加 `Wasm` 变体。

**演进路径**

1. 定义与数据源无关的 `StateSnapshot` 与 `Decision`（fail/hold/rollback + reason），先由内置策略实现。
2. supervisor 接入内置策略，完成启动宽限、连续/累计失败、panic 升级、健康探针、人工强制。
3. 外部进程插件实现同一决策协议，只做传输；WASM host crate 随后接入同一协议。
4. LKG 记录与回滚执行留在 supervisor；插件只给决策，不碰 profile/store path。

## 5. 反馈 / 工单

**决策内容**

- 不新造独立工单系统。
- 两段式演进：
  1. 记录 + 对外契约 + 检索（SQLite FTS）+ 订阅（ntfy / 页面）；
  2. 需要看板/多人协作时，再接自托管 Gitea / Forgejo 作为后端。
- 接口设计成 adapter 形，便于替换后端。
- GitHub Issues 仅适合联网公开场景，不作为默认。
- 解决侧与进程无关：
  - `acb feedback resolve --id N --note ...`；
  - 桥在跑走 API；桥没跑写 spool（JSONL + fsync），启动时按 id 幂等导入；
  - **禁止外部进程直接写 `acb.db`**。

**理由**

- 工单价值主要在 UI、检索、订阅；权限模型对本机单用户场景几乎无用。
- 先做记录与检索，成本低且能立即使用；看板/多人协作再升级后端。
- adapter 形接口让 SQLite/Gitea/Forgejo 可替换，数据归属和迁移路径可控。
- spool + 幂等导入让「桥没跑」也能解决工单，同时保持单写者。

**当前状态**

已定（路线与解决侧形态）。SQLite FTS、订阅、spool/CLI 尚未实现。

**演进路径**

1. 当前阶段：feedback 表 + 检索 + ntfy/页面订阅；`resolve` 走 API，桥停机写 spool。
2. 触发升级的条件：需要看板、多人协作、权限/审计或外部客户可见。
3. 迁移时数据归属：ACB 保留权威 id 与状态；导出/导入到 Gitea/Forgejo；旧表保留只读或归档，不丢历史。
4. GitHub Issues 仅在明确联网公开场景使用，不进入默认路径。

## 6. 记忆管家

**决策内容**

- 保留记忆管家，做成可配置的「触发器 + 动作 + 范围」：
  - 触发：定时 / 每 N 次记忆提交 / 冲突或会话结束 / 手动；
  - 动作：合并到本地主仓库、push/pull 远端、冲突转成一条反馈而非硬解；
  - 范围：分支与会话。
- 默认保守：本地合并自动；push 手动或定时。
- **会话侧永远不处理冲突**。

**理由**

- 记忆需要周期性整理，但不能让每个会话各自解决冲突，否则上下文和状态会互相覆盖。
- 冲突转反馈能保留现场，避免自动硬解丢信息。
- 默认本地自动、远端手动/定时，控制跨机写风险。

**当前状态**

已定（设计方向）。触发器/动作/范围配置尚未实现；远端拓扑仍受 T9 决策约束。

**演进路径**

1. 先实现本地定时合并与手动触发；会话只读，不碰冲突。
2. 加「每 N 次提交 / 会话结束」触发器；冲突生成 feedback。
3. 远端 push/pull 作为可配置动作，默认手动或定时；与 T9 memory 远端决策共用凭据/冲突策略。
4. 范围先按分支/会话隔离；需要跨机时再接远端权威源。

## 7. 扩展工厂

**决策内容**

- **先不做**扩展工厂/安装器/市场。用户已确认。
- 保留扩展点本身（插件 ABI / 载体 / manifest），不因不做工厂而收缩。

**理由**

- 机制本身不难；真实成本在供应链信任、签名、兼容矩阵、托管。
- 自建工厂会与 Nix 形成两个版本真相来源，升级/回滚边界会打架。

**当前状态**

已定（当前不做）。现有 `extensions_*` authoring 与插件 ABI 保留。

**演进路径**

- 若将来确有分发需求：只维护一份插件清单（名称 / 来源 / 期望版本）。
- 安装交给 Nix 或系统包管理器；ACB 只读清单并校验，不自己下载/安装。
- 扩展点与 ABI 保持现有契约，工厂不成为新的权限/信任中心。

## 8. WASM 双载体参考实现（已定，待实现）

**决策内容**

- WASM 的两种载体都要有**参考实现**，不只写设计：
  1. **独立进程宿主**：`acb-plugin-wasm-host` 二进制 + 示例 `.wasm` 模块 + 最小宿主协议说明，可直接照抄。
  2. **动态库宿主**：`.so` 实现 v2 插件入口 `acb_plugin_entry_v2`，内部起同一套运行时，加载同一个示例模块。
- 两个载体共享**同一个示例模块**和**同一份决策契约**：输入是状态快照，输出是 `fail / hold / rollback + reason`；相同输入必须给出相同输出。
- 两种载体都进入 CI 作为一致性测试：同一组 fixture 输入分别喂给两个宿主，断言输出一致（CI 双跑）。
- 排期：**先做 ①，② 随后**。
  - ① 的运行时在独立进程中，bridge 进程地址空间不进入 wasm 运行时；
  - ② 的 bridge **构建**同样不含 wasm 依赖，但 `.so` 被 `dlopen` 后运行时进入 bridge 地址空间，故排在 ① 之后。
- 状态：**已定（待实现）**。
- 代价：两套产物 × 三平台（Linux / macOS / Windows）+ CI 双跑；① 需要新增最小进程协议与 `ProcessProviderLoader`，② 需要跨平台 `.so` 构建与 v2 入口。

**构建与部署边界**

- `.so` 宿主**自己链接** wasm 运行时；桥的 cargo feature 不作用于 `.so` 的构建，不能靠桥的 feature 开关改变它。
- 桥默认构建本来就不含 wasm 依赖。② 复用现有 `DynamicLoader`：桥只负责 `dlopen`，不认识库里的内容。因此「桥不链接 wasm 运行时」在 ② 的**构建/链接**意义上天然成立，不需要 feature 或独立构建标签来保证。
- ① / ② 的区别是**部署选择**：装 host 二进制、装 `.so`、或都不装。它不是构建开关；用户按部署形态自主选择。
- 不做「拒绝加载 wasm 插件」的专门构建变体：默认行为是「装了才加载、不装就没有」，部署级的不安装 / 删除目录即最彻底的禁用。只有将来出现共享主机或多用户场景，才考虑增加一条配置级禁用（`enabled = false` 或不写入插件清单），且只做配置，不做构建变体。
- ② 的固有代价：`dlopen` 后运行时进入桥进程地址空间；模块崩溃会带走桥。这个风险不能靠 flag 消除，只能靠**不安装**来规避。

**核实边界**

- 动态库 v2 当前可以注册 `platform_adapter`、`wake`、`access`、`delivery`、`send`、`turn_error`、`receive`、`clock`；**不能**注册 manager、tool、task、observer、prompt、route、tool_access。
- 因此 ② 的参考实现限定为**单个决策 provider**（例如 wake），不是 manager；它**不是只能 adapter**，不触发「决策类只能走 ①」的降级。
- ① 在独立进程内承载运行时；② 在 bridge 进程内承载运行时。两者共享同一个示例模块与决策契约。
- 运行时选型（wasmtime / wasmer）与跨平台构建约束仍待核实。

**演进路径**

1. 先定义示例模块与决策契约（状态快照 / 决策结构），用同一套 fixture 锁定输出。
2. ① 实现 `acb-plugin-wasm-host`、最小进程协议、bridge 侧 `ProcessProviderLoader`，接入 CI。
3. ② 实现 v2 `.so`，内部复用同一运行时与同一模块；把 CI 扩成双载体一致性测试。
4. 若未来 v2 支持 manager/tool 等扩展点，再扩大 ② 的范围；当前不承诺。
