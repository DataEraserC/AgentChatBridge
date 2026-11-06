# nix-config → ACB 迁移盘点

> **时效**：2026-11-06。
> nix-config `main` 相对 `gitee/main` ahead 20，工作区干净，但**未 switch 到最新 generation**：当前 `/run/current-system` 为 `g8ix…`，HEAD 求值为 `h5d9…`，`acb-stack.target` 仍为 `not-found`。
> ACB 仓库 `dev_20261025` HEAD 为 `74e20b8`；nix-config `flake.lock` 里的 `acb` 仍锁 `c26dd2c`，落后于 ACB HEAD。
> ACB 工作区另有未跟踪的 `code-review-prompt.md`、`docs/plan/feedback-workitem.md`，与本文无关。

## 0. 结论

- **长期保留**：dsh 基线、opencode2、agenix secrets、issue service 及其 uid/preservation、部署拓扑/端口、flake input/lock（但语义降级为构建工具）。
- **过渡期保留**：`acbPkg`（当前 unit 的二进制来源）、`agentChatBridge` 的 out-of-store 源码根、`acbl` 兼容别名。它们只在 profile 落地前存在。
- **迁到 ACB**：四个 unit + `acb-stack.target`、六个 CLI 脚本、config merge、driver 启动脚本、dsh 的 ACB preset/bundle 注入、session 配置。
- **删除或归档**：ACB 仓库里的手工 deploy unit/脚本、opencode keeper、launcher/supervisor 脚手架（保留参考，不接生产）。

`acbPkg` 的终局：nix 最多做一次 **bootstrap 种子**，把首个版本 link 进 `~/.local/share/acb/profile`；之后运行版本归 ACB 自迭代，nix 不再钉死。**profile 落地是 `acbPkg` 退场的前置条件；没有 profile 就不要提前删。**

---

## 1. 长期保留（nix 仍该提供）

| 归属 | 路径 | 作用 | 动作 |
|---|---|---|---|
| 长期保留 | [flake.nix](file:///home/nixos/nix-config/flake.nix#L540-L545) / [flake.lock](file:///home/nixos/nix-config/flake.lock#L234-L245) | ACB 源与工具构建钉版；版本归 profile 后只用于构建 `acbctl`/launcher，不再决定运行版本 | **保留**，语义降级（见 §5） |
| 长期保留 | [acb/default.nix](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/default.nix#L29) 的 `opencode2` | 外部 opencode2 包 | **保留**；外部组件不自我迭代 |
| 长期保留 | [dsh/default.nix](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/dsh/default.nix#L967-L994) 的 `runtimePackage`/`profileTemplates`/`seedProfiles`，以及 `common.yml`/`web*.yml`/`*-failsafe.yml` | dsh 基线、受管 profile 快照来源 | **保留**；不要迁 ACB 的 preset/插件 |
| 长期保留 | [secrets/agenix/acb.env.age](file:///home/nixos/nix-config/secrets/agenix/acb.env.age) | ACB 静态密钥；键名见 §8 | **保留**（只列键名，不读值） |
| 长期保留 | [secrets/agenix/ntfy-acb-topic.env.age](file:///home/nixos/nix-config/secrets/agenix/ntfy-acb-topic.env.age) | 带外通知 topic；键名 `NTFY_ACB_TOPIC` | **保留** |
| 长期保留 | [secrets/secrets.nix](file:///home/nixos/nix-config/secrets/secrets.nix#L43-L46)、[nixos.nix](file:///home/nixos/nix-config/secrets/nixos.nix#L201-L213)、[nixos_agenix.nix](file:///home/nixos/nix-config/secrets/nixos_agenix.nix#L187-L198) | age 收件人/secret 声明 | **保留** |
| 长期保留 | `/run/agenix/acb.env`、`/run/agenix/ntfy-acb-topic.env` | agenix 运行时落地文件 | **保留**；ACB 只消费 |
| 长期保留 | [hosts/idols-ai/ai/acb-issue.nix](file:///home/nixos/nix-config/hosts/idols-ai/ai/acb-issue.nix#L21-L43) | 启用 issue service、静态账户、`ACB_ISSUE_URL` | **保留/改造**为只传 ACB module 参数 |
| 长期保留 | ACB 仓库 [deploy/nix/issue-service.nix](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/nix/issue-service.nix) | issue service 的 NixOS 模块定义 | **保留**（ACB 提供，nix import） |
| 长期保留 | [hosts/idols-ai/preservation-downstream.nix](file:///home/nixos/nix-config/hosts/idols-ai/preservation-downstream.nix#L99-L107) | `/var/lib/acb-issue` SQLite 目录持久化 | **保留** |
| 长期保留 | [hosts/service-user-ids.nix](file:///home/nixos/nix-config/hosts/service-user-ids.nix#L86-L91) | `acb-issue` 固定 uid/gid 950 | **保留** |
| 长期保留 | [hosts/idols-ai/ai/acb-notifications.nix](file:///home/nixos/nix-config/hosts/idols-ai/ai/acb-notifications.nix) | 把 `ntfy-acb-topic.env` 路径交给 `modules.acb.notifications` | **保留** |
| 长期保留 | [hosts/idols-ai/default.nix](file:///home/nixos/nix-config/hosts/idols-ai/default.nix#L137-L154) 的 `modules.acb` 部署输入 | enable/autoStart/host/exposeHost/ports/secretsFile | **保留/改造**；sessions 部分迁 ACB |
| 长期保留 | [vars/networking-downstream.nix](file:///home/nixos/nix-config/vars/networking-downstream.nix#L576-L597) 的 host/ports/autoStart | 部署拓扑与端口 | **保留**；sessions 部分迁 ACB |
| 长期保留 | [acb/default.nix](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/default.nix#L42-L83) 的 notifications 片段生成 | 从 topic secret 生成 `[notifications]` 片段 | **保留**；合并逻辑迁 ACB |
| 长期保留 | [acb-issue.nix](file:///home/nixos/nix-config/hosts/idols-ai/ai/acb-issue.nix#L39-L43) 的 `ACB_ISSUE_URL` | 告诉 bridge issue provider 地址 | **保留** |

---

## 2. 过渡期保留（profile 落地前）

| 归属 | 路径 | 作用 | 动作 |
|---|---|---|---|
| 过渡期保留 | [acb/default.nix](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/default.nix#L27-L28) 的 `acbPkg` | `acb-bridge` / issue service 的二进制来源；当前四个 unit 的 `ExecStart` 仍指向 store 路径 | **保留到 profile 落地**；之后删除，终局只留 bootstrap 种子 |
| 过渡期保留 | [outputs/x86_64-linux/src/idols-ai.nix](file:///home/nixos/nix-config/outputs/x86_64-linux/src/idols-ai.nix#L53) 的 `agentChatBridge` | out-of-store 可写源码根，供 `harnesses`/`lua` 链接 | **保留到 profile 落地**；之后删除或降级为显式 dev 选项 |
| 过渡期保留 | [acb/default.nix](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/default.nix#L198-L204) 的 `acbl` | `acb stack` 的兼容别名 | **保留**；ACB CLI 稳定后再决定是否只留 alias |
| 过渡期保留 | `acbPkg` 的 bootstrap 种子 | 首次把 ACB 版本 link 进 `~/.local/share/acb/profile` | **一次性**；之后版本归 ACB |

### `acbPkg` 为什么现在留、长期不能留

- **现在留**：四个 unit 的 `ExecStart` 都还是 nix store 路径：`dsh-acb` 用 `acb-dsh-exec` wrapper，`acb-bridge` 用 `acbPkg`，`acb-drivers` 用 `driverScript`，`acb-opencode-serve` 用 `${acbSrc}/deploy/opencode-serve.sh`。迁移期没有第二个二进制来源。
- **长期不能留**：`acbPkg` 就是「版本被 nix 钉死」的实现本体；ACB 想自迭代，不能每次改完都等 `nix flake update` + rebuild。留着不用又会变成死代码。
- **终局**：nix 最多给一次 bootstrap 种子，把首个版本 link 进 `~/.local/share/acb/profile`；profile 之后由 ACB 自己更新。**没有 profile，不要提前删 `acbPkg`。**

---

## 3. 迁到 ACB

| 归属 | 路径 | 作用 | 动作 |
|---|---|---|---|
| 迁到 ACB | [acb/default.nix](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/default.nix#L328-L461) 的四个 unit + `acb-stack.target` + `autoStart` | 生命周期、依赖图、重启契约、opt-in | **改造**：由 ACB 的 `deploy/nix`/`deploy/units` 生成；nix 只 import 并传参 |
| 迁到 ACB | [acb.sh](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/acb.sh) | 命令分发 | **迁移**到 ACB CLI；nix 删除副本 |
| 迁到 ACB | [stack.sh](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/stack.sh) | start target + 就绪等待 + 汇总 | **迁移** |
| 迁到 ACB | [up.sh](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/up.sh) | 启动 target 或单组件 | **迁移** |
| 迁到 ACB | [down.sh](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/down.sh) | 停 target 或单组件 | **迁移** |
| 迁到 ACB | [status.sh](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/status.sh) | 健康判定 | **迁移** |
| 迁到 ACB | [logs.sh](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/logs.sh) | 跟日志 | **迁移** |
| 迁到 ACB | [config-merge.sh](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/config-merge.sh) | 把 nix 生成的 `[notifications]` 合并成 `config.active.toml` | **迁移/改造**为 ACB 配置分层；nix 只给片段 |
| 迁到 ACB | [drivers.sh](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/drivers.sh) / [driver-session.sh](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/driver-session.sh) / `driverScript` | 按会话启动 dsh driver | **迁移/改造**；nix 只传 sessions/ports |
| 迁到 ACB | [acb/default.nix](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/default.nix#L90-L208) 的 `acbCli`/`acbStatus`/`acbStack`/`acbUp`/`acbDown`/`acbLogs`/`acbConfigMerge` | 把脚本打成 nix 包 | **改造/删除**：由 ACB 包提供；`acbl` 过渡保留 |
| 迁到 ACB | [dsh/default.nix](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/dsh/default.nix#L765-L768) 的 `acbBundle`/`webAcbBundles` | 把 ACB `dsh-bundle` 注入 dsh profile | **改造**：ACB 启动时自行安装 preset/插件 |
| 迁到 ACB | [dsh/default.nix](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/dsh/default.nix#L925-L937) 的 `acbSessions` | 每条 session 生成 agent preset + MCP client | **改造/迁移**：sessions 归 ACB |
| 迁到 ACB | [dsh/web-acb.yml](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/dsh/web-acb.yml) | ACB 专用 dsh profile 模板 | **迁移**：ACB 侧提供 profile |
| 迁到 ACB | [vars/networking-downstream.nix](file:///home/nixos/nix-config/vars/networking-downstream.nix#L598-L640) 的 `acb.sessions` | session id / preset / driver 端口 | **迁移**；nix 保留 host/ports/secrets |
| 迁到 ACB | [hosts/idols-ai/default.nix](file:///home/nixos/nix-config/hosts/idols-ai/default.nix#L154) 的 `modules.dsh.acbSessions` | 把 ACB sessions 接进 dsh preset | **改造/删除** |
| 迁到 ACB（已就绪） | ACB 仓库 [deploy/units/systemd](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/units/systemd) | ACB-managed unit 模板（含 target、75/78、StartLimit） | **保留**，作为迁移目标 |
| 迁到 ACB（已就绪） | ACB 仓库 [deploy/opencode-serve.sh](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/opencode-serve.sh) | opencode serve 启动 + `/api/info` + `/api/plugin` | **保留**；当前 nix unit 仍引用 store 副本 |

---

## 4. 删除或归档

| 归属 | 路径 | 作用 | 动作 |
|---|---|---|---|
| 删除或归档 | ACB 仓库 [deploy/systemd/dsh-acb.service](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/systemd/dsh-acb.service)、[acb-bridge.service](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/systemd/acb-bridge.service)、[acb-drivers.service](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/systemd/acb-drivers.service)、[acb-opencode-serve.service](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/systemd/acb-opencode-serve.service) | 旧手工部署 unit；已被 nix/ACB 模板取代 | **删除/归档** |
| 删除或归档 | ACB 仓库 [deploy/systemd/acb-issue-service.service](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/systemd/acb-issue-service.service) | 旧 issue service 手工 unit | **删除/归档** |
| 删除或归档 | ACB 仓库 [deploy/opencode-keeper.sh](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/opencode-keeper.sh) + [acb-opencode-keeper.service](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/systemd/acb-opencode-keeper.service) | 旧 keeper 方案，已被 `opencode-serve` 取代 | **删除/归档** |
| 删除或归档 | ACB 仓库 [deploy/systemd/acb-launcher.service](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/systemd/acb-launcher.service)、[acb-supervisor.service](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/systemd/acb-supervisor.service) | launcher/supervisor 双重监督脚手架，生产未用 | **归档参考**，不接生产（Q2 再定） |
| 删除或归档 | ACB 仓库 [deploy/supervisor.example.toml](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/supervisor.example.toml)、[deploy/run/supervisor.toml.tmpl](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/run/supervisor.toml.tmpl)、[deploy/verify-launcher.sh](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/verify-launcher.sh) | launcher 配置/验证 | **归档参考** |
| 删除或归档 | ACB 仓库 [deploy/acb-up.sh](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/acb-up.sh)、[acb-down.sh](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/acb-down.sh)、[start-dsh.sh](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/start-dsh.sh)、[start-drivers.sh](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/start-drivers.sh) | 旧手工一键启动/停止/驱动脚本 | **删除/归档**，由 ACB CLI 取代 |
| 删除或归档 | ACB 仓库 `deploy/acb.env`、`deploy/acb.env.example` | 旧环境文件模板 | **归档**；不读取/不列出值 |
| 删除或归档 | ACB 仓库 [deploy/nix/nixos.nix](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/nix/nixos.nix)、`home-manager.nix`、`wasm-host*.nix` | ACB 侧通用打包示例，未被当前 nix-config 使用 | **保留在 ACB 侧/归档**，不算 nix-config 装配物 |
| 删除或归档 | ACB 仓库 `deploy/snowluma-compose.yml`、`relay-examples.md` | 部署示例，与当前 ACB 栈无关 | **保留/归档** |

> `deploy/units/systemd/*.tmpl` **不是**遗留物；它是 ACB 侧新生成的 unit 模板，迁移时优先复用。

---

## 5. flake input 与 lock（单列）

**路径**：[flake.nix](file:///home/nixos/nix-config/flake.nix#L540-L545)、[flake.lock](file:///home/nixos/nix-config/flake.lock#L234-L245)。

**现状**：`acb` 输入是 `github:DataEraserC/AgentChatBridge?ref=dev_20261025`；lock 当前 rev `c26dd2c`，而 ACB `dev_20261025` HEAD 已是 `74e20b8`，锁落后。

**语义变化**：

- 迁移前：lock 的 `acb` rev **决定运行什么版本**（四个 unit 的二进制/store 脚本都来自它）。
- 版本归 `~/.local/share/acb/profile` 之后：lock 的 `acb` rev 只用于**构建 `acbctl`/launcher 这些工具本身**，不再决定运行版本。
- 工具仍需要来源，所以 flake input/lock 长期保留；但不能再被当作「部署版本开关」。
- `opencode2` 和 dsh 基线**不适用这条**：它们是外部组件，本不该自我迭代，留 nix 钉版是对的。

**动作**：保留；迁移完成后更新文档描述，不要在 profile 落地后继续用 `nix flake update acb` 当作升级 ACB 运行版本的手段。

---

## 6. 尚未处理的写死引用

以下两处仍在 `${acbSrc}`（flake store）里，迁移时若不处理，「入口写死」只做了一半：

| 位置 | 现状 | 影响 | 建议 |
|---|---|---|---|
| [acb/default.nix](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/default.nix#L32) `driver = "${acbSrc}/harnesses/dsh/src/driver.mjs"` | dsh driver 入口从 store 取 | 即使 `harnesses` 链接指向 worktree，`acb-drivers` 实际执行的 driver 仍是 store 版本 | 迁到 profile 内的 `driver.mjs`，或由 ACB CLI 解析路径 |
| [acb/default.nix](file:///home/nixos/nix-config/modules/nixos/base/custom/ai/acb/default.nix#L440) `ExecStart = "${acbSrc}/deploy/opencode-serve.sh"` | opencode 启动脚本从 store 取 | `opencode` 的插件初始化逻辑仍被 nix 钉死 | 迁到 profile 内的 `opencode-serve.sh`，或由 ACB CLI 解析 |

另外，`acbPkg` 本身也是 store 路径，已单列在「过渡期保留」；`dshBin`/`opencode2` 的 store 路径属于外部组件，长期保留。

---

## 7. 迁移顺序与前置条件

### 前置条件

1. **profile 落地**：`~/.local/share/acb/profile` 内要有 ACB 自己维护的 `acb-bridge`、`driver.mjs`、`opencode-serve.sh`、dsh preset/bundle 等入口。
2. **secrets 不变**：`/run/agenix/acb.env`、`/run/agenix/ntfy-acb-topic.env` 继续由 nix/agenix 提供。
3. **外部组件不变**：dsh 基线、opencode2 继续由 nix 提供。

### 顺序

1. **用户脚本**：先把 `acb.sh`/`stack.sh`/`up.sh`/`down.sh`/`status.sh`/`logs.sh` 迁到 ACB CLI；`acbl` 保留为 shim。
2. **`render --print/--diff`**：ACB CLI 增加渲染命令，输出四个 unit + target 的 unit 文本；`--print` 只打印，`--diff` 与当前 nix unit 对比，不安装。
3. **遮蔽式安装**：把生成 unit 写到 `~/.config/systemd/user/`（优先于 `/etc/systemd/user` 与 nix store），`systemctl --user daemon-reload`，然后核对：
   ```bash
   systemctl --user show acb-bridge.service -p FragmentPath
   # 期望指向 ~/.config/systemd/user/...，不是 /etc/systemd/user 或 /nix/store
   ```
   对 target 和四个 unit 都核对一遍。
4. **切窗口**：停 `acb-stack.target`，确认无活跃 turn，再启动新 unit/target；保留旧 nix generation 作为回滚。
5. **删 nix 侧 unit**：确认新 unit 连续运行、日志/状态正常后，删除 nix-config 里的四个 unit + target 定义；保留 nix 侧材料。

### 回滚

- 删除或移走 `~/.config/systemd/user/` 下的遮蔽 unit，`systemctl --user daemon-reload`，回到 nix 生成的 unit。
- 只要 profile 还没落地，**不要删 `acbPkg`**；它是最后的二进制来源。

---

## 8. 当前状态与 secrets 键名

### 当前状态

```text
nix-config: main ahead 20; working tree clean
current generation: g8ix…（不是 HEAD 的 h5d9…）
acb-stack.target: not-found
ACB: dev_20261025 HEAD 74e20b8
nix-config flake.lock acb rev: c26dd2c
```

### secrets 键名（只列键名，不写值）

`acb.env`：

- `NEW_API_Y9000P2025_OPENAI_RESPONSES_API_KEY`
- `ACB_MODEL_KEY`
- `ACB_CONSOLE_TOKEN`
- `ACB_SNOWLUMA_TOKEN`
- `ACB_OPENCODE_PASSWORD`

`ntfy-acb-topic.env`：

- `NTFY_ACB_TOPIC`

说明：issue service 注释里提到 `ACB_ISSUE_TOKEN`，但当前 `acb.env` 实际键列表里没有它；补键时只补键名，不在本文记录值。`ACB_MODEL_DSH` / `ACB_MODEL_OPENCODE` 是配置里的非密钥可选占位，不属于 secrets。
