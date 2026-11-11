# 下游变更点（防止被后续更新忘掉）

这份文档只记 **已经定下、且容易被后来的人改回去** 的变更点。不是完整 changelog，也不替代 ADR：
每条写清「改了什么、为什么、落在哪、改回去会怎样、怎么验」。

维护规则：

1. 任何改变用户可见行为（配置语义、默认值、文件布局、协议字段）的提交，必须在这里加一行。
2. 条目只增不删；被新决定取代时标 `superseded` 并写新条目 id，不覆盖历史。
3. `状态` 只允许：`done`（已实现）/ `planned`（定了但没做）/ `superseded`。

| id | 变更点 | 状态 | 为什么 | 落点 | 改回去会怎样 | 验收 |
|---|---|---|---|---|---|---|
| D1 | 仓库不再提供 provider / 默认模型 / preset；`harnesses/dsh/dsh-mcp.patch.yml` 只留空占位 `[]` | done | provider 属于使用者自己的部署配置，仓库替用户决定会把所有人的模型绑死 | `harnesses/dsh/dsh-mcp.patch.yml`；`deploy/start-dsh.sh`、`deploy/start-drivers.sh` 读 `ACB_DSH_PATCH` | 用户换模型要改仓库；空 patch 契约被填回内容后又变成隐式默认 | 文件内容为 `[]`；`grep -rn "agent-default-model" harnesses/` 无命中 |
| D2 | 会话模型只来自 `[[sessions]] model` 或控制台，仓库不内置模型目录 | done | 同上；每个部署的 provider 目录不同 | `crates/kernel/src/config.rs` `Session.model`；驱动每轮 `selectModel` | 模型列表重新进仓库，换网关要改代码 | 新会话不填 model 时走 harness 默认；库里会话 model 可被控制台改 |
| D3 | 会话增删后动态重放 dsh presets（drivers 检测表指纹变化，调 `dsh_presets_acb.sh`） | done | preset 只在启动时生成，新增会话会报 `Unknown agent preset` | `deploy/presets/drivers/drivers_acb.sh`；dsh 靠 HMR 重组 | 新增/删除会话要重启 dsh 才生效 | 新增会话 ≤1 个对账周期内出现 `presets 已刷新`，驱动能起 |
| D4 | 库内密钥必须由 unit 提供 `ACB_KEY_FILE`，不能只写本地 drop-in | done | rebuild 会清掉未纳管的 drop-in；缺 key 时 bridge 以 78 退出且无 LKG 可退 | `nix-config` 的 `modules/nixos/base/custom/ai/acb/default.nix` Environment | 重启/重建后整条桥起不来 | `systemctl --user show acb-bridge -p Environment` 含 `ACB_KEY_FILE=`；`FragmentPath=/etc/...` |
| D5 | `acb-bridge` 挂 `default.target`（配合 linger 开机自启） | done | 用户级 unit 默认不随登录拉起，只能手工 `acbctl up all` | 同上模块的 `wantedBy` | 重启后桥不自动起，表现为「unit=inactive」 | `systemctl --user is-enabled acb-bridge` = `enabled` |
| D6 | 新建会话的第一条订阅自动创建 `[[sessions.subscriptions]]` 段 | done | 新会话没有该段，加订阅会 400 并要求用户改 TOML | `crates/bridge/src/config_patch.rs` `subscription_op` | 控制台「加订阅没反应」复发 | 对裸会话加订阅返回 200，配置里出现订阅段 |
| D7 | 投递格式改为「触发前后 N 条 + 完整未读区间 + 投递次数映射」 | done（2026-11-11） | 用户 2026-11-10 定稿；旧行为按 `delivery_limit` 连续投，长积压会一次塞 30 条正文 | `crates/proto/src/frame.rs`（v8 字段）、`crates/kernel/src/{config,plugin,engine,storage}.rs`、`harnesses/opencode/plugin/acb-wire.mjs`、`crates/bridge/src/console.*`；规格见 §8.2 | 回退成连续投递会让刚被 @ 的消息排到队尾，模型无法自己决定读哪些 | `delivery_plan.rs` / `turn_flow.rs` / `wire.test.mjs` 的 D7 用例全绿；`stuck_unread` 出现在 `/api/sessions` |
| D8 | 组件 `failed` 是锁存终态，跨 launcher 重启保留 | done（约定） | 避免坏组件无限重启；但失败原因修好后需要人工解除 | `state/components/<name>.json`；`acbctl` 无 reset 命令 | 修好配置也不会自愈，表现为「怎么重启都不起」 | 删除对应 json 或改配置 sha 后能重新拉起 |
| D9 | acbl 预设脚本来自 acbl 包，payload 换代不覆盖 | done（约定） | 组件表在 `supervisor.toml`，脚本在 `share/acb/presets` | 部署/升级流程 | 以为换代能更新脚本，结果跑的还是旧包 | `ps -o cmd` 里的脚本路径等于当前 acbl store 路径 |

## 相关

- 需求对齐批与执行顺序：`docs/plan/todo-ledger.md` 第 13 节（R1–R13）。
- 投递格式的完整规格：`docs/plan/config-rbac-plugin-platform-plan-v2.md` §8.2。
