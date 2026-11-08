# 自迭代架构对抗性评审

> 评审基线：仓库 `10a8ac9`（2026-11-07）。本文先读代码与 `docs/plan`、`docs/design`，再对“已定结论”逐条裁定。
> 判定口径：**成立**＝结论与实现/文档一致且可落地；**有保留**＝方向对但表述过强或缺少必需条件；
> **必须推翻**＝按当前表述实施会把系统带进错误状态，需换方案。
> 文中“未实现”指仓库里没有对应代码路径，不等于“设计上错误”；“已矛盾”指结论与代码或既有设计文档直接冲突。

## 0. 总评

这套架构**方向成立、细节不成立**。它不是一份可以照做的施工图，而是把三层不同成熟度的东西写成了同一等级的“已定结论”：

- 已经实现且正确：请求/执行分离的思想、store/copy 显式策略、75/78 编码、profile generation 布局、issue 的 append-only 事件、插件 capability 审计。
- 只存在于文档、代码零实现：升级请求信箱、`capabilities.json`、LKG 提升的成功判定、发布验收、审批、事故模型、secret store、`policy.db`。
- **与代码或既有设计直接矛盾**：`current.json` 与 `profile/current` 的“唯一提交点/投影”关系；`profile switch` 是否更新 `current.json`；LKG 的“连续健康 60 秒”；配置“业务对象全部入库”；插件 `external` 自管数据库；“一个 launcher 一个子进程”与四到五个进程的栈。

最要紧的不是补功能，而是先承认三件事：**当前没有升级请求的消费者**；**“generation”只覆盖一个二进制，不覆盖整个栈**；**审批在 issue 层可被伪造**。在这三件事解决前，其余结论都只是纸面自洽。

---

## 1. 逐条判定

### 1.1 第 1 条：分层（nix/acbl/systemd 各司其职，ACB 一个目录树、一个启动钩子）

**判定：有保留。** 职责分层本身成立，但“一个目录树、一个启动钩子”和“nix 可选”在当前交付里都不成立。

事实：

- `acbctl unit render` 生成 5 个 unit：`dsh-acb`、`acb-bridge`、`acb-drivers`、`acb-opencode-serve`、`acb-stack.target`，见 [unit.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/unit.rs#L24-L40)。**没有 launcher unit**。
- `acb-bridge.service` 直接 `ExecStart=@ACB_UNIT_BIN_DIR@/acb-bridge`，不是 acbl，见 [acb-bridge.service.tmpl](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/units/systemd/acb-bridge.service.tmpl#L10-L20)。
- nix 模块同样直接起 4 个服务，`ExecStart` 指向 `profile/current/bin/acb-bridge`，且 nix 包仍在 `systemPackages` 里。见 [default.nix](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/default.nix#L380-L410)。
- `acb-launcher.service` 只存在于 `deploy/systemd/` 的示例里，没有进入 `unit render` 的输出，也没有进入 nix 模块。

所以“系统里只留一个启动钩子”是**目标，不是现状**；现状是 5 个钩子，且 75/78 没有消费者。这一点直接决定第 3 条当前不可用。

替代与代价：要么把 `unit render` 的 bridge/drivers/opencode/dsh 全部改为“启动 acbl，acbl 读启动规格”，要么明确“只版本化 bridge，其余组件不参与 LKG”。前者改动 `unit.rs`、nix 模块、四份 unit 模板、`stack.sh`/status/logs 脚本；后者要把第 1 条降级为“bridge 的单进程版本化”，并放弃“整包回退”的叙事。两者都比现状诚实。

### 1.2 第 2 条：store/copy 显式可选；`current.json` 唯一提交点；`profile/current` 只是投影；`switch` 与 `current.json` 同一次原子操作

**判定：前半成立，后半必须推翻（措辞与实现都不成立）。**

事实：

- 显式策略确实实现了：`required_strategy` 缺失即报错，不自动判断，见 [profile.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/profile.rs#L399-L414)。
- `profile switch` **只改软链**，完全不写 `current.json`，见 [profile.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/profile.rs#L426-L443)。文档自己也写明“switch 只改 current 软链”，见 [acb-profile.md](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-profile.md#L89-L90)。
- 升级链文档已经承认“跨目录不能真正一次原子”，改用 journal + 启动对账，见 [acb-upgrade-chain.md](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/acb-upgrade-chain.md#L231-L265)。**所以“必须是同一次原子操作”这句话与自家详细设计冲突。**
- 回滚时 supervisor 只写 `current.json`，**不更新 `profile/current`**，见 [supervisor/lib.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/supervisor/src/lib.rs#L900-L950)。下一次 `acbctl run` 仍从 `profile/current` 解析 `bin_dir`/`repo_dir`，见 [run.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/run.rs#L221-L222)。
- 更糟的是读路径：`resolve_bin_dir`/`resolve_repo_dir` 读的是 `profile/current`，不是 `current.json`，见 [profile.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/profile.rs#L207-L239)。**代理“唯一提交点”的读端根本不读它。**

替代：把“提交”收敛为一个单写者操作（控制库事务，或带 `fence_token` 的 journal），`current.json` 与 `profile/current` 都只做投影；所有读端（`unit render`、`acbctl run`、`profile gc`、`profile verify`）统一读同一个提交点。若暂时保留文件，至少要加 `flock`、`fsync` 文件与目录、固定单写者。代价：改 `resolve_*`、`gc`、`verify`、`run`、`unit render`、nix 模块；迁移时先写一次 `current.json` 再重建投影，破坏所有直接读 `profile/current` 的外部脚本。

另外，store 策略还有一个被忽略的硬依赖：**没有 gcroot**。`profile add --strategy store` 只是把 `root` 软链到 `/nix/store/...`，见 [profile.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/profile.rs#L712-L720)。`nix-collect-garbage` 不认普通软链，LKG 指向的 store path 随时可能被回收。要么 `nix-store --add-root` 建 gcroot，要么 store 只作为“当天构建、当天验证”的临时模式，长期 LKG 必须 copy。

### 1.3 第 3 条：升级链（请求文件 → acbl 认领 → 校验/stage → 停旧 → 提交 → 75 → 重拉；60 秒提升 LKG；失败回滚；再失败 78；无重拉方拒绝升级）

**判定：方向成立，关键实现全部缺失，且“60 秒提升 LKG”实现错误。必须返工后再称“已定”。**

事实：

- 仓库里**没有任何升级请求的读写路径**：没有 mailbox 文件、没有 `acbctl upgrade`、supervisor 不监听请求。`acbctl` 的子命令里没有 upgrade。
- `acbctl run` 的父循环只在子进程返回 75 时重拉，最多 3 次/60 秒；launcher 自身崩溃（exit 1）不会被重拉，见 [run.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/run.rs#L120-L170)。所以“无 systemd 也成立”对 launcher 崩溃不成立。
- LKG 提升的判定不是“连续健康 60 秒”，而是“**首次健康后经过 60 秒**”：`healthy_since_ms.get_or_insert(now)` 只设一次，不健康时不清零；`maybe_promote` 用 `now - since` 判定，见 [supervisor/lib.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/supervisor/src/lib.rs#L599-L618) 与 [L725-L751](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/supervisor/src/lib.rs#L725-L751)。一个版本可以“先健康 1 秒、再连续不健康 5 分钟、再恢复 1 秒”就被晋级；崩溃重启后也不清零，第一次健康探针就可能直接晋级。这是假恢复的典型入口。
- 子进程主动退出 75（bridge 的 SIGHUP 排空路径）会被 supervisor 当作**失败**：没有 75 的特判，统一走 `record_failure` → 策略评估，可能触发回滚，见 [bridge/main.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/bridge/src/main.rs#L470-L540) 与 [supervisor/lib.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/supervisor/src/lib.rs#L570-L590)。75 在两层有相反语义。
- 停旧进程用 `child.kill()`（Unix 上是 SIGKILL），没有 SIGTERM 宽限，直接绕过 bridge 的 30 秒 drain，见 [supervisor/lib.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/supervisor/src/lib.rs#L259-L290)。这与“优雅停旧、回退事件不丢”冲突。
- “无重拉方拒绝升级”没有检测手段：launcher 无法可靠判断自己是否会被重新拉起（`NOTIFY_SOCKET`、父进程、systemd/SCM 都可能不在）。文档里的 `capabilities.json` 没有实现。

替代：先做最小可验证闭环（launcher unit + 请求信箱 + 端到端 75/78 测试），再谈策略。健康窗口改为“**连续健康**”：进程重启、任一不健康探针、任一失败决策都清零；LKG 晋级必须有验收证据，而不是计时器。停旧改为 SIGTERM → 超时 SIGKILL。无重拉方时写 `requires_restart` 并拒绝自动升级。代价：改 supervisor 状态机、launcher、run 循环、unit 模板；增加一个 mailbox 与 e2e 测试环境（这是必须付的，不付就没有第 3 条）。

### 1.4 第 4 条：自迭代五步（提议 → 施工 → 验证 → 上线 → 记账；不发明 semver）

**判定：有保留。流程骨架成立；“内容哈希记账”和“旁路起候选”在当前实现里不成立。**

事实：

- generation id 是 `gen-<unix_ms>-<8hex>`，由时间、源路径、配置 sha 拼接，见 [profile.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/profile.rs#L1527-L1550)（`allocate_id`）。同样的字节、同样的配置，两次构建得到两个“版本”，不能幂等去重，也不能跨机比较。copy 策略有逐文件 manifest，但元数据里没有整体内容摘要。
- `validate_payload` 只检查目录与文件存在性，见 [profile.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/profile.rs#L1299-L1330)，不做 schema、兼容性、权限 diff、烟测。
- “旁路起候选”与现有运行形态冲突：bridge 的端口、`state/acb.db` 的 SQLite 单写者、console 单实例，都要求候选使用**独立的临时 state + loopback 端口 + 假平台/假 harness**。当前没有这种候选模式，也没有把候选的配置 revision 钉进验证结果。
- 数据迁移：文档明确“插件回滚不负责数据回滚”，但没有“可逆/向后兼容/不可逆”三态声明；一旦候选执行了不可逆 schema 变更，回滚只是把代码退回去，数据库已经回不去。

替代：`artifact_set_digest`（字节）与 `activation_manifest_digest`（字节 + config revision + policy revision + 启动规格 + 迁移声明）分开；候选验证用临时 state/端口；迁移声明进 manifest，不可逆则禁止自动回退。代价：改 `profile add/meta`、加候选运行器、加 DB 备份/迁移闸门；破坏现有“generation id 即版本”的外部引用。

### 1.5 第 5 条：半自动，审批点未定（“人点一下”还是“agent 做、人只查账”）

**判定：必须推翻“未定”。这不是可以留到后期的策略选择，因为当前 issue 层已经能被绕过自批。**

事实：

- `Principal::actor_for` 对 principal id 为 `acb-bridge` 的 token 无条件接受 `on_behalf_of`，见 [issue-service/lib.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/issue-service/src/lib.rs#L600-L612)。任何拿到这个 token 的进程/agent 都能以任意 actor 写事件。
- `update_issue` 允许调用方直接设置 `verification_owner`（[L968-L970](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/issue-service/src/lib.rs#L968-L970)），`default_verifier` 又优先返回这个字段（[L742-L756](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/issue-service/src/lib.rs#L742-L756)）。因此执行者可以把自己设为验收人，再调用 verify。`can_verify` 只比较 `actor == default_verifier`，见 [L758-L766](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/issue-service/src/lib.rs#L758-L766)。
- 结论：**“人点一下”不能靠 issue 状态实现**。issue 是审计与协作层，不是授权层。

替代与代价见 §3.3。最少要修三处：`on_behalf_of` 改为“显式委托 grant”而非按 principal id 特判；`verification_owner` 只允许管理员/人类主体设置；审批记录绑定 `activation_manifest_digest` 并由 ACB 在提交前验签。代价：issue-service token 模型、升级请求 schema、控制台审批 UI、审计表；破坏现有“bridge 可代所有 actor 写”的便利。

### 1.6 第 6 条：issue 就是平台适配器（房间=单、消息=评论、状态=平台事件；参与关系推导；不写订阅表；谓词处理；`platform_call`）

**判定：作为“传输/呈现适配器”成立；作为“自迭代的组织单位”必须推翻。**

事实：

- 当前 issue 服务有 `subscriptions` 与 `event_deliveries` 的 outbox，见 [issue-service/lib.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/issue-service/src/lib.rs#L820-L845)。新文档说“不写订阅表”，但代码里已经有一套；迁移期会出现两套投递。
- ACB 的房间来自静态 `[[sessions.rooms]]`/订阅配置；issue 数量是动态的。把“每个 issue = 一个房间”意味着房间注册、房间级 retention、每个连接一个镜像房间。`issue-platform-adapter.md` 自己也写明“同一个 issue/<id> 在两条连接上各自成为房间”，见 [issue-platform-adapter.md](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/plan/issue-platform-adapter.md#L20-L45)。这会让同一条评论落两份记录，retention 与未读也按两份算。
- “参与关系推导”需要 `room_members`/`bindings`；当前只有静态 subscriptions 与 grants，没有可写的参与关系表。`@` 提及没有 mentions 实体，只能从正文解析；平台 display name → actor 的身份映射也不存在。
- 内源故障（探针失败、插件崩溃）走这条链会把“秒级恢复”变成“开单-分诊-领取-修复-验收”。
- `platform_call` 的“core 不解释 issue 语义”成立，方向正确；但给 `platform_call` 授权用的是房间可达性，一旦可见房间改为参与关系推导，grants 的判定点也要跟着改，不是“加两张表”那么小。

替代：保留 issue 作为**外源入口**与**呈现层**；内源走 incident + bounded action（见 §3.1）；参与关系只做“谁可见”，不做“谁被唤醒”的唯一来源；唤醒谓词由适配器算完后通过受控事件投递，core 只执行通用的 record/visibility。代价：新增 incident/action 模型、房间可见性新数据源、迁移 outbox；收益是恢复速度与责任边界。

### 1.7 第 7 条：修订制（append-only revisions 1..N；评论修改=新修订+系统事件；Azure 富信息）

**判定：作为审计模型成立；作为当前实现不成立，且与“版本身份”要分开。**

事实：

- 当前 `issues` 只有 `version` 乐观并发列，`issue_events` 只记事件，没有 revisions 表、没有 comments 实体、没有链接/附件/参与者实体，见 [issue-service/lib.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/issue-service/src/lib.rs#L340-L420)。
- 状态机是 `new/in_progress/pending_verification/done/closed`，不是 Azure 的 `new/active/resolved/closed/removed`，需要迁移映射（文档已提）。
- `issue_events` 有 actor 文本，但没有 actor 类型（人/agent/服务）、没有连接身份、没有签名；加上 §1.5 的伪造问题，“任意两修订可比”在审计上不可信。

替代：revisions 表（`issue_id, revision_no, content_hash, actor, actor_kind, created_at, source_event`）+ comments 表（`comment_id, issue_id, revision_no`），actor 从 token/委托 grant 派生而不是请求体；状态迁移由服务端策略校验。代价：schema 迁移、API 兼容、控制台/时间线改写；性能上几百张单、上千事件没问题（`issue_events` 已有 `(issue_id, created_at DESC)` 索引），但“每条评论一次全订阅扫描”的 outbox 要先改。

### 1.8 第 8 条：流水（升级/回退/自愈结构化事件；绑单进 issue，无主进系统流；保留策略；回退中被杀事件不能丢）

**判定：有保留。intent/result 与保留策略方向正确；“多流”会制造多事实源，SIGKILL 会丢事件。**

事实：

- 现状已经有至少四套记录：supervisor 的 `audit.jsonl`、`supervisor.log`、升级链文档设想的 `state/events/*` 与 `emergency.jsonl`、`policy.db` 的 `audit`、issue 的 `issue_events`。没有一套是“判定事实”的唯一来源。
- `audit()` 每条打开文件写入，没有 fsync；进程被 SIGKILL 时已 write 的数据通常还在页缓存，但 OS 崩溃会丢；更重要的是 `ProcessRunner::stop()` 用 SIGKILL，bridge 内存中的 in-flight 事件/未提交事务没有机会落盘，见 [supervisor/lib.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/supervisor/src/lib.rs#L281-L290)。
- 保留策略按房间“只削已读段”，见 [config.example.toml](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/config.example.toml#L80-L95)。镜像房间 + 长期不读的 issue 房间会无界增长；“无主事件进系统流”如果没有 retention，就是第二个无界流。

替代：控制库 `audit` 为唯一判定事实源；issue 时间线与系统流都是 outbox/投影；supervisor 的 JSONL 只做应急降级副本，带 fsync + rotation；停旧进程先 SIGTERM，再宽限后 SIGKILL，并把“即将停旧”作为 intent 先落盘。代价：控制库/audit 表、outbox、日志轮转；改 `ProcessRunner`。

### 1.9 第 9 条：配置单一源（连接/harness/会话/人格/权限/token 入库；`config.toml` 只留引导；token 加密、`ACB_KEY_FILE` 优先、密文带 kid、缺密钥启动失败）

**判定：方向成立，但**与既有设计直接冲突**，且密钥方案未实现、命名也不一致。**

事实：

- `acb-policy-store.md` 明确“**留文件**：连接拓扑、harness、人格、记忆、Lua 路由、端口、unit、supervisor、profile/generation”，并给了理由（拓扑/路由是部署与代码，变了要重启/重建），见 [acb-policy-store.md](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/design/acb-policy-store.md#L76-L129)。结论第 9 条却把 `连接、harness、人格` 全部入库，二者不能同时为真。
- 当前实现确实是 base TOML + `config.d` 合并，控制台写 `config.d/90-console.toml`，见 [config.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/kernel/src/config.rs#L1286-L1310) 与 [console.html](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/bridge/src/console.html#L858-L859)。`policy.db` 只有 `PolicySource` 抽象，没有实现。
- token 当前是 config.toml 明文；`ACB_KEY_FILE`/`ACB_KEY`/`kid` 在代码里 **grep 为零**。设计文档用的是 `token_ref|encrypted|plaintext` 与 `key_id`，见 [acb-policy-store.md](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/design/acb-policy-store.md#L1196-L1246)。结论第 9 条的命名（`kid`、`ACB_KEY`）与文档不一致，且“有密文但缺密钥则失败”没有覆盖 `plaintext` 模式。
- `ACB_KEY` 走环境变量还有一个安全问题：同用户可读 `/proc/<pid>/environ`，且会被子进程继承；`ACB_KEY_FILE` + systemd `LoadCredential` 更好。轮换只带 kid 不够，需要 key ring + 重加密或“旧 kid 只解密”。

替代：采用 policy-store 的“单一 active revision + importer + 冻结旧文件 + 运行期不合并”；连接拓扑/harness/人格留在文件但把摘要钉进 activation；token 一律 `token_ref`，密文进 secret store，明文 schema 层拒绝；密钥默认文件/凭据，环境变量只做开发逃生口。代价：一次性 importer（解析 base+config.d 生成 revision + diff）、控制台改写、token 迁移（明文→引用）、密钥轮换流程。破坏性：所有直接改 TOML 的运维习惯失效，必须走 revision API。

### 1.10 第 10 条：插件存储 `builtin`（ACB 库内命名空间）/`external`（插件自管连接、建库、迁移、降级；地址与密码写在插件配置段）

**判定：必须推翻 `external` 这一半。`builtin` 成立，但要用受限 StorageHandle，不是“按 id 命名空间”这么松。**

事实：

- policy-store 的结论是：默认同库分表 `plugin_<id>_*`，**只有 core 开 SQLite 句柄**，插件拿到的是受前缀约束的 StorageHandle，不能传路径、不能自开连接；`file` 模式也必须由 core 管理文件与迁移，见 [acb-policy-store.md](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/docs/design/acb-policy-store.md#L648-L735)。这与“external 插件自管连接/建库/迁移/降级”正面冲突。
- 让插件自管数据库，会立刻重演 policy-store 警告的那些问题：备份不完整、WAL/加密策略分叉、迁移不可回滚、卸载留垃圾、权限审计绕过。结论第 9 条又要求 token 入库加密，第 10 条却允许账号密码写在插件配置段，这两条也互相打架。
- 文档只允许把“真正需要独立进程/独立信任边界”的组件作为 C 层（如 issue 服务），且“不作为默认选项”。结论第 10 条把 C 层当成普通插件选项，扩大了攻击面。

替代：插件只拿 core 的 `StorageHandle`（表前缀强制、authorizer、单事务迁移、配额、导出/卸载清理）；确实需要独立库的，升级为**独立服务**（有自己的部署单元、凭据、备份、信任边界），不叫“插件存储”。代价：需要外部 DB 的插件要么改造，要么独立部署；短期兼容可用只读视图/导出。破坏性：已有 `external` 插件若存在，需要迁移或降级为独立服务。

### 1.11 第 11 条：nix-config 只是环境与交付层（dsh/opencode、目录持久化；不承载 ACB 业务配置）

**判定：成立（方向），但“交付层”当前是唯一入口，不能说 nix 可选。**

事实：nix 模块生成 4 个 unit、`config-merge.sh`、`up/down/status/logs/stack/acb` 脚本、`ExecStartPre profile seed`，见 [default.nix](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/default.nix#L380-L410)。它确实不写业务配置（`configFile` 是手写唯一真源），这点与第 11 条一致；但“nix 可选”要等 `unit render`/`acbctl run` 能独立完成安装、seed、拉起之后才成立。另一个问题见 §2.2：nix 的 `seed` 调用缺 `--strategy`，全新 profile 会直接失败。

---

## 2. 结构性攻击

### 2.1 “一个 launcher 只能监督一个子进程” vs 四到五个进程的栈

这是全套结论里最硬的结构限制，而结论第 1、3 条假装它不存在。

代码事实：`ProcessRunner` 只持有一个 `Child`（[L259-L266](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/supervisor/src/lib.rs#L259-L266)）；`supervisor.toml` 只有一组 `program/args`；`acbctl run` 只 spawn 一个子进程（[run.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/run.rs#L365-L384)）。而实际栈是 bridge + drivers + opencode-serve + dsh + issue-service。三个可行路径都不成立：

1. **一条 launcher 监督 bridge**：drivers/opencode/dsh 不参与版本化；一次“回滚”只回滚 bridge，harness 脚本仍是新一代。更糟的是 `ACB_UNIT_REPO_DIR` 被钉成 `profile/current`（软链路径，不是解析后的 generation 路径），见 [profile.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/profile.rs#L219-L239) 与 [run.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/run.rs#L374-L376)。**运行中的进程会在切换后跟随软链读到新代的 harness/Lua**，连“旧进程跑旧代码”都不成立。
2. **每个进程一条 launcher**：如果共用同一个 `state_dir`，每个 launcher 都在启动时 `save_current`（[launcher/main.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/launcher/src/main.rs#L51-L68)），并且 `save_current` 用同一个 `current.json.tmp` 临时名（[supervisor/lib.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/supervisor/src/lib.rs#L1079-L1090)），会互相覆盖；如果各自一个 `state_dir`，就没有跨进程原子切换，回滚各自为政，产生混合代。
3. **一个 conductor 监督 N 个子进程**：这是唯一正确方向，但要新的多进程监督器、组件注册表、每组件 LKG、健康/依赖顺序、启动预算。它直接推翻“一个 launcher 一个子进程”和“系统只留一个启动钩子”的现有表述。

结论：要么承认“只版本化 bridge”，把第 3 条的 LKG 语义缩小；要么把 launcher 升级为“稳定恢复内核 + 组件监督器”。中间态（一条 launcher + 整包 current.json）在代码上做不到，在语义上是假的。

### 2.2 崩溃与并发窗口

| 窗口 | 事实 | 后果 | 修法 |
|---|---|---|---|
| `save_current` 无 fsync、无目录 fsync | [L1079-L1090](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/supervisor/src/lib.rs#L1079-L1090) | 断电后 `current.json` 可能为空/旧；下次启动 `load_current` 返回 `None`，launcher 用配置值并**覆盖**指针 | 用 `write_atomic`（已有 fsync）+ 目录 fsync + 单写者 |
| `load_current` 损坏即静默回退 | [L1071-L1076](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/supervisor/src/lib.rs#L1071-L1076) | 与“损坏即 78、不猜”冲突；指针被静默重置到 `profile/current`（可能正是坏版本） | 区分“文件不存在”与“解析失败”；后者拒绝启动或进入安全模式 |
| 两个 launcher 并发 | 无锁，同一 `.tmp` 名 | 后写者赢，可能读到半写文件 | `flock`/租约；控制库单写者 |
| `profile switch` 与 `current.json` 分开 | §1.2 | 提交后崩溃 → 投影与真值分叉 | journal + 启动对账 + fence token |
| `profile gc` 与升级并发 | `gc` 只保护 keep/current/lkg，没有锁、不读 inflight，见 [L445-L500](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/profile.rs#L445-L500) | 刚 `add`、还没 `switch` 的目标被删；提交后启动失败 | gc 读同一提交点 + 保护 inflight + 加锁 |
| 健康窗口不重置 | [L599-L618](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/supervisor/src/lib.rs#L599-L618) | 不健康/重启后立即晋级 LKG | 连续健康计数；任一异常/重启清零 |
| 子进程 75 被当失败 | §1.3 | 主动重启触发回滚 | 区分“请求重启”与“崩溃”；75 重置稳定性计时，不记失败 |
| `ProcessRunner::stop` = SIGKILL | [L281-L290](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/supervisor/src/lib.rs#L281-L290) | 丢 in-flight 事件/事务 | SIGTERM + 宽限 + SIGKILL；先写 intent |
| 全新 profile 的 seed 必失败 | nix 调用 `profile seed <path>` 无 `--strategy`（[default.nix#L404](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/default.nix#L404)）；`required_strategy` 缺失即报错（[L399-L414](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/profile.rs#L399-L414)） | 装不上；profile 被清空（impermanence）后起不来 | 安装器显式传策略，或把策略写进 bootstrap 配置 |
| 初始 LKG 缺 `--config` | `config_identity(None)` 读 `ACB_UNIT_CONFIG_FILE`，nix seed 未设；`write_initial_lkg` 于是 `args=[]`，bridge 默认读 cwd 的 `config.toml`（[L548-L577](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/profile.rs#L548-L577)、[bridge main](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/bridge/src/main.rs#L757)） | 回退到初始 LKG 时找不到配置 → 78 | seed 必须传 config；LKG 必须记录启动规格 |
| `~/.local/share/acb` 不在持久化列表 | 本机 `preservation.nix` 只持久化 `~/.local/state/acb` | 重启后 profile/lkg/current 全丢，unit 指向不存在的 profile | 把 dataDir 纳入持久化，或全部重建 |
| 两个 `state_dir` 默认值 | `gc` 固定读 `<profile>/../state/supervisor`（[L1450-L1467](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/profile.rs#L1450-L1467)）；`acbctl run` 在 `ACB_UNIT_REPO_DIR` 未设时用相对 cwd 的 `state/supervisor`（[run.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/run.rs#L352-L357)） | 从 repo 根启动会写另一个 mailbox，gc 保护的是另一份 LKG | 提交点/信箱路径只允许一个来源，且绝对化 |

### 2.3 跨平台

**Windows 不是“把软链换成指针文件”就能成立**，而是三处重写：

- `profile::atomic_switch` 在非 Unix 平台直接报错（[L1470-L1505](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/profile.rs#L1470-L1505)）。copy 策略也依赖 `current` 软链，所以 **copy 在 Windows 同样不可用**；“copy 兜底”只兜住了快照，没兜住切换。
- bridge 直接使用 `libc::signal`、`SIGTERM`、`SIGHUP`（[main.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/bridge/src/main.rs#L55-L85)）；Windows 的 libc 没有 `SIGHUP`。drain/重启协议要改成跨平台控制通道（命名管道/HTTP + 控制台事件），否则编译与运行都不成立。
- launcher 的 `shutdown::requested()` 在 Windows 恒为 false；`pid_alive` 在非 Unix 恒为 false（[fsutil.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/acb/src/fsutil.rs#L53-L70)）；没有 Job Object，launcher 被终止不会带走子进程，也没有“子进程退出后重启”的可靠机制。Windows 服务包装器（SCM）也没有实现。
- 容器：75/78 只有在 entrypoint 显式消费时才有意义；只读根文件系统、PID 1 信号语义、volume 上的 SQLite（网络卷 WAL）都会破坏“默认只靠 SQLite+文件系统+进程控制”的假设。容器里“自迭代”必须缩到“更新 volume 中的指针/产物，然后由编排层重建容器”。

结论：独立设计说“默认路径只需要 SQLite + 文件系统 + 进程 API”是**方向对、工作量低估**。真正需要的是一个 `Platform` trait（信号/控制通道、Job Object/进程组、原子替换、指针、锁），Windows 实现是独立工程量。

### 2.4 权限与注入面

- **审批伪造**：见 §1.5。`on_behalf_of` + `verification_owner` + `can_verify` 三处叠加，等于“谁执行谁验收”。
- **升级请求注入**：结论第 3 条禁止请求里带 `command/args/env`（好），但 `probe`、`observe_window`、`rollback_script` 等策略参数若由请求携带，就是注入面。策略必须来自签名的 policy revision，请求只能引用 revision id。
- **`platform_call` 授权**：现在靠房间可达性；一旦可见房间改为“参与关系推导”，一次 issue 的创建者/被 @ 者可能自动获得对 issue 专有动作的调用权。权限扩张必须走 grants diff + 审批，不能随参与关系隐式扩张。
- **插件凭据**：第 10 条允许账号密码写在插件配置段，而第 9 条要求 token 加密入库；两者不一致。秘密必须只以引用形式给插件，日志脱敏，schema 拒绝明文。
- **密钥**：`ACB_KEY` 环境变量会被子进程继承、可被同用户读取；默认应为 `ACB_KEY_FILE`/systemd 凭据。轮换要有 key ring 与旧 kid 解密期。
- **审计可否认**：issue 的 `actor` 来自请求体/委托，不是签名身份；升级审计必须记录“谁批准了哪个 digest”，而不是“谁把状态改成 done”。

### 2.5 迁移期双数据源

会同时存在的双源，至少六处：

1. `config.toml` + `config.d/*.toml` + `90-console.toml` + （未来）`policy.db`；运行期合并。
2. `profile/current`（投影）与 `current.json`（所谓真值），以及 `supervisor.toml` 里再钉一次的 `program`/`generation`。
3. issue-service 的 `subscriptions/event_deliveries` outbox 与新的“参与关系推导投递”。
4. `acbctl` 的 store 包和 `profile/current/bin/acbctl`（两套入口，可能不同版本）。
5. nix 的 `services.acb` 与 `unit render` 生成的 unit（谁覆盖谁没有定义）。
6. 插件数据：`builtin` 表前缀与 `external` 自管库（如果保留第 10 条）。

迁移原则：每个逻辑实体只允许一个写者、一个读入口；旧的写路径在导入后**冻结并产生 drift 告警**，而不是继续参与仲裁。否则“单一源”只是文档口号。

### 2.6 体量与性能（几百张单、上千事件）

- **镜像房间**：一条评论在 proposer 连接和 handler 连接各成一个房间，记录 ×2；retention 按房间算，未读按会话算，读侧要合并两个房间。几百单 × 2 连接 = 上千房间，SQLite 仍可承受，但“每房间 5000 条”会把上限放大。
- **outbox**：`deliver_event` 对每个事件全表扫 `subscriptions` 并解析 filter，见 [issue-service/lib.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/issue-service/src/lib.rs#L820-L845)。几百订阅、低事件率没问题；一次批量导入/回放会造成 O(事件×订阅) 的尖峰，需要索引或改为按 issue 分组的投递。
- **保留策略**：只削“已读段”，长期不读的房间无界；镜像房间把这个问题放大。
- **gc**：`--keep 5` + copy 35MB/代，约 175MB，可接受；但 gc 扫描 generation 目录、读 lkg，O(代数)，并发无锁才是风险，不是磁盘。
- **audit**：1 探针/10 秒 ≈ 8640 行/天，无 rotation 时几个月几十 MB；不影响运行，但会拖慢排障。
- **候选验证**：临时 DB 复制是主要成本；如果 `state/acb.db` 已到几百 MB，每次候选复制不现实。应支持“空 DB + fixture 回放”，而不是复制生产库。

### 2.7 五步的可执行性

“提议 → 施工 → 验证 → 上线 → 记账”在单进程、单用户、低频场景成立；在“插件迭代为主”的场景缺三块：

- 没有候选隔离运行器（临时 state/端口/假平台）；
- 没有内容身份（时间戳 id 不能去重、不能跨机）；
- 没有迁移三态（不可逆迁移时禁止自动回退）。

这三块不补，“五步”只是流程名字。

---

## 3. 追加问题的结论

### 3.1 内源触发是否应完全绕开工单、只产事件？

**结论：绕开工单——是；只产事件——不成立。**

事件是“发生了什么”，事故是“系统处于什么状态”。没有状态对象就无法去重、聚合、指派、关闭、升级；靠事件流做这些，等于在日志上重建一个数据库。保留策略会先删掉“没人负责”的事件，复盘时只剩碎片。

最小模型（不是工单）：

- `incidents(incident_id, fingerprint, severity, trigger, state, first_seen, last_seen, count, owner, evidence_ref)`；
- `actions(action_id, incident_id, kind, params, started_at, result, evidence_ref)`，kind 是白名单（回退 LKG、禁用插件、重启、隔离候选、钉住已知好配置）；
- 未知 fingerprint：创建 incident + 通知值班，不自动执行；
- 有界动作连续失败 N 次或超预算：`escalate` → 才生成 issue/work order（此时才需要负责人、验收标准、`expected_version`）。

代价：新增两张表 + 指纹规则 + 动作注册表 + 通知；收益是“秒级恢复”与“可复盘”同时成立。若坚持“只产事件”，代价是永久告警疲劳与不可关闭的事故，评审建议驳回。

### 3.2 外源走工单、内源走短链，共用的尾声是否成立？

**结论：部署机制共用，验收/授权必须分叉。**“尾声共用”只对了一半。

共用：候选构建、manifest/闭包校验、pre-deploy 验证（烟测/密封集成/回放）、激活指针、journal、观察数据采集、回退执行、审计。这些必须只有一套，否则两套发布机制必然漂移。

分叉：

- **验收标准**：外源由需求方确认“问题解决了”；内源由证据确认“服务恢复且无回归”。不能共用一个 `accepted` 状态机。
- **观察窗口**：内源分钟级（恢复优先），外源小时/天级（需求方确认）。
- **授权**：内源“回退/禁用/重启”是收缩，可自动；外源“新行为/权限扩张”是扩张，需审批。
- **关闭语义**：incident 关闭 = 恢复 + 证据；work order 关闭 = 验收通过 + 需求方确认。

所以正确的表述是：**共用 deploy/evidence 原语，不共用 acceptance/closure 策略**。把 `RolloutPolicy`（风险下限、关键事件数、探针轮次、审批人、回退预算）挂在 `change_request` 上，而不是挂在 issue 或全局配置上。代价：多一个 policy 对象、两套关闭逻辑；收益是内源不被人类验收阻塞、外源不被自动关闭。

### 3.3 风险分级审批是否可行？线怎么划、谁判、会不会被绕过？

**结论：可行，但必须按“效果”划线、由 ACB 静态策略判、审批绑定内容摘要；否则会被绕过。**

| 级别 | 允许的变更 | 授权 | 观察 |
|---|---|---|---|
| T0 自动 | 单插件内部实现/配置；不新增 capability；存储只在自己前缀内；迁移可逆/向后兼容；回退目标健康 | 策略引擎放行 + 审计通知 | 10–30 分钟 + 关键事件 |
| T1 单人 | core/bridge；跨模块；新增 capability；数据迁移（可逆）；启动规格变化 | 人批准，绑定 `activation_manifest_digest` | 60 分钟 + 重启循环 |
| T2 双钥匙 | 权限扩张、secret/密钥、信任根、恢复内核自身 | 人 + 独立审计者（或两个 actor） | 24 小时异常观察 |

谁判：**不能是构建者自报**。由 ACB 的静态策略读 manifest：变更路径集合、capability diff、migration 三态、是否触及恢复内核/密钥/权限、插件作用域。构建者可以提供 provenance，但不能自证“我只改了插件”。

绕过分析（当前真实存在）：

- 自批：`verification_owner` 可写 + `can_verify` 默认执行者 → 已可绕过（§1.5）。
- 冒充：`on_behalf_of` 对 `acb-bridge` 无条件生效 → 可把“人批准”写成任意 actor。
- 换包：如果审批只绑定 issue id 或 generation label，批准后替换 payload 即可绕过。必须绑定 digest。
- 越界写：静态路径 diff 看不到运行时写 `/etc`、读别的插件数据。没有 OS 级沙箱/能力限制，T0 自动上线是**安全 theater**。

代价：manifest 签名/来源、审批 token 服务、`verification_owner` 与 `on_behalf_of` 收紧、进程级沙箱（Unix namespace/Windows Job Object + 受限文件系统）、审计表。没有沙箱就只能把 T0 缩到“纯声明式插件”，否则不要声称 T0 安全。

---

## 4. 与独立设计（`self-iteration-architecture-independent.md`）的正面比对

> 独立设计的核心主张按用户给的六条；以下裁决只针对**影响正确性**的分歧，不做“两边都对”的和稀泥。

### 4.1 先确认一致的部分

两边真正一致的：请求/执行分离；单一提交点 + 投影；journal + 启动对账；store/copy 显式；Windows 不依赖软链；75/78 是适配器编码；无 LKG 不自动升级；失败冷却/冻结；事件先 intent 后 result；配置/权限单一 revision；插件存储 core-owned；恢复优先于整包重装；agent 不得自验收/自关闭。

这些不需要再讨论，直接合并。

### 4.2 逐点裁决

| # | 分歧 | 裁决 | 理由 | 代价 |
|---|---|---|---|---|
| 1 | 版本身份：内容摘要 vs 时间戳 generation id | **独立设计赢，但要拆成两个身份** | 时间戳不是身份，无法去重/跨机。它的 manifest 若是每组件 scope，则“全局配置耦合”不成立；但把 artifact 与 config/policy 合成一个 id，会让“只改配置”也必须整包回退，且同一 artifact 无法对应多个 activation。应为 `artifact_digest` + `activation_digest`，config/policy 钉进后者 | manifest schema、profile add/meta、兼容 alias；旧 generation id 保留为本地别名 |
| 2 | 控制库唯一真相 vs `current.json` 唯一提交点 | **独立设计赢（控制库），但驳回“完全不要文件”** | 多文件状态机没有跨文件事务；但恢复内核在 DB 损坏/锁死/迁移失败时需要一条可启动的、签名/带摘要的缓存，否则连“回退执行者”都起不来。应是“控制库唯一 + 签名缓存作为灾备/引导” | 控制库、单写者租约、fence token、迁移 JSON 状态；改所有读端 |
| 3 | 验收：60 秒健康 vs 证据化窗口 | **独立设计赢，但驳回无界窗口与 inconclusive 即回退** | 60 秒只能证明 boot；但 `time_until(N_critical_events)` 无上限，低流量系统会永远不验收；`inconclusive` 自动回退会让安静系统无法采用任何 core 变更。应加 `W_max` 与“低置信度接受”显式标签 | 指标/证据管线、金丝雀/回放；合成流量或降低样本要求 |
| 4 | 恢复：整包回退 + 78 vs 组件阶梯 | **独立设计赢，但五级降为四级** | 插件是主迭代对象，组件级恢复必须保留；但“单插件”与“插件闭包”在单用户系统里可合并。保留：插件 → core → 安全模式 → 78（78 只在安全模式也起不来时） | 组件注册表、每组件 LKG、健康依赖；中等改造 |
| 5 | 恢复内核在 payload 之外 vs 一个目录树/一个启动钩子 | **独立设计赢** | 执行回退的进程和被回退的产物同代，是自替换死锁；Windows 运行中 exe 无法覆盖更让它致命 | 独立小内核 + 两阶段自升级；推翻“一个目录树/一个钩子” | 新增一个极小产物、独立更新流程、systemd/SCM 适配 |
| 6 | 默认只需 SQLite+FS+进程 API | **独立设计方向对，工作量低估；驳回“默认即可”** | 现有 bridge 是 Unix 信号/drain、profile 切换是 Unix 软链、Windows 无 Job Object/无 shutdown；这不是“能力端口”一句话，是平台层重写 | `Platform` trait + Windows SCM/Job Object/控制通道；大工程 |
| 7 | 不以工单为原子单位（外源 issue，内源 incident+action） | **独立设计赢，且是本次最重要的修正** | 内源强行开单会把秒级恢复变分钟级；但“只产事件”不可关闭，必须 incident+action | incident/action 表、指纹、动作注册表、通知 |
| 8 | 插件存储：core-owned StorageHandle vs `external` 自管 | **独立设计与 policy-store 赢；结论第 10 条必须推翻** | 自管连接会重演备份/迁移/加密/审计分叉；C 层独立服务不是普通插件 | 插件改造/独立部署；迁移外部数据 |
| 9 | 秘密：secret store + refs vs `ACB_KEY` 环境变量 | **独立设计赢；结论第 9 条需改** | 环境变量泄露面大；明文应 schema 拒绝；kid 轮换需要 key ring | secret store、token 迁移、轮换流程 |
| 10 | 配置：单一 active revision + 禁止运行期合并 vs “业务对象入库、config.toml 留引导” | **独立设计赢（方法），但配置范围按 policy-store** | “全部入库”与 policy-store §2.2 冲突；连接拓扑/harness/人格应留文件但摘要钉进 activation；运行期不合并 | importer + 冻结 + drift 告警；控制台改造 |
| 11 | `version_id = sha256(manifest)` 单一身份 | **改为两个 digest（修正，不是全盘驳回）** | 若 manifest 按组件 scope，它是对的；但“artifact 身份”和“这次激活用的配置/策略”语义不同。合并后无法回答“同一 artifact 换配置算不算新版”，也无法只回退配置；见 #1 | 两个 digest；manifest 引用 scope 化 revision |
| 12 | 观察窗口公式 | **驳回无上限版本** | 见 #3；必须 `W_max` + 低流量策略 | 策略表、证据等级 |
| 13 | 五级恢复阶梯 | **驳回，降为四级** | 见 #4；单用户系统里“单插件/插件闭包”可合并，状态爆炸不划算 | 组件注册表简化 |
| 14 | `effective_runtime_identity` 用“解析后的可执行文件真实路径 + 摘要” | **驳回路径参与哈希** | npm 临时目录/不同安装位置会造成假 drift；身份应由闭包内容哈希 + argv/env 摘要 + 包装层类型决定 | 解析闭包成本；可能需包管理器元数据 |
| 15 | “无重拉方拒绝升级” vs “journal 持久化 requires_restart” | **都对但取舍不同** | 两者不矛盾：没有 actuator 时拒绝**自动**升级，但把结论持久化为 `requires_restart` + 通知 + 安全模式 | 需要 capability 探测 + journal |
| 16 | nix 可选增强 vs 当前唯一交付 | **都对但取舍不同** | 设计目标应可选；现状是唯一入口。先解耦 `unit render`/seed 再谈可选 | 安装器重构 |
| 17 | append-only revisions vs manifest 事务 | **都对但取舍不同** | revision 是记录/审计，version 是内容身份；不要用一个表承担两种语义 | 两套表/引用关系 |
| 18 | 75/78 是核心协议 vs 适配器编码 | **都对但取舍不同** | 作为合同必须稳定；作为语义应只有“restart/give_up”两种意图 | 适配器映射表 |

### 4.3 独立设计自身的问题（不能照单全收）

1. **单一 `version_id` 的语义把 artifact 与 activation 混在一起**（#1/#11）：若 manifest 按组件 scope，就不存在“全局配置把所有人都变成新版”的问题；但同一 artifact 配不同 config/policy 仍然只能有一个 id，导致“只改配置”要整包回退、同一 artifact 不能有多个 activation 记录。拆成 `artifact_digest` + `activation_digest` 更干净。
2. **控制库单点**：它把 activation/LKG/journal/failures/audit 全部放进一个 SQLite。DB 损坏、迁移失败、网络卷 WAL、容器只读 volume 都会同时带走“服务”和“回退能力”。它定义了 safe mode 与 emergency，但**没定义启动时序**：DB 打不开时，恢复内核从哪份数据选 target、是否允许升级、emergency 是否可信。必须明确：DB 不可用 → 用签名缓存启动 → 禁止升级 → 安全模式。
3. **验收窗口无界**（#12）：`time_until(N_critical_events)` 在低流量系统可无限长；`inconclusive` 即回退会把“安静但健康”判成失败。它要求“流程被跳过则 unverified”，但没给 `W_max` 与“低置信度接受”路径。
4. **金丝雀/影子的成本被低估**：G4 用临时 state/端口/假依赖，成本可控；G6“只读镜像真实事件 + 第二实例”则需要事件镜像、第二 DB、端口与身份隔离。bridge 固定端口 + SQLite 单写者让 G6 很贵。建议 G6 只对高风险变更启用，默认 G4+G5。
5. **五级阶梯对单用户系统过细**（#13，方向不否定）：插件是主迭代对象，“插件 → core → 安全模式 → 78”已覆盖绝大多数场景；“插件闭包”可并入插件级。层级越多，每组件 LKG/activation 状态与 UI 越容易失配。
6. **`effective_runtime_identity` 路径哈希会产生假 drift**（#14）：npm 临时目录/不同安装位置会让同一个包显示为不同身份；应使用闭包内容摘要 + argv/env 摘要。
7. **“agent 不得自验收”只写在文档里**：它没有给出 token 层强制。当前 `on_behalf_of`/`verification_owner` 就是反例；不修代码，这条约束不存在。
8. **恢复内核“极小”但没有定义接口**：它要读 journal/控制库、决定回退、spawn 进程、发通知；接口一旦膨胀就不再“极小”。需要明确它只做“读 journal + 选 target + spawn/kill + 写结果”，不做构建/验证。
9. **两阶段自升级的失败面只答了一半**：它保留上一代内核，但旧内核 spawn 新内核后退出；新内核若启动即坏，谁触发“回到上一代”？需要外部 watchdog、双内核并行或启动计数，否则“两阶段”仍会死锁。
10. **“文件只做引导/导入/灾备”与“部署拓扑/端口/unit 留文件”有张力**：它用“摘要 pin 进 manifest + drift 事件”补上了，但必须定义 drift 的处置（文件改了但未重新激活 → 告警还是拒绝升级），否则又是双源。

### 4.4 最终合并建议

**应改共识（不是措辞，是语义）：**

- 第 2 条：删掉“同一次原子操作”，改为“控制库/单写者提交 + 投影 + fence”；或者保留文件但加 journal、锁、fsync、单写者。
- 第 3 条：把 LKG 从“命令 + 60 秒”改为“activation + 连续健康 + 验收证据”；把“一个 launcher 一个子进程”改为“稳定恢复内核 + 每组件启动规格”；补 journal/boot_count。
- 第 4 条：把“记账（内容哈希）”落到 `artifact_set_digest` + `activation_manifest_digest`；候选验证要有临时 state/端口。
- 第 5 条：不再“未定”；立即采用 T0/T1/T2 分级，审批绑定 digest，修 `on_behalf_of`/`verification_owner`。
- 第 6 条：保留“issue 作为平台适配器”，删掉“issue 是自迭代原子单位”；内源加 incident+action。
- 第 8 条：以控制库 audit 为唯一判定事实；issue/系统流是投影；停旧进程改 SIGTERM。
- 第 9 条：按 policy-store 的“留文件 + 摘要 pin”划范围；token 加密/密钥命名统一为 `token_ref`/`key_id`/`ACB_KEY_FILE`，并实现。
- 第 10 条：删掉 `external` 自管数据库；需要独立库的插件升级为独立服务。
- 第 1 条：把“一个目录树/一个钩子”改成“一个稳定恢复内核 + 每进程启动规格”；nix 可选要等安装器解耦。

**应驳回（无论谁说的）：**

- 独立设计把 artifact 与 activation 合成一个 `version_id`（拆成两个 digest）。
- 独立设计的无界观察窗口 + inconclusive 自动回退（加 `W_max` + 低置信度接受）。
- 独立设计的五级恢复阶梯（降为四级）。
- 独立设计的“默认只需 SQLite+FS+进程 API 就可跨 Windows”（需要平台层重写）。
- 独立设计的 runtime identity 路径哈希（改内容哈希）。
- 结论第 10 条的 `external` 插件自管数据库。
- 结论第 2 条的“switch 与 current.json 同一次原子操作”。
- 结论第 5 条的“审批未定”。

**都对但取舍不同：**

- nix/systemd 是增强还是当前唯一交付（目标 vs 现状）。
- 75/78 是核心协议还是适配器编码（合同 vs 语义）。
- revisions 与 manifest 事务（记录 vs 身份）。
- “无重拉方拒绝升级”与“journal + requires_restart”（拒绝自动升级，但持久化意图；不矛盾）。
- store 与 copy（store 需 gcroot，copy 需激活前 verify；两者都保留，默认按环境）。
- 一个控制库 vs 控制库/热库分离（分离是对的；不要把所有状态塞一个库）。

---

## 5. 先做什么（阻断项，按顺序）

1. **端到端 75/78**：launcher unit + 请求信箱 + `acbctl upgrade` + 一次真实升级/回滚/终态测试。没有这个，第 3 条不成立。
2. **修 LKG 判定与子进程 75**：连续健康、重启/不健康清零、75 不算失败、SIGTERM 宽限。
3. **消除双源**：选一个提交点（控制库或带锁 journal），让 `switch`、回滚、`unit render`、`gc`、`verify` 都读它；修 `load_current` 损坏静默回退。
4. **补安装可重复性**：seed 显式 strategy + config；初始 LKG 带启动规格；`~/.local/share/acb` 持久化；store gcroot 或默认 copy。
5. **收紧授权**：`on_behalf_of` 改显式委托；`verification_owner` 限管理员/人类；审批绑定 digest。
6. **再谈多进程**：明确“只版本化 bridge”还是“恢复内核 + 组件监督器”；不要在两可之间发版。

---

## 6. 五条最要紧的结论

1. **升级链当前没有消费者**：unit render 直接起 bridge，没有 launcher unit，也没有请求信箱/`acbctl upgrade`；75/78 从未端到端跑过。第 3 条是设计，不是现状。
2. **“整包 generation 回退”是假的**：launcher 只监督一个子进程，栈是 4–5 个进程；`ACB_UNIT_REPO_DIR` 还是 `profile/current` 软链，回滚后旧 bridge 会读到新一代 harness/Lua。要么只版本化 bridge，要么做恢复内核 + 组件监督器。
3. **`current.json` / `profile/current` 是双源，不是“真值 + 投影”**：`switch` 不写 `current.json`，回滚只写 `current.json` 不更新投影，读端读投影；`load_current` 损坏还静默回退并覆盖。必须单写者提交 + fence + 锁 + fsync。
4. **LKG 与 GC 不安全**：健康计时是“首次健康后 60 秒”，不健康/重启不清零；store 无 gcroot 会被 nix GC 删；gc 无锁、不保护 inflight；seed 缺 strategy 在全新 profile 必失败。这些会让“回退”在最需要时失效。
5. **审批在 issue 层可伪造**：`acb-bridge` token 可 `on_behalf_of` 任意 actor，`verification_owner` 任意写 token 可设，`can_verify` 默认执行者；所以“人点一下”必须落在 ACB 升级链内、绑定内容摘要、双钥匙，而不是 issue 状态。内源故障应走 incident + 有界动作，不强行开单。
