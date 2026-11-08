# ACB 自迭代架构（独立设计）

> 本文是一次“先不看既有设计”的独立推演。写作顺序严格分为两段：
> 前半部分只基于代码与约束形成判断；读 `docs/plan` 既有设计后的对照、分歧与修正，
> 统一放在第 11 节，并逐条标注“读后改变”。
>
> 约束复述：ACB 是给 dsh、opencode 等 harness 提供聊天平台能力的一层；要同时闭环
> 自反馈（issue）、自迭代（插件为主、core 也很频繁）、自恢复；core 与聊天平台、具体
> harness 解耦，策略交给插件；自迭代/自恢复不依赖操作系统与服务管理器，Windows 也
> 必须成立；nix、systemd 只作为可选增强；配置与权限必须单一数据源；历史教训是多份
> TOML 叠加导致“看起来对、实际错”。

---

## 0. 结论先行

1. **版本的身份是内容摘要，不是路径、时间或 git tag。** 一个“新版本”由一次完整的
   manifest 事务定义；构建成功只产生候选，不自动成为版本。人/agent 决定“要不要”，
   内容决定“是哪一版”。
2. **运行时只有一个权威指针。** 现有 profile 的 `current` 软链、supervisor 的
   `current.json`、`lkg.json` 三个指针并存，是漂移源；应统一为数据库中的一条激活指针，
   文件指针只做可再生的物化缓存。Windows 上软链不可依赖，指针文件/目录联接才是默认。
3. **回退目标默认是 LKG（最后一次通过验收的激活），不是“上一次激活”。** 上一次可能
   从未通过验收；把坏版本回退成“上一个坏版本”是自迭代最典型的假恢复。
4. **“成功”必须是一条带证据的验收记录。** 启动成功、健康检查通过都只是关口，不是
   验收；没有验收证据不得改写 LKG。
5. **升级执行者必须自监督，不能假设一定有人把自己拉起来。** systemd/SCM/容器入口
   都只是“重启执行器”；核心是持久化 journal + 启动计数 + 自动回退，重启是请求，
   不是前提。退出码 75/78 只是其中一种适配器编码。
6. **配置与权限各自只有一个 active revision，运行期不存在“叠加合并”。** 文件只允许
   作为引导、导入/导出和灾备存在；导入后冻结并记录摘要，运行期改动视为漂移。
7. **ACB 只拥有自己生出的进程。** onebot 在 podman 里、dsh 是 nix 包装器、opencode
   是 npm 直装，这些“外部组件”的重启只能通过 actuator 请求；没有 actuator 时，结论
   只能是“持久化的需要重启 / 放弃并进入安全模式”，不能假装自己能重启。
8. **自迭代不必以工单为原子单位。** 工单是“生成型变更”的组织单位；内源故障首先应是
   事故（incident）+ 有界处置动作（action），无法自动解决时才升级为工单。两条链共用
   候选、验证、激活、观察、验收的流水线，只在入口和审批处岔开。

---

## 1. 设计边界与不变量

### 1.1 机制 / 策略 / 适配器三层

- **机制（core，不可替换）**：版本模型、候选与激活状态机、单一写者租约、journal、
  验收证据、审计、回退上限、安全模式。
- **策略（插件/数据库，可替换）**：健康判定、观察窗口、风险分级、审批规则、重试预算、
  通知对象。策略可以收紧或放宽参数，但**不能关闭机制底线**。
- **适配器（可选，环境相关）**：nix、systemd、Windows SCM、容器运行时、podman、
  nix profile、外部组件 actuator。适配器只把“意图”变成具体动作，不决定意图。

这条边界回答了两个看似冲突的要求：“策略不要写死”与“自恢复必须可靠”。可靠性来自
机制底线，而不是来自某个策略实现永远正确。

### 1.2 不可关闭的宪法级不变量

1. 同一作用域同一时刻只有一个 active revision / 一个激活指针。
2. 候选内容不可变；激活只改指针，不原地修改产物。
3. 无验收证据不得晋级 LKG；未验证的版本不得覆盖已验证的 LKG。
4. 重试、回滚、启动次数都有上限；超限进入安全模式，禁止无限重启。
5. 一切“需要重启”的结论必须先持久化，且可重放；不得只存在于内存或日志里。
6. 审计只追加，不原地改；版本、配置、权限、激活、验收都要能追溯 actor 与理由。
7. ACB 不得声称拥有它没有生出的进程的生命周期。
8. 权限扩张不得由同一主体自批；至少需要两个独立授权来源（人 + 策略，或两个 actor）。

### 1.3 现状证据（只读代码得到的事实）

- [profile.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/profile.rs#L1470-L1516)：
  `current` 用软链实现，`atomic_switch` 在非 Unix 平台直接返回错误；Windows 目前不可用。
- [fsutil.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/fsutil.rs#L52-L88)：
  非 Unix 的 `pid_alive` 恒为 `false`，`send_sigterm` 直接失败；跨平台进程控制尚未抽象。
- [supervisor/lib.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/supervisor/src/lib.rs#L38-L75)：
  已有 `EXIT_ROLLBACK=75` / `EXIT_GIVE_UP=78`、`RollbackMode::Internal/External`、
  `LkgUpdate::OnStable/Never`、稳定窗口；回退目标读 `lkg.json`。
- [acb/run.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/run.rs#L128-L159)：
  direct 模式的父循环处理 75/78，说明“无服务管理器”的原型已存在，但仍是进程内循环，
  没有持久化 journal 与启动计数。
- [acb/unit.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/unit.rs#L20-L52)：
  systemd 模板编译期内嵌，`ACB_UNIT_*` 白名单；systemd 是当前唯一一等公民。
- [config.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/kernel/src/config.rs#L61-L127)：
  `Config::load(path, config_dir)` 是 base TOML + `config.d` 的合并模型；
  [console.html](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/bridge/src/console.html#L858)：
  控制台把改动写到 `config.d/90-console.toml`。这正是“多份 TOML 叠加”的现状。
  历史教训不是抽象故事，而是当前代码路径。
- [grants.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/grants.rs#L100-L131)：
  `PolicySource` 抽象已有，但唯一实现是 `ConfigSource`；注释里 `policy.db` 仍是“将来”。
- [plugin_meta.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/kernel/src/plugin_meta.rs#L60-L117)：
  插件已有 `effects` 与 `options_schema`，且明确“描述是文档，真正强制的是 capability 审计”。
  这为“插件声明配置形状、core 强制执行”提供了现成基础。
- [issue-service](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/issue-service/src/bin/acb-issue-tools.rs#L76-L186)：
  已有 issue、work item、订阅、结果回报与 `expected_version`；工单化有承载，但 issue
  目前不等于“可执行变更”。
- [storage/sqlite.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/kernel/src/storage/sqlite.rs#L43-L118)：
  现有表是消息/投递/反馈等业务数据，没有版本、配置 revision、策略 revision、插件私有
  存储、激活与验收表。这说明“能入库的入库”还没落到自迭代本身。

---

## 2. 核心概念与数据模型

### 2.1 七个对象，必须分清

| 对象 | 是什么 | 不是什么 |
|---|---|---|
| **Artifact** | 内容寻址的不可变字节（二进制、wasm、lua、前端资源、声明式规则） | 不是“某个目录” |
| **Version** | 一份可解析的安装清单：产物摘要集合 + 依赖闭包 + 配置 revision + 策略 revision + 启动规格 + 数据迁移声明 | 不是构建成功 |
| **Candidate** | Version + 验证计划 + 验证状态 | 不是“已上线” |
| **Activation** | 某环境/某组件在某时间段内实际运行某 Version 的记录 | 不是“文件替换” |
| **LKG** | 每个组件/环境最后一次通过验收的 Activation | 不是“上一次激活” |
| **Revision** | 配置或权限的一次不可变快照（content hash 标识） | 不是“文件版本” |
| **Incident / ChangeRequest** | 问题记录 / 变更意图 | 不是版本本身 |

这个区分是整套设计的骨架。混用“版本=目录”“回退=上一个”会直接导致第 7 节列出的腐烂。

### 2.2 统一数据平面

- **唯一权威库**：一个本地 SQLite（`acb.db`），WAL，schema 版本表。所有期望状态
  （desired state）与历史都在库里。
  > 读后修正（11.4 #1）：按既有 policy-store 的多写者/高频分离理由，改为
  > **控制库**（desired + release/activation + plugin config/storage + audit）与
  > **热运行库**（records/deliveries 等）分开；“控制库是唯一权威”这一点不变。
- **字节存储**：内容寻址 CAS 目录（`<state>/cas/<algo>/<digest>`），或 nix store
  作为可选后端（digest → store path 映射）。CAS 只存字节，**不承载“哪个是当前版本”
  的语义**。
- **文件**：只允许四类——引导位置（`ACB_HOME` / 编译期默认）、导入导出、灾备转储、
  密钥主密钥/证书等外部输入。每类都记录摘要；运行期不参与优先级仲裁。
- **物化缓存**：`current.json`、`lkg.json`、`profile/current` 都是由库派生、可删除、
  可重建的缓存，不是权威。任何缓存都要带 `fence_token`，读端发现令牌过期即重读库。

### 2.3 建议的表（语义，不锁死字段）

```
artifacts(digest PK, media_type, size, provenance_json, signature, created_at)
versions(version_id PK, component_id, manifest_json, manifest_hash,
         config_revision, policy_revision, created_by, created_at)
candidates(candidate_id PK, version_id, plan_json, state, created_at)
activations(activation_id PK, environment, component_id, version_id,
            channel, started_at, ended_at, state, evidence_ref)
lkg(environment, component_id, activation_id, since, PRIMARY KEY(environment, component_id))
setting_revisions(revision_id PK, scope, schema_version, value_json, hash,
                  actor, reason, created_at)
active_setting(scope PK, revision_id, fence_token, updated_at)
policy_revisions(revision_id PK, version_no, rules_json, hash, actor, reason, created_at)
active_policy(scope PK, revision_id, fence_token, updated_at)
secrets(namespace, key, ciphertext, key_id, rotated_at, PRIMARY KEY(namespace, key))
plugin_installations(installation_id PK, plugin_id, version_id, manifest_json,
                     granted_caps_json, config_scope, data_scope, state)
plugin_data(installation_id, namespace, key, value_json, updated_at,
            PRIMARY KEY(installation_id, namespace, key))
incidents(incident_id PK, fingerprint, severity, trigger, state,
         first_seen, last_seen, count, evidence_ref)
change_requests(change_id PK, origin, incident_id, work_order_id, scope_json,
                risk, state, requested_by, approved_by, created_at)
journal(environment, component_id, phase, target_version, rollback_version,
        attempts_json, boot_count, deadline, fence_token, updated_at)
audit(seq PK, at, actor, action, object, before_hash, after_hash, reason, evidence_ref)
```

关键点：**`active_*` 是唯一的“当前”语义**；`current.json` 只是它的投影。

---

## 3. 版本是怎么产生的、怎么标识、谁定义“这是一个新版本”

### 3.1 产生路径

**内部组件（core、插件）**

1. 构建者（人、agent、CI、插件自己的构建脚本）在**隔离沙箱**里产出字节，写入 CAS；
   同时写一份 manifest 草稿。沙箱不得触碰 active 状态。
2. core 的“发布闸门”做四件事：摘要校验、依赖闭包解析（所有插件/适配器/运行时都
   pin 到 digest）、schema 校验（配置、权限、插件 options）、兼容性校验（OS/arch/
   必要 capability）。
3. 全部通过后，在**一个数据库事务**里写入 `versions` 行。事务提交的那一刻，才存在
   “一个新版本”。

**外部组件（onebot、dsh、opencode）**

ACB 不生产这些组件的字节，因此版本定义为**有效运行身份**：

```
effective_runtime_identity = hash(
  解析后的可执行文件真实路径 + 该文件/其闭包摘要 +
  argv + 受控环境变量白名单摘要 +
  适配器配置 revision +
  harness 包装层类型（dsh / dsh-real / npm 直装 / nix 包装器）
)
```

这解决“dsh 是 nix 包装器还是 npm 直装、onebot 在 podman 还是宿主机”的问题：ACB 不
猜安装方式，而是记录**实际解析出来的运行身份**。当外部安装方式变化（nix profile
switch、npm update、podman 换镜像）导致身份变化时，产生一条 `drift` 事件，而不是
悄悄继续跑。drift 的处置有两个合法选项：作为新的外部版本“追认”并记录新的 activation
（需要重新验证），或冻结后续升级直到人工对账。默认是后者更安全，策略可放宽。

### 3.2 标识

- `version_id = sha256(canonical_manifest_json)`。
- manifest 必须包含：组件与依赖闭包、每个 artifact 的 digest、**config_revision**、
  **policy_revision**、启动规格（argv/env 白名单/工作目录/权限）、数据迁移声明、
  运行时要求、来源与构建输入摘要。
- 人类标签（`label`）只是索引，不参与身份；重命名标签不产生新版本。
- “同一版本”的判定是摘要相等，不是路径相等。复制到另一台机器不改变身份。

### 3.3 谁定义“这是一个新版本”

分三层，避免职责混淆：

- **内容定义身份**：摘要与闭包决定“是哪一版”。任何主体都不能把不同内容叫成同一版。
- **发布闸门定义“它是否构成一个版本”**：manifest 不完整、闭包不可解、schema 不过，
  就只是 candidate draft，不是 version。
- **策略/人定义“它是否可被采纳”**：通过验证、审批、风险门槛后，activation 成功才是
  “这一版被采用了”。adoption 不等于 identity。

所以答案是：**不是某个人拍板“新版本”，而是“完整清单 + 校验通过 + 事务提交”共同
定义；人/agent 定义意图与授权，内容定义身份。**

---

## 4. 快照、切换与回退

### 4.1 三种快照，不能混为一谈

| 快照类型 | 内容 | 谁保存 | 何时保存 | 用途 |
|---|---|---|---|---|
| **产物快照** | 不可变 artifact（CAS/nix store） | 发布闸门/构建者 | 候选产生时 | 回退到旧代码 |
| **期望状态快照** | config revision + policy revision + 启动规格 | core 自动 | 激活前 | 回退到旧配置/权限 |
| **运行数据检查点** | 仅插件数据迁移前的必要数据、DB schema 备份 | core 自动（按插件声明） | 数据迁移前 | 迁移失败可恢复 |

**明确不做的快照**：消息、issue、投递记录等业务数据。回退版本不得回退用户数据；否则
“恢复”会变成“丢消息”。因此数据迁移必须二选一：向后兼容（可安全回退），或声明为
前向不可逆（此时禁止自动回退，必须走“备份恢复”流程并要求停机窗口）。

### 4.2 切换

- 权威切换 = 数据库事务里改 `active_setting` / `active_policy` / activation 指针，
  并递增 `fence_token`。
- 事务提交后，物化 `current.json`（同目录临时文件 + 原子替换）。Unix 可以额外维护
  `profile/current` 软链；Windows 默认不建软链（避免开发者模式/管理员权限），由指针
  文件或目录联接承担；都不用也不影响正确性。
- 常驻进程通过 `fence_token` 感知换代；不依赖文件 mtime、不依赖“重启才生效”。
- 单一写者：激活前取 `BEGIN IMMEDIATE` 租约（带 TTL 与 owner）。并发升级被串行化。

### 4.3 回退目标

默认解析顺序（每个组件独立解析，避免“一个插件坏了回退整个 core”）：

1. 该组件/环境的 **LKG**（最后一次通过验收的 activation）。
2. 若 LKG 不可用（artifact 被 GC、启动规格失效），取**最近一个 accepted activation**。
3. 若仍无，取**基线版本**（安装时钉住、永不自动删除）。
4. 若基线也无法启动 → **安全模式**（core-only，禁用非必要插件），不继续回退。

“谁决定回到哪一版”的答案是：**策略决定规则，core 执行解析，LKG 优先；人和 agent
只能提议，不能把“上一个”硬编码成回退目标。**

### 4.4 什么算“成功”

成功不是单点事件，而是三段全过：

1. **启动成功**：进程存活 + 健康端点 + 依赖就绪（在 boot deadline 内）。
2. **观察成功**：观察窗口内无崩溃/重启、关键探针全过、错误率/延迟在基线容差内、
   业务闭环（收发/投递/会话）有真实样本。
3. **验收成功**：上述证据写入 `activations.evidence_ref` 与 `audit`，状态置为
   `accepted`，才允许把 `lkg` 指向它。

仅有 1 只能叫“booted”，不能叫成功；只有 1+2+3 才叫“这一版被接受”。

---

## 5. 升级怎么发起、由谁执行、失败如何收场

### 5.1 发起

两类入口，共用一个 `change_request`：

- **内源**：探针失败、崩溃、行为回归、能力退化、依赖告警、外部 drift。由 core 的
  观测器生成 incident；默认动作是**有界处置**（回退/禁用插件/重启/隔离候选），
  不是先开工单。
- **外源**：人或其他 agent 提出需求、报告缺陷。生成 issue/work order；需要负责人、
  验收标准与 `expected_version`，再转成 change_request。

两者的差别见第 9 节；这里只强调：**发起不是执行，提议不是授权。**

### 5.2 执行者

**Release Conductor（发布指挥器）**，是位于 payload 之外的稳定“恢复内核”，不随
payload 一起被替换。它负责：
> 读后修正（11.4 #3）：不是 bridge/core 自己 exec 自己；core 仍只排空、退出，
> 由这个外部稳定进程负责重启、回退与自身两阶段升级。

1. 冻结期望状态（读取 active config/policy revision 并 pin 进 manifest）。
2. 组候选（构建/解析/闭包）。
3. 跑验证闸门（第 6 节）。
4. 写 journal（目标、回退目标、预算、deadline、fence）。
5. 切换指针。
6. 重启它拥有的进程，或向 actuator 发请求。
7. 观察、验收、晋级 LKG 或回退。
8. 关闭 change_request / incident，写审计与通知。

**为什么要“恢复内核”独立**：如果更新器本身和 payload 一起被替换，一次坏更新会同时
带走“回退能力”。恢复内核保持极小、极少变更；更新它自己时必须两阶段交接，并保留
上一代恢复内核作为最后手段。

### 5.3 失败分类与收场

| 阶段失败 | 处置 | 用户可见性 | 是否终态 |
|---|---|---|---|
| manifest/闭包/schema | 候选隔离，旧版本不动，记 incident | 无 | 否（可重试构建） |
| 静态/行为验证 | 候选隔离，保留证据，通知变更发起者 | 无 | 否 |
| 写指针失败 | 不动旧指针；journal 标失败；重试一次 | 无 | 否 |
| 拉起失败 | 立即回退 LKG；不重试坏候选 | 短暂 | 否 |
| boot 超时/健康失败 | 回退 LKG；boot_count+1 | 短暂 | 否 |
| 观察期失败 | 回退 LKG；候选永久隔离 | 短暂 | 是（该候选） |
| 回退目标无效 | 降到下一候选；仍无效→安全模式 | 降级 | 是 |
| 回退执行失败 | 停止抖动；安全模式；通知人 | 明显降级 | 是 |
| conductor 崩溃 | 下次启动读 journal；超预算先回退再服务 | 可能一次抖动 | 否 |
| 外部 actuator 不可用 | 落盘 `requires_restart`；不假装已重启 | 取决于组件 | 否 |

**没有服务管理器时怎么把结论变成“重启”或“放弃”**：

- Conductor 自己就是父进程：先通过控制通道请求 drain/stop，超时后按 OS 能力强制终止
  （Unix 信号；Windows Job Object / `TerminateProcess`），再 spawn 新子进程。
- 它不假设自己会被重新拉起。所有结论写入 `journal`：`decision=restart` 或
  `decision=give_up`，带 target/rollback/boot_count/deadline。
- 进程启动时先读 journal：若存在未完成激活，且 boot_count 未超预算，继续；超预算则
  **先回退再开始服务**；回退也失败则进入安全模式，不再尝试。
- 如果 conductor 不是这些进程的父进程（例如容器入口、外部 systemd/SCM、用户手动
  启动），它只能把 `requires_restart` 交给 actuator；没有 actuator 时，结论是
  “持久化的待重启 + 通知 + 安全模式”，而不是无限循环。
- systemd/SCM/容器入口都只是 **RestartActuator**：把 journal 里的意图变成实际重启。
  75/78 退出码是其中一种编码，不是核心协议。

### 5.4 一个反直觉但必要的规则

**升级失败时，优先“缩小爆炸半径”，而不是“整体回退”。** 例如某插件更新导致异常：
先禁用该插件（激活其 previous plugin version 或 disable 状态），观察 bridge 是否恢复；
只有 bridge 本身受影响才回退 core。整包回退是最后手段，因为它会连带回退已经成功且
无关的变更。

---

## 6. 验证与上线

### 6.1 闸门（按顺序，任何一道失败都不得激活）

| 闸门 | 验证内容 | 失败含义 |
|---|---|---|
| G0 完整性与签名 | manifest、digest、签名/来源、闭包可解、无缺失依赖 | 候选不成立 |
| G1 schema | 配置/权限/插件 options 对 schema 与 DB 约束 | 配置不可用 |
| G2 权限差异 | 与当前 policy revision 做 grants diff；扩张需双授权 | 安全风险 |
| G3 静态烟测 | `--version`、`--help`、自检、动态库加载、wasm 实例化 | 基本不可运行 |
| G4 密封集成 | 临时 state/DB、loopback 端口、假平台/假 harness/假模型，跑关键链路 | 契约不成立 |
| G5 回放/回归 | 录制的消息、事件、会话 fixture 回放，与基线比对 | 行为回归 |
| G6 金丝雀/影子 | 旁路实例只读镜像真实事件；比较输出/延迟/错误 | 真实环境不适配 |
| G7 激活 + boot | 指针切换、进程拉起、健康端点、依赖就绪 | 启动失败 |
| G8 观察 | 时间窗 + 事件量 + 探针 + 指标容差 | 运行不稳定 |

Windows 上 G4/G6 不需要 namespace/cgroup：临时目录 + 本机 loopback + 进程隔离即可；
没有容器也能成立。若沙箱能力被削弱（如无法起假平台），G4 降级为“最小自检 + G5”，
并在验收证据里明确标注“验证强度下降”，策略可据此拒绝自动上线。

### 6.2 观察窗口多长

不要写死一个魔法数字。窗口由三部分取最大值：

```
W = max(
  W_min(risk),                         # 风险下限
  time_until(N_critical_events),       # 至少 N 个关键真实事件
  time_until(all_critical_probes_pass_twice),
  time_until(canary_parity_within_tolerance)
)
```

建议默认（可由策略调整，但不能低于下限）：

| 变更类型 | 风险下限 | 关键事件数 | 附加条件 |
|---|---|---|---|
| 纯插件、可逆、无迁移 | 10 分钟 | 50 | 所有探针连续两轮通过 |
| 插件 + 数据迁移 | 30 分钟 | 100 | 迁移校验 + 旧数据抽样可读 |
| core/bridge | 60 分钟 | 200 | 至少一次干净的重启循环 |
| 权限/策略扩张 | 24 小时异常观察 | — | 人工审批 + grants diff 归档 |
| 外部组件启动方式变更 | 30 分钟 | 100 | 运行身份 digest 稳定 |

“观察窗口”是**验收条件**，不是建议。journal 记录 deadline；如果流程被跳过，activation
标记为 `unverified`，不得晋级 LKG，并产生审计事件。

### 6.3 验收标准

必须同时满足：

1. 无意外退出/重启；探针失败数为 0。
2. 错误率 ≤ 基线 + 容差；p95 延迟 ≤ 基线 × 容差；队列深度不持续增长。
3. 关键业务闭环有真实样本（消息收发、投递、会话恢复至少各 N 次）。
4. 权限审计无未授权扩张。
5. 金丝雀/影子输出与基线在容差内（若启用）。
6. 证据完整（每项有指标/日志/探针结果的引用），且可通过审计回放。

容差和样本量由策略给，但**不得把“启动成功”降级为验收通过**。若证据因观测系统故障
而不完整，结论是 `inconclusive`：候选保持隔离；若已激活，则默认回退到 LKG，而不是
“再等等看”。

---

## 7. 跨环境成立：nix、systemd、Windows、容器

### 7.1 能力端口与缺省退让

| 能力 | 缺省实现（任何环境） | nix 增强 | systemd 增强 | Windows 增强 | 容器增强 |
|---|---|---|---|---|---|
| 状态库 | SQLite 文件 | 同左 | 同左 | 同左 | 状态放 volume |
| 产物存储 | CAS 目录 | nix store 作后端 | 同左 | 同左 | 同左 |
| 指针切换 | DB 事务 + `current.json` 原子替换 | profile generation 映射 | 同左 | `ReplaceFileW`/`MoveFileEx` | 同左 |
| 版本快照 | CAS + DB 历史 | `nix profile` generation | 同左 | 同左 | 镜像/卷快照可选 |
| 进程启停 | 自己 spawn/kill | 同左 | unit start/stop | Job Object + `TerminateProcess` | 容器内子进程 |
| 健康检查 | HTTP/TCP/进程存活 | 同左 | `WatchdogSec` 额外心跳 | 同左 | 同左 |
| 重启执行器 | Conductor 自己 | 同左 | `RestartForceExitStatus=75` | SCM / 计划任务 / 启动项 | 容器入口循环 |
| 回退物化 | journal + 指针 | nix rollback 可选 | 同左 | 同左 | 同左 |
| 沙箱 | 临时目录 + loopback | nix shell 可选 | systemd-run 可选 | 进程隔离 | 另一个容器可选 |
| 外部组件 actuator | 人工/HTTP 控制 | — | systemctl/podman | SCM/podman | podman API |

**缺省路径只需要 SQLite + 文件系统 + 进程 API。** 这是“Windows 也成立”的真正含义：
不是把 Unix 方案翻译过去，而是把 Unix 特有机制降级为可选增强。

### 7.2 各环境的分工

- **nix**：只做“产物获取 + 不可变快照 + 可选 profile generation 回退”。不是前提。
  有 nix 时，artifact 可以直接引用 store path（省去 copy），profile generation 可作为
  LKG 的额外快照层；但 DB 仍是权威。
- **systemd**：只做“启动/停止 + 看门狗 + 重启策略”。它能保证进程被拉起，但不决定
  回退到哪一版；75/78 只是它认识的退出码。没有 systemd 时，Conductor 自己监督，
  journal 保证崩溃后可续。
- **Windows**：默认用指针文件 + 原子替换；进程控制用 Job Object；服务控制是可选的
  SCM 适配器；启动项/计划任务可替代 systemd 的“拉起”职责。软链一律不作为默认。
- **容器**：容器是边界。ACB 在容器内只监督自己生出的进程；升级镜像不由容器内自行
  完成（那会与镜像不可变性冲突），而是更新 volume 中的指针/产物目录，或通过 podman
  API 创建新容器。若容器完全只读且无 volume，ACB 只能给出 `requires_restart` 并
  退让给外部编排。
- **外部组件**：onebot（podman 或宿主直跑）、dsh（nix 包装器或 npm）、opencode
  （npm 或其他）统一抽象为“LaunchSpec + actuator”。ACB 记录有效运行身份，并在
  drift 时告警；它不假装能替用户重装 npm 包或切 nix profile。

### 7.3 退让原则

任何增强缺席时，系统能力按以下顺序退让：

1. 自动化重启 → 持久化的待重启请求 + 通知；
2. 自动回退 → 持久化的回退决策 + 下次启动时执行；
3. 金丝雀 → 仅密封集成 + 回放；
4. 快照 → 仅 CAS + DB 历史；
5. 外部组件恢复 → 标记 degraded，等待 actuator/人；
6. 如果连“持久化”都做不到（只读文件系统），拒绝声称支持自迭代，只做只读运行。

---

## 8. 配置与权限如何保证单一数据源

### 8.1 单一权威 revision，禁止运行期合并

- 每个作用域（system / component / plugin installation）只有**一个 active revision**。
- 运行期读取只有一个入口：`DesiredState::snapshot()`，一次性解析所有相关 revision，
  校验 hash，返回不可变句柄。所有模块用同一句柄，不允许各自读文件再合并。
- 写入是 CAS：客户端带 `expected_revision`，服务端校验 schema，追加新 revision，
  同一事务里更新 active 指针与 `fence_token`。冲突返回明确错误，不做“最后写入者赢”。
- **不存在配置优先级、不存在 base + config.d + 90-console 的叠加**。控制台直接写
  新 revision，不再写侧文件。
- 迁移路径：提供一次性 importer，把现有 base TOML + `config.d` **解析一次**，生成
  一个 revision 并输出 diff 供审阅；导入后记录各文件摘要并标记 frozen。此后这些文件
  的改动只产生 drift 告警，不参与运行。

### 8.2 配置与权限必须同版钉住

activation 的 manifest 同时包含 `config_revision` 与 `policy_revision`。这解决一个
隐蔽的 TOCTOU 问题：验证时用配置 A，激活时配置已变成 B，却仍被当成“验证过的版本”。
任何一版变化都产生新的 version 身份，需要重新验证或明确豁免。

### 8.3 权限模型

- grants 规范化存表；`policy_version` 单调递增；每次判定记录 revision hash。
- 变更走 diff（可复用现有 `acbctl grants diff` 的思想），扩张需要双授权；收缩可以
  自动。
- 审计追加写；禁止原地改规则。紧急收紧可以即时生效（安全方向不需要审批）。
- 策略插件可以决定“谁需要审批”“观察多久”，但不能决定“权限扩张可以自批”。

### 8.4 插件存储与配置归属

插件 manifest 必须声明（core 校验，插件不能自报）：

```
id, version, kind, entrypoint, artifact_digest,
config_schema,            # JSON Schema，含默认值与约束
storage: {
  kv: bool, quota_bytes, namespaces: [...],
  migrations: [{from, to, reversible, backup}]
},
secrets: [names...],       # 只拿引用，不拿明文
capabilities: [...],
effects: [...],            # 文档；强制执行靠 capability
health: {...},
launch: {...}
```

归属规则：

- **配置值**：属于 desired state，由 ACB 的配置系统拥有，存在数据库；插件通过
  `ctx.config.get/set` 访问，写入时校验 schema、记 actor 与 revision。
- **私有运行数据**：属于插件安装实例，存在 `plugin_data`，按 `installation_id +
  namespace` 隔离；core 管配额、备份、导出、卸载清理。插件不得直接读写任意文件/库。
  > 读后修正（11.4 #2）：插件数据接口从 KV 升级为受限 `StorageHandle`（前缀强制的
  > SQL + authorizer + 单事务迁移 + 配额），KV 只作为简单插件的子集。
- **秘密**：存在 core 的 secret store；插件只拿到引用或受限句柄。日志自动脱敏；
  插件配置里禁止明文 secret（schema 层拒绝）。
- **内置插件**：同一契约，只是 artifact 是编译进去的；不得因为“内置”就绕过 schema
  与能力审计。
- **声明式插件**：无任意代码，不分配存储，配置只作为规则输入。
- 插件升级：新版本 manifest 是新的 plugin version；其 config schema 变更要兼容旧
  revision（或声明迁移）。插件数据不随版本回退自动回退，除非迁移声明为可逆且显式备份。

这样“插件的存储与配置由谁拥有”有了单一答案：**core 拥有存储引擎与命名空间边界，
插件拥有自己命名空间内的数据语义，配置由 ACB 的 revision 系统拥有。**

---

## 9. 自迭代是否必须以「工单 / 派工」为组织单位？

**结论：不是必须。** 工单是“生成型变更”的组织单位，不是所有自迭代的原子单位。
把内源故障也强行做成工单，会把“秒级恢复”变成“分钟级流程”，并制造告警疲劳；反过来，
外源需求若没有工单，会失去责任、验收与去重。

更准确的分轴不是“内源/外源”，而是：

- **反应型（reactive）**：已知有界修复——回退 LKG、禁用插件、重启、隔离候选、钉住
  已知好配置。目标是把系统恢复到已知状态，不需要“设计新东西”。
- **生成型（generative）**：需要新代码/新行为/新配置结构——插件迭代、core 迭代、
  新能力、需求变更。目标是产生一个从未存在过的版本。

| 维度 | 内源（探针/崩溃/回归/退化） | 外源（人/agent 需求/缺陷） |
|---|---|---|
| 首选记录 | **incident**（按 fingerprint 去重、计数、聚合） | **issue / work order**（负责人、验收、`expected_version`） |
| 执行单位 | **action**（有界处置剧本） | **change_request → candidate** |
| 发起者 | 观测器自动 | 人或 agent |
| 关卡数 | 1 道自动关口（爆炸半径上限）+ 处置后验证 | 4–6 道（受理/分诊、设计/审批、构建、验证、上线、验收） |
| 授权 | 反应型默认允许自动执行“回退/禁用/重启” | 生成型必须有人或策略授权；权限扩张双授权 |
| 失败后 | 升级为 incident 的 escalate 状态；仍无解才转工单 | 保持工单，记录失败证据，重新评估 |
| 典型时延 | 秒到分钟 | 小时到天 |

### 9.1 共用与分叉

**共用（必须只有一套）**：候选构建、manifest/闭包校验、验证闸门、权限 diff、激活
指针、journal、观察与验收、LKG 晋级、回退执行、审计与通知。两条链若各有一套发布
机制，必然漂移成“两套真相”。

**分叉（必须明确不同）**：

1. **入口与身份**：incident 用 fingerprint 去重；work order 用 issue ID + 负责人。
2. **紧迫性与预算**：反应型允许更短观察窗、更激进的自动回退；生成型必须完整验证。
3. **审批**：反应型的“收缩/恢复”可自动；生成型的“扩张/新行为”需审批。
4. **爆炸半径**：反应型默认只动最小集合（单插件/单组件）；生成型可以整包。
5. **通知对象**：反应型通知运维/值班；生成型通知需求方与验收人。
6. **关闭语义**：incident 关闭 = 恢复且有证据；work order 关闭 = 验收通过且需求方确认。

### 9.2 取舍与代价

- 若强制“一切皆工单”：恢复变慢、值班人员不填单、重复单爆炸、自动化退化为“发单
  机器人”。代价是可用性。
- 若“内源完全无记录”：无法复盘、无法发现反复回退的根因、无法审计自动决策。代价是
  可维护性。
- 若两条链各建一套发布：验证标准不一致、指针语义分叉、回退互踩。代价是正确性。
- 本方案的选择：**记录统一（都落 audit + change_request），执行分轴（incident/action
  vs work order/candidate），流水线共用**。代价是需要维护两套入口模型，但换来确定性
  与速度。

补充一条：即使是 agent 自己发现并自己修，也应留下 `change_request` 与 provenance；
但 agent 不得自批权限扩张。这可以用“双钥匙”实现：策略判定 + 独立审计者（人或另一个
agent）批准。
> 读后修正（11.4 #4）：按 feedback-workitem / issue-system 的既有约束进一步收紧——
> agent 只能提交/评论/提议，不能自行把工作项改成 resolved，也不能自验收；
> 反应型自动动作（回退/禁用/重启）是 core 机制，不是 agent 越权操作。

---

## 10. 这套东西最容易烂掉的地方，以及怎么防

| # | 腐烂点 | 症状 | 防护 |
|---|---|---|---|
| 1 | 多指针并存 | profile `current`、`current.json`、`lkg.json` 各自漂移；重启后行为不一致 | DB 单一权威 + 物化缓存 + `fence_token`；缓存可删可重建 |
| 2 | 多份 TOML 叠加 | 控制台改了却没生效；不同进程读到不同配置 | 禁止运行期合并；单 revision + 单读取入口 + importer 冻结旧文件 |
| 3 | 路径当版本 | 源目录被改、nix store 被 GC、回退指向已不存在的路径 | 内容摘要身份 + CAS 保留 + 启动规格摘要 |
| 4 | 回退到“上一个” | 回退到同样坏的版本，形成假恢复循环 | 回退目标 = LKG → accepted → baseline → 安全模式 |
| 5 | 无界重试/抖动 | 崩溃-重启-再崩溃，日志刷屏，服务不可用 | 启动计数 + 预算 + 指数退避 + 超限安全模式 |
| 6 | 观察窗被跳过 | 为了“尽快恢复”直接晋级 LKG，坏版本被当成好版本 | journal deadline + `unverified` 状态 + 审计告警 |
| 7 | 验证退化为“能启动” | 测试 flaky，大家开始忽略失败；闸门形同虚设 | 证据化验收 + flaky 测试隔离 + 最低闸门不可关 |
| 8 | 双决策脑 | supervisor 与 conductor 都判断回退，互相覆盖指针 | 单一写者租约 + 明确决策所有者 + 幂等动作 |
| 9 | 外部组件漂移 | nix profile/npm/podman 在 ACB 背后换了版本，回退目标失效 | 有效运行身份摘要 + drift 事件 + 追认/冻结二选一 |
| 10 | 插件私存文件 | 数据散落在插件目录、卸载丢数据、备份不完整 | 命名空间化 `plugin_data` + 配额 + 导出 + 卸载清理 |
| 11 | 秘密进配置 | 明文 token 写进 TOML/DB/日志 | secret store + 引用传递 + schema 拒绝明文 + 日志脱敏 |
| 12 | GC 删掉唯一回退 | 清理策略只看“旧”，不看 LKG/基线 | 保留 LKG + 基线 + 最近 N 个 accepted；GC 前检查引用 |
| 13 | 更新器更新自己 | 坏更新同时带走回退能力 | 独立恢复内核 + 两阶段交接 + 保留上一代内核 |
| 14 | 数据库成为单点 | 文件损坏/锁死/迁移失败，全部历史丢失 | WAL + 定期备份 + 迁移前快照 + 恢复演练 + 导出指针 |
| 15 | 策略插件变成“关闭安全” | 某个策略实现返回“永不回退/永不放弃” | 宪法级不变量不可关闭，只可收紧 |
| 16 | 并发变更互踩 | 两个 agent 同时升级不同插件，指针互相覆盖 | 单写者租约 + change_request 串行化 + 组件级锁 |
| 17 | 时间与时区 | 窗口/超时用本地时间或墙上时钟，DST 出错 | 单调时钟算时长，UTC 存时间戳 |
| 18 | 只读/无状态环境被误称支持 | 容器只读却声称能自迭代，实际无法落盘 | 启动时探测能力；不满足则拒绝声称，只做只读运行 |

最危险的不是单点故障，而是 **1、2、4、7 的组合**：多指针 + 多配置源 + 回退到坏版本 +
验证退化。它们一起会把“自愈”变成“稳定地运行错误的东西”。所以不变量要放在机制里，
而不是文档里。

---

## 11. 与 `docs/plan` 既有设计的对照（读后补写）

> 本节在完成上述独立设计之后才阅读既有文档；此处逐条列出分歧、遗漏，以及被文档
> 改变的结论。阅读范围：`acb-upgrade-chain.md`、`acb-profile.md`、
> `acb-policy-store.md`（实际位于 `docs/design/`，不在 `docs/plan/`）、
> `feedback-workitem.md`、`issue-system.md`、`supervisor-policy-interface.md`、
> `native-reload-and-supervisor.md`、`backend-capability-matrix.md`。`handoff`
> 只用于确认当前落地状态，不作为设计依据。

### 11.1 先说一致的部分

既有设计不是从零开始，它已经把“发起、执行、指针、对账、退出码、回滚、事件”串成了
一条相当完整的链。以下判断我与它一致，而且它比我的独立稿更具体：

| 主题 | 既有设计 | 我的评价 |
|---|---|---|
| 发起与执行分离 | ACB 只写请求，不碰指针、不碰子进程、不调用 nix/systemctl；acbl 是唯一执行者（[acb-upgrade-chain.md](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L54-L67)） | 正确，且比“conductor 什么都做”更安全。请求里禁止 `command/args/env` 也是对的（[L148-L181](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L148-L181)） |
| 单一提交点 + 投影 | `current.json` 是执行真值，`profile/current` 是投影；跨目录不做假原子（[L231-L265](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L231-L265)） | 正确。我的“单一权威指针”与它同向；区别只在权威放在文件还是库（见 11.2） |
| journal + 启动对账 | 崩溃窗口由启动对账封住，不猜损坏状态（[L130-L137](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L130-L137)） | 正确，这是整套设计里最值得保留的工程细节 |
| store/copy 双策略 | nix store 可选，copy 兜底；非 Unix 明确拒绝 store，不静默降级（[acb-profile.md](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-profile.md#L19-L22)、[upgrade-chain L267-L285](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L267-L285)） | 正确，符合“nix 只是可选增强” |
| Windows 不依赖软链 | `current.pointer` + copy 策略 + 父进程重拉（[L419-L429](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L419-L429)） | 正确，也确认了我读代码时的判断：现有 `atomic_switch` 在非 Unix 直接报错 |
| 75/78 与后端分工 | systemd/`acbctl run` 能区分；容器必须在入口内消化（[backend-capability-matrix.md](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/backend-capability-matrix.md#L5-L48)） | 正确。退出码是合同，但只是适配器编码，不是核心真相 |
| 无 LKG 不自动升级 | 首次 seed 写 LKG；没有可用 LKG 时拒绝自动升级（[upgrade-chain L17-L20](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L17-L20)） | 正确，比我“回退到 baseline”的默认更保守 |
| 失败冷却与冻结 | `failures.json`，30s/2m/10m/1h/6h，连续 3 次冻结（[L362-L382](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L362-L382)） | 正确，是“有界恢复”的具体化 |
| 能力声明 | `capabilities.json` 的 `restart_owner`，无重拉方时拒绝升级（[L394-L408](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L394-L408)） | 正确，等价于我说的“退让原则” |
| 事件先 intent 后 result | 不可逆动作先写 intent；崩溃补录（[L624-L649](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L624-L649)） | 正确，比只在内存里做状态机可靠 |
| 单一实体单一源 | 同一逻辑实体只允许一个源；`config.d` 导入后归档；控制台写库（[acb-policy-store.md](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/design/acb-policy-store.md#L623-L646)） | 正确，且比我写得更细。我的第 8 节与它基本重合 |
| 插件存储归属 | 默认同库分表 `plugin_<id>_*`；只有 core 开句柄；前缀强制；迁移单事务（[L648-L735](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/design/acb-policy-store.md#L648-L735)） | 正确。我的“KV 命名空间”过于简化，读后改成 StorageHandle |
| 策略插件化 | 决策接口 + 外部策略 + 超时/非法输出降级 + LKG 语义（[supervisor-policy-interface.md](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/supervisor-policy-interface.md#L297-L385)） | 方向正确；但只覆盖健康决策，不覆盖发布决策（见 11.3） |
| core 不自 exec | 进程重启是 native 生命周期的唯一可信边界；core 只排空、退出（[native-reload-and-supervisor.md](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/native-reload-and-supervisor.md#L10-L53)） | 正确。我读后把“conductor 自监督”改写为“payload 之外的稳定恢复内核”（见 11.4） |
| 工单/core 分层 | core 只有 Record/Transition/Timeline；工作流语义在挂载层；agent 不能改状态（[feedback-workitem.md](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/feedback-workitem.md#L26-L82)）；执行者不能自验收（[issue-system.md](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/issue-system.md#L99-L114)） | 正确，且强化了我第 9 节的“工单不是原子单位” |

结论：**这不是“我的设计 vs 一份错误设计”，而是两套已经接近的方案在几个关键支点上
分叉。** 下面只列真正影响正确性的分歧。

### 11.2 我认为错的地方（按严重度排序）

#### 错误 1：执行真值放在文件里，与“能入库的入库”方向冲突

既有升级链把 `current.json`、`lkg.json`、`journal.json`、`failures.json`、`active.json`、
`history/*.result.json` 全部作为运行时真相（[L27-L52](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L27-L52)），
同时 policy-store 又把配置、权限、插件注册、审计放进 `policy.db`。结果是系统有两套控制
平面：一套文件状态机，一套数据库。它们各自都很谨慎，但**没有一个事务能同时覆盖
“配置 revision 切换”和“版本指针切换”**。

具体后果：

- `current.json` 与 `lkg.json` 是两份可独立损坏的 JSON；`journal` 与 `failures` 又是
  两份；`profile/current` 再是一份投影。四份东西靠“启动对账”维持一致，而启动对账只会
  在进程重启时发生；运行期没有对账点。
- 事件流水又增加了 `state/events/producers/*` 与 `delivery`，加上 `supervisor.log` 和
  `policy.db` 的 `audit`，同一动作最多有四种记录。它们不是同一事务写出的。
- 这套东西在单机、单用户、低频升级时能工作；一旦插件迭代频繁（本需求明确说 core 也
  很频繁），文件状态机的组合爆炸会成为主要故障源。

我的替代：**控制库是唯一真相，文件是物化缓存或投递信箱。**

- `activations`、`lkg`、`journal`、`failures`、`change_requests`、`audit` 都在控制库；
  指针切换、配置/策略 revision 切换、LKG 晋级在一个事务里完成。
- `current.json` / `profile/current` 是派生缓存，带 `fence_token`；损坏可重建，读端
  发现过期就重读库。
- 请求 mailbox 可以保留为文件或独立队列，作为**权限边界**（bridge 只写自己的投递区，
  不直接写控制库），这点我接受并且读后强化了（见 11.4）。但 mailbox 只承载“意图”，
  不承载“执行真值”。

这不是“文件和库谁更高贵”，而是：**多文件状态机没有跨文件事务，而自迭代最需要的恰好是
“版本 + 配置 + 权限 + 指针”一起成功或一起失败。**

#### 错误 2：把“60 秒 HTTP 200”当成升级成功，验收标准太弱

既有设计明确：LKG 是“一条可执行命令”，健康 = `GET` 返回 200 或进程存活，连续健康
60 秒就提升 LKG（[supervisor-policy-interface.md L348-L361](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/supervisor-policy-interface.md#L348-L361)）；
升级链沿用同一套参数（[L312-L333](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L312-L333)）。
`lkg_update=never` 被称为 canary，但它只是“不提升 LKG”，并没有并行影子或输出比对。

问题不在“保守”，而在**保守错了维度**：

- 60 秒只能证明“进程没死、端口能应答”，不能证明消息收发、路由、投递、会话恢复、
  插件输出正确。行为回归完全可以在 60 秒后发生。
- 健康 = HTTP 200 对 bridge 成立，对 onebot/harness 这类外部组件不成立；
  它们的“成功”是消息真的送达，而不是端口开着。
- LKG 只记命令（后来补了 generation + config sha），仍然没有把 **policy revision**
  钉进去。配置或权限在验证后、激活前被改，旧 LKG 也回不到当时的权限状态。
- 60 秒窗口对 core 太短；对纯插件可能又太长。固定值无法覆盖“插件、core、权限、
  外部组件”四类风险。

我的替代：把 60 秒降级为 **boot gate**，另设 **acceptance gate**：观察窗口按风险取
`max(下限, 关键事件数, 探针两轮通过, 金丝雀一致)`；验收证据落库（见第 6 节）。
LKG 指向 **activation**（含 version manifest + config revision + policy revision +
evidence），而不是一条命令。

#### 错误 3：只允许一次自动回退、然后 78 停机，爆炸半径不可控

既有语义：回退后清空 LKG，回滚版本再失败就 `fail` → 退出 78，不再自动拉
（[upgrade-chain L335-L357](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L335-L357)、
[supervisor-policy L362-L385](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/supervisor-policy-interface.md#L362-L385)）。
这在“core 只有一个整体版本”的模型里是合理的止损；但本需求里**插件迭代是主场景**，
插件坏了不应该让整个 bridge 停机。

- 插件导致的问题，第一反应应该是禁用/回退该插件，而不是回退 core 整包。
- 整包回退会连带撤销已经成功、无关的变更，制造“越修越坏”。
- `78` 是“放弃”，不是“降级运行”。聊天平台能力本身可以继续提供，只是缺一个插件；
  直接停机把可恢复故障变成了不可用。

我的替代：**组件级恢复阶梯**（见 5.3）：

1. 禁用/回退单个插件；
2. 回退插件闭包；
3. 回退 core；
4. 安全模式（core-only，非必要插件禁用，只读/降级能力）；
5. 只有 core 本身无法运行时才 `78`。

同时保留既有设计的“防抖动”意图：每个组件每轮只允许一次自动回退，跨轮用预算和冷却，
不做 A/B 反复横跳。**“不反复横跳”和“一次都不许多级恢复”不是一回事。**

#### 错误 4：更新器与 payload 同代，存在自替换死锁

`acbl`（launcher + supervisor）位于 `profile/current/bin`，而 generation 切换会替换
整个 payload（[acb-profile.md L8-L17](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-profile.md#L8-L17)）。
也就是说：**执行回退的进程，和被回退的产物，是同一代文件。**

- Unix 上，运行中的可执行文件可以继续用旧 inode，问题被掩盖；
- Windows 上，运行中的 `.exe` 不能被覆盖/重命名，copy 策略会直接失败
  （既有文档只说了“copy + pointer”，没提在用的 launcher 自身）；
- 新 generation 里的 `acbl` 如果启动即坏，外部重启方拉起的仍是坏 `acbl`，
  而“回退到 LKG”的逻辑也在坏 `acbl` 里 —— **没有可信的回退执行者**。

我的替代：**稳定恢复内核（recovery kernel）放在 payload 之外**，极小、极少更新；
它负责指针、journal、启动计数、回退与安全模式。payload（bridge + 插件）随便换。
更新恢复内核自身时必须两阶段交接，并保留上一代内核作为最后手段。
这比“让 core 只排空、不做重启”更彻底：既有设计正确地把“重启”交给外部，但没有把
“外部”本身做成可回退的。

#### 错误 5：版本身份是时间戳 ID，配置/策略没有进入身份

既有 generation ID 是 `gen-<unix_ms>-<8hex>`（[acb-profile.md L54-L55](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-profile.md#L54-L55)），
`meta.json` 里只有 `config.sha256`（[L39-L62](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-profile.md#L39-L62)）。
后果：

- 同样的字节、同样的配置，两次 `add` 会得到两个“不同版本”，无法幂等去重；
- 跨机器无法判断“这是不是同一版”，回退目标依赖本地 ID；
- 配置 sha 只是记录，**policy revision 根本没有出现**；activation 也没有把两者作为
  身份的一部分锁定；
- 升级链的 `expected_current_id` 是乐观并发，不是内容一致性。

我的替代：`version_id = sha256(canonical manifest)`，manifest 显式包含 artifact 集合、
依赖闭包、config revision、policy revision、启动规格、数据迁移声明。generation ID 保留
为**本地别名**，兼容现有布局；但“是不是同一版”由内容摘要回答。这样也顺带解决
“验证时配置 A、激活时配置 B”的 TOCTOU。

#### 错误 6：把“payload 完整”当成验证，没有候选验证与权限闸门

升级链的 `validated` 状态只检查 schema/TTL/请求者/目标 generation 存在或 payload 完整
（[L98-L103](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L98-L103)）。
这能挡住“文件缺了”，挡不住“行为坏了”“权限变大了”“插件 schema 不兼容”。

- 没有静态烟测、密封集成、回放/回归、影子比对；
- 没有 grants diff。policy-store 有 diff 工具，但升级链没有把它接进激活前闸门；
- 没有审批。只要管理员发起，就能切；
- 配置 revision 与 policy revision 不在同一验证快照里，验证结论无法复用到激活。

我的替代：G0–G8 闸门（第 6 节），其中 **G2 权限 diff 与 G1 schema 必须与 activation
的 config/policy revision 同版**；扩张权限需要双授权。验证结论写入 evidence，激活时
校验“产物摘要 + 配置 revision + 策略 revision”三者与验证时一致，否则重新验证。

#### 错误 7：跨外部重启的 boot 计数不可靠

升级链承认“supervisor 连续失败计数只在内存，重启后归零；跨升级止损靠 failures.json”
（[L89-L91](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L89-L91)）。
但 `failures.json` 是**升级层**的计数，不是**单次 activation 的启动层**计数。一个
“启动 40 秒后崩溃”的坏版本，会在每次外部重拉后重置 supervisor 内存计数，可能长期
在“崩溃—重拉—崩溃”之间循环，而 `stable_window=60s` 永远到不了，`failures.json`
也未必被写入。

我的替代：`journal` 里持久化每次 activation 的 `boot_count`、`deadline`、`target`、
`rollback_target`。启动时先读 journal：超过预算就先回退再服务；这等价于 systemd 的
`StartLimit`，但不依赖 systemd。

#### 错误 8：事件流水有四套记录，缺少唯一事实源

升级链设计了 `state/events/producers/*`、`delivery/*`、`emergency.jsonl`，并明确
`supervisor.log` 继续保留；policy-store 又有 `audit` 表；issue 服务有 `issue_events`
（[L497-L662](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L497-L662)）。
每一套都有存在理由（跨进程、离线、排障、对外时间线），但组合起来**没有唯一事实源**：

- 同一动作的“结论”可能只在事件文件里，不在 `audit` 表里；
- `supervisor.log` 与事件流的 `at_ms` 需要人工对齐；
- `emergency.jsonl` 是“写失败后的降级”，但它的补录规则与事件流又不同。

我的替代：**控制库 `audit` 是唯一判定事实源；对外投递用 outbox 表**；
文件只保留两类：崩溃时 DB 不可用的 emergency，以及跨 uid 的只读导出。既有的
intent/result 时序保留，但落在同一个事务域里，而不是四个目录。

#### 错误 9：GC 只保护 latest N + current + LKG，没有永久 baseline

`profile gc` 默认保留最新 N + current + LKG（[acb-profile.md L91-L93](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-profile.md#L91-L93)）。
升级链额外保护 inflight 目标（[L294](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L294)），
但没有“永不删除的基线”。如果 LKG 被反复替换，最后一次可用的旧版本可能已经被 GC；
再遇到新版本和 LKG 都坏，就只剩 `78`。

我的替代：每个组件保留一个 **baseline**（安装时钉住，GC 永不删除），外加 LKG 和
最近 N 个 accepted；GC 前做引用检查（LKG/baseline/inflight/当前运行）。

### 11.3 我认为的遗漏

1. **插件迭代没有完整发布链。** 升级链只覆盖 core generation；插件侧只有运行时 LKG
   （失败计数 3 次/600 秒窗口，回退后丢弃新候选）（[native-reload L212-L256](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/native-reload-and-supervisor.md#L212-L256)）。
   缺 candidate、manifest、验证、配置 pinning、per-plugin activation 历史、插件数据迁移
   闸门。插件是本需求的主迭代对象，不能只有“坏了回退”，没有“上线前验证”。
2. **外部组件的有效运行身份与 drift 未覆盖。** dsh 可能是 nix 包装器（dsh/dsh-real）
   或 npm 直装，onebot 可能在 podman 或宿主机，opencode 多种装法。既有文档把
   dsh 的 profile/generation 留在文件，但没有记录“解析后的可执行文件 + argv + 环境
   摘要”。外部安装方式一变，LKG 指向的命令可能已经不存在；回退会指向一个失效的运行
   规格。需要 `effective_runtime_identity` + drift 事件。
3. **配置/权限与 activation 的同版 pinning。** `meta.config.sha256` 只是记录；policy
   revision 没有进版本，也没有进 `current.json`/`lkg.json`。这是最隐蔽的遗漏：
   验证时和回退时的权限可能不是同一份。
4. **验收证据模型缺失。** 事件是“发生了什么”的叙述，不是“凭什么判定成功”的证据。
   需要 `acceptance` 记录：探针结果、指标窗口、真实业务样本、权限 diff、金丝雀比对，
   每项可引用、可回放。否则 `stable` 只是“时间到了”。
5. **不可逆数据迁移的边界。** native-reload 明确“插件回滚不负责数据回滚”
   （[L209-L210](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/native-reload-and-supervisor.md#L209-L210)），
   policy-store 规定插件版本低于库版本时拒绝加载（[L723-L735](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/design/acb-policy-store.md#L723-L735)）。
   这防住了“回退跑坏 schema”，但没有规定：什么时候允许自动回退、什么时候必须先备份、
   什么时候只能“恢复备份 + 停机”。需要 manifest 里的 `migration: reversible /
   backward_compatible / irreversible` 三态声明，并据此在发布闸门里禁止/允许自动回退。
6. **安全模式没有定义。** 78 是“放弃”，但放弃之后跑什么？是不跑、core-only、还是
   只读？容器里 78 还可能被编排层当普通失败重拉。需要明确的 `safe_mode` 状态：
   禁用非必要插件、只保留收发/投递、只读控制台、拒绝自动升级，等待人处理。
7. **权限扩张的双钥匙没有和发布链连起来。** policy-store 有审批、角色、委托，但没有
   规定“升级请求携带 grants diff 时，必须由两个独立主体批准”。我的第 8 节要求；
   既有设计只覆盖了“谁能发起升级”（默认仅控制台管理员），没覆盖“发起者能否批准
   自己带来的权限扩张”。
8. **观察窗口没有风险分级。** 60 秒一刀切，既不区分插件/core，也不区分是否涉及
   权限、数据迁移、外部组件。需要第 6 节的窗口公式和默认表。
9. **恢复内核自身的升级流程缺失。** 既有设计把 acbl 放在 generation 里，但没有
   “acbl 更新失败怎么办”“上一代 acbl 是否保留”“Windows 如何替换正在运行的 exe”。
   这是第 11.2 错误 4 的遗漏面。
10. **内源故障没有事故模型。** feedback/issue 是用户或 agent 主动报告；系统自己发现的
    探针失败、崩溃、回归、能力退化，没有 incident/fingerprint/自动处置剧本。升级链
    允许请求带 `issue_id`，但“没有 issue 的内源故障”只能落系统事件流，没人负责关闭。
    这正是第 9 节要补的。
11. **供应链来源没有进入版本。** `profile add` 校验目录布局和文件摘要，但没有构建者、
    构建输入摘要、可选签名。对“自迭代”来说，知道“这版是谁用什么输入产出的”和知道
    “字节没变”同样重要。
12. **发布策略没有插件化。** supervisor 策略只覆盖健康/回退决策；发布侧的 TTL、
    观察窗口、是否允许无 LKG、是否需要审批、冻结阈值仍写死在文档里。按本需求的
    “策略交给插件”，这些也应通过 policy provider 暴露，同时保留宪法级不变量。

### 11.4 读文档后我改变/修正的结论（明确标注）

| # | 我原来的判断 | 读后修正 | 触发文档 | 性质 |
|---|---|---|---|---|
| 1 | 所有状态都进一个 `acb.db` | 拆成**控制库**（desired state + release/activation + plugin config/storage + audit）与**热运行库**（records/deliveries 等）；理由是多写者/低频与高频单进程不应共用事务域 | policy-store §2.1（[L63-L74](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/design/acb-policy-store.md#L63-L74)） | **改变**：从“一个库”改为“一个控制权威 + 分离热库” |
| 2 | 插件存储只提供 KV 命名空间 | 改为 `StorageHandle`：受限 SQL、表前缀强制、authorizer、单事务迁移、配额；KV 只是简单插件的子集 | policy-store §3.12/§3.13（[L648-L735](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/design/acb-policy-store.md#L648-L735)） | **改变**：接口从 KV 升级为受限 SQL |
| 3 | “Conductor 自监督，core 自己重启子进程” | 明确为 **payload 之外的稳定恢复内核**；core 仍只 drain/exit，不自 exec。恢复内核负责重启与回退，且自身可两阶段升级 | native-reload §0/§1（[L10-L53](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/native-reload-and-supervisor.md#L10-L53)） | **修正措辞/架构**：不是 core 自重启，是外部稳定进程 |
| 4 | agent 可以发起并执行反应型修复 | agent 只能提交/评论/提议，不能自改工作项状态或自批权限；自动回退/禁用/重启是 core 机制，不是 agent 越权 | feedback-workitem §2.2（[L54-L82](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/feedback-workitem.md#L54-L82)）；issue-system §1.4（[L99-L114](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/issue-system.md#L99-L114)） | **改变**：增加“agent 不可自验收/自关闭”的硬约束 |
| 5 | 所有请求一律进控制库 | 承认**文件/独立 mailbox** 作为权限边界与离线入口是合理的；但 `current/lkg/journal/failures/history` 的执行真值应进控制库，mailbox 只承载意图 | upgrade-chain §3.3/§7（[L208-L217](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L208-L217)、[L431-L441](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L431-L441)） | **部分改变**：接受 mailbox 分离，拒绝文件作为执行真值 |
| 6 | 文件只允许 bootstrap/import/export/灾备 | 补充：部署拓扑、端口、unit、程序路径可以留文件/Nix；但必须把它们的摘要 pin 进 activation manifest | policy-store §2.2（[L76-L129](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/design/acb-policy-store.md#L76-L129)） | **细化**：不是所有文件都要入库，但所有影响运行的输入都要可摘要 |
| 7 | 回退目标 = LKG → accepted → baseline → 安全模式 | 保留，但承认“单组件单步回退、禁止 A/B 横跳”的既有约束；多级只用于组件级降级，不做整包来回切 | supervisor-policy §9.2 | **细化**：从“多级回退”改为“组件级降级阶梯” |
| 8 | 事件/审计全进 DB | 部分接受文件事件流：跨进程、离线、issue 投递用 outbox/文件；但**判定事实**仍以控制库 `audit` 为准，文件是投递副本 | upgrade-chain §9 | **部分改变**：从“全 DB”改为“DB 判定 + 文件投递” |
| 9 | 版本身份 = 内容摘要，时间戳 ID 是错的 | 内容摘要仍是身份；但接受 generation ID 作为本地别名和兼容层，不再要求废除它 | acb-profile §2 | **未改变，补充兼容策略** |
| 10 | Windows 软链不可用是主要问题 | 既有升级链已规划 copy + pointer，所以这不是“未被发现的设计缺陷”，而是**实现缺口**；我把重点转向“Windows 正在运行的 exe 无法替换” | upgrade-chain §6.3 | **优先级改变**：从“软链”转到“恢复内核替换” |

**没有改变的结论**：版本身份应为内容摘要；LKG 必须是带证据的 activation；观察窗口
按风险分级；组件级恢复优先于整包回退；配置/策略必须与版本同版钉住；ACB 只拥有自己
生出的进程；策略可换但安全不变量不可关。

### 11.5 与工单/派工设计的对照

既有文档在“工单”上有两块很好的基础：

- issue 服务把 Issue 与 WorkItem 分开，WorkItem 是 Issue 的执行切片，必须有
  `issue_id`；执行者不能默认验收自己的成果（[issue-system L58-L84、L99-L114](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/issue-system.md#L58-L114)）。
- feedback 把 core 压到 Record/Transition/Timeline 三个原语，工作流语义放挂载层；
  agent 只能提交/评论，不能改状态（[feedback-workitem L26-L82](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/feedback-workitem.md#L26-L82)）。

这**强化**了我第 9 节的结论，而不是推翻它：

- 对**生成型/外源**变更，Issue + WorkItem 是合适的组织单位：有负责人、验收人、
  `expected_version`、时间线和通知。我的 `change_request` 应作为它与发布链之间的
  连接层，而不是另造一套工单。
- 对**反应型/内源**故障，WorkItem 不是合适的原子单位。每次探针抖动都开单会造成
  告警疲劳；正确单位是 incident（fingerprint 去重）+ action（有界处置）。
  只有“没有已知修复、需要改代码/改配置结构”时，才升级为 Issue/WorkItem。
- **两条链必须共用**候选、验证、激活、观察、验收、回退和审计；既有设计里升级请求
  只是“可选带 issue_id”，没有定义内源 incident 如何自动转成 change_request。这是
  我认为最大的流程遗漏：不是缺工单，而是**缺“什么时候不需要工单”的机制**。
- 既有设计默认“聊天平台消息不能触发升级、只有控制台管理员能发起”
  （[upgrade-chain §7.1](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L431-L441)）。
  我认为方向可以更细：授权应按 capability 而不是按入口一刀切；agent 可以发起
  “生成型”变更，但必须经过双钥匙（策略 + 独立审计者），且不得自批权限扩张。
  这属于策略分歧，不是安全底线分歧。

### 11.6 一句话总结对照

既有设计在**“怎么安全地切换一个已知产物”**上做得比我细；我的独立设计在
**“什么才算一个版本、什么才算成功、失败后降到什么程度、哪些东西必须同版锁定”**
上更严格。两者合并后的取舍应该是：

- 保留：请求/执行分离、`current.json` 单一提交点、journal 对账、store/copy、
  Windows pointer、75/78、失败冻结、能力声明、intent/result、issue 时间线；
- 改掉：文件作为执行真值、60 秒存活即验收、只允许一次整包回退、时间戳版本身份、
  “payload 完整”即验证、恢复内核随 payload 替换、GC 无 baseline；
- 补上：插件发布链、外部组件运行身份与 drift、配置/权限同版 pinning、验收证据、
  不可逆迁移三态、安全模式、权限双钥匙、内源 incident/action 模型。
