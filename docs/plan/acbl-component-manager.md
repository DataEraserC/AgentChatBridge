# acbl 定位升级：从升级执行器到组件管理者

状态：2026-11-08 设计稿（第九轮：`supervisor.toml` 所有权定稿 + 探针契约核对）。**只写文档，不改代码、不重启、不提交。**

结构前提：

- **acbl 独立安装，不进 payload。** payload 只装会迭代的内容：`acb-bridge`、drivers、插件、harness 适配层。
- **NixOS 上 acbl 由 nix-config 提供与维护**，随 `nixos-rebuild switch` 更新，属于低频变更。nix-config 里保留 acbl 不是技术债，是设计；但**只保留 acbl 这一个包**，不声明任何 ACB 单元（§2.4.1）。
- 因此**升级链只切 payload、只重启 bridge；acbl 全程不动**。
- 代价：acbl 与 payload 之间的接口（命令行参数、状态目录约定、健康探测方式、启动/停止契约、环境变量）必须**版本化**，当作稳定契约维护（§6.4）。

目标：把 acbl 从“bridge 的升级执行器”升级为 ACB 现场的**唯一常驻编排者**：

- acbl 负责拉起 `acb-bridge`（bridge 本身归 acbl 管）；
- 外部组件（dsh、opencode serve 等）的**启动脚本**由 ACB 提供预设，acbl 来维护，但**可选、可配置**；用户也可以完全不选预设、自己写；
- 外部组件的**安装**不由 ACB 承诺，用户按自己的包管理器装，ACB 只给参考组合；
- 谁能启动 acbl 本身有三类并列方式，由安装器显式选择（§4.3），不做环境探测。

与 `docs/guide/deployment-matrix.md` 的定位一致：acbl 是唯一入口，systemd 是可选启停适配器，nix 是可选产物来源；**payload 与 acbl 是两个独立分发单元**。

---

## 0. 结论摘要

1. **结构变了：acbl 与 payload 分开分发。** payload 只有 `bin/acb-bridge`、`harnesses/`、drivers 扇出脚本与插件；acbl（`acb-launcher` / `acbctl` / `acb-supervisor` / 预设 / unit 模板）单独安装，NixOS 上由 nix-config 提供**且只提供这一个包**，不声明任何 ACB 单元（§2.4 / §2.4.1）。
2. **升级不重启 acbl。** acbl 在原进程里完成：停旧 bridge → 提交 `current.json` → 按新指针在同一进程树里起新 bridge。旧模型里的 `restarting` 状态和“退出 75 等外部重拉”一起退役。
3. **75 收窄为兼容保留值，78 保留。** 75 不再由升级路径发出；78 仍表示“配置/指针/journal 不可恢复，放弃自动恢复”。acbl 自身崩溃由 init 重拉，不用退出码表达（§4.2）。
4. **`restart_owner` 取消作为升级门禁，`self` 档取消。** 升级只要求“有一个活着的 acbl 在消费信箱”，不要求“有人会重拉 acbl”。`capabilities.json` 里的同类字段降级为 `managed_by` 诊断信息（§4.3）。
5. **组件管理者不变，但耦合解开。** acbl 仍管 bridge（核心组件）与可选预设组件（dsh / drivers / opencode）；bridge 重启只跟 payload/升级链走，不与 acbl 自身版本绑定。**acbl 自身升级只重启 acbl**：bridge 继续服务，由新 acbl 接管（adopt，§4.7）；预设组件没有独立身份，仍随 acbl 重启。
6. **契约版本化是新的硬要求（B-12 已实现）。** payload 根目录用 `acb-contract.json` 声明 `contract` / `min_acbl` / `layout` / `commands`，acbl 只支持 `1..=1`；不匹配就拒绝切换、保留当前版本（§6.4）。
7. **presets 是“怎么起”，不是“装什么”，且按安装方式拆分。** 预设放在 `deploy/presets/<component>/<component>_<source>.sh`（如 `dsh_nix` / `dsh_npm` / `opencode_nix`），由清单 `presets.toml` 登记成“带开关的服务列表”；同一组件只能启用一个来源。预设**随 acbl 安装分发**，不随 payload；组件本体（dsh / opencode / node）仍由用户安装（§5）。
8. **谁启动 acbl：三类并列，由安装器显式选择**——用户终端手工、systemd 开机自启、systemd 只守护不自启；无 systemd 时由用户自管。区别只在于“acbl 崩溃/78 后由谁重拉、日志归谁、有无 cgroup”（§4.3），不做环境探测。
9. **组件表不做自动热加载**，只提供控制台“重新加载组件表”按钮；失败保留旧表，不允许停一半（§4.6）。
10. **acbl 是观察者 + 恢复者，不是 bridge 的所有者（目标态，尚未实现）。** 契约目标是 acbl 停止、崩溃、升级期间 bridge 继续运行；新 acbl 启动先探测身份再接管，禁止盲目再起一个（§4.7）。**当前实测仍是 owner 模型：launcher 退出会连带停 bridge，升级走 75 → systemd 重拉**；adopt 的落地差距见 §4.8。组件侧同理，按 §5.5 分两步。

---

## 1. 现状核实

### 1.1 launcher 到底具备什么

`crates/launcher/src/main.rs` 证明 launcher 是一个常驻进程，而不是一次性的升级执行器：

| 能力 | 现状 | 证据 |
|---|---|---|
| 常驻主循环 | **有**，100ms tick，直到 supervisor 停止 | `crates/launcher/src/main.rs:157-174` |
| 读取运行版本 | 读 `state_dir/current.json`，缺省时写入配置版本 | `main.rs:82-107` |
| 启动对账 | 用 `ActivationWriter` 的 journal 对账，损坏不猜、退出 78 | `main.rs:66-81` |
| 监督子进程 | **有，单个**：`ProcessRunner` 持 `Option<Child>`，spawn/kill/wait | `crates/supervisor/src/lib.rs:266-319` |
| 健康探针 | **单个** `health_url`，默认读 `$XDG_RUNTIME_DIR/acb-health.token` 做 Bearer 鉴权；没有时用 `AlwaysHealthy` | `supervisor.toml.tmpl:5`；`lib.rs:375-476` |
| 退避重启 | 全局策略：`restart_backoff_secs = [1,2,4,8,30]` | `supervisor.toml.tmpl:13-27` |
| LKG / 回滚 | **有**，但只针对当前这一个 `CommandSpec` | `lib.rs:751-770, 906-975` |
| 升级请求 | 认领、校验、stage、停旧、提交、退出 75（**旧模型**） | `main.rs:288-386` |
| 退出码契约 | 提交后 75；放弃 78；正常停止 0（**旧模型**，新结构见 §4.2） | `main.rs:389-392, 408-436` |
| sd_notify / watchdog | 有；无 `NOTIFY_SOCKET` 时为空操作 | `main.rs:489-525` |
| 多组件 | **没有** | `CommandSpec` 只有单个 `program/args`（`lib.rs:96-122`） |
| 进程组管理 | **没有**：`stop()` 只 kill 直接子进程 | `lib.rs:289-299` |
| 每组件环境 / 日志 / 健康 / 重启策略 | **都没有**；子进程继承 launcher 环境，stdout/stderr 直接继承 | `lib.rs:273-287` |
| 组件状态可见性 | 只有 `run.json` / `supervisor.log`，没有按组件的状态文件 | `run.rs:400-422` |

### 1.2 supervisor 管的是谁

`deploy/run/supervisor.toml.tmpl` 只有一个 `[supervisor]`：

```toml
program = "@ACB_UNIT_BIN_DIR@/acb-bridge"
args = ["--config", "@ACB_UNIT_CONFIG_FILE@"]
health_url = "http://127.0.0.1:@ACB_UNIT_CONSOLE_PORT@/healthz"
```

`health_url` 指向 console 的 `/healthz` 时必须带鉴权：探针自动读 bridge 写出的
`$XDG_RUNTIME_DIR/acb-health.token`（无该变量时退回临时目录，与 bridge 写入一致），
不必把 token 写进 supervisor 配置或 unit。

unit 保持 `KillMode=process`：systemd 只给 launcher 发信号，launcher 在
SIGTERM/SIGINT 收尾时自己 `stop_child` 停桥，避免 `systemctl stop` 或回退到
旧 unit 时留下占端口的孤儿 bridge。launcher 被 SIGKILL（崩溃）时子进程才会
留下，由下一任 acbl 通过 `bridge.instance.json` 接管。

所以 supervisor 管的是**桥**，且只有桥。dsh / drivers / opencode 目前由 systemd 各自的 unit 拉（`deploy/units/systemd/*.service.tmpl`），launcher 完全不认识它们。

### 1.3 75/78 的真实语义

| 码 | 触发 | 含义 | 谁处理 |
|---|---|---|---|
| 75 | `supervisor.request_exit(EXIT_ROLLBACK)`（`main.rs:391`）；或 supervisor 判定回滚（`lib.rs:965`） | **指针已提交 / 已回滚**，请求外部重拉 launcher，让它按新 `current.json` 起 | systemd `RestartForceExitStatus=75`；`acbctl run` 父循环读到 75 重拉（`run.rs:131-162`） |
| 78 | 配置/指针/journal 不可恢复；或回滚后的版本再失败 | **放弃自动恢复**，不要再拉 | systemd `RestartPreventExitStatus=78`；`acbctl run` 直接退出 78（`run.rs:163-166`） |
| 0 | SIGTERM/SIGINT；无 `NOTIFY_SOCKET` 时先停子进程 | 正常停止 | 外部 init |

关键点：75 不是“升级成功”的同义词，而是“**我需要被重拉**”。这是**旧模型**的语义：升级必须换掉正在执行升级的那个进程，才需要外部重拉。新结构下 acbl 独立于 payload，升级不再换 acbl，75 因此退役（§4.2）；78 保留。

### 1.4 无 systemd 时谁在监管

`acbctl run --mode=launcher` 是**父循环**：spawn `acb-launcher`（`run.rs:278-296`），等待退出码，75 重拉（有 60s/3 次限流）、78 终止、SIGTERM 转发并等待 45s（`run.rs:131-174, 474-495`）。它自己不是组件管理者，只是“重拉 launcher 的人”。在新模型里它只剩这一件事：acbl 崩溃或被人重启时拉起；升级不再经过它。

### 1.5 现状结论

**launcher 已经是一个常驻的、单组件的进程监管者**，并且升级链、LKG、75/78 都围绕这一个组件成立。它缺的不是“常驻”，而是**组件表、进程组、每组件策略和预设装载**。

另一个已经落地的结构点（**A-8 已实现**）：unit 的 `ExecStart` 不再从 payload 的 `profile/current/bin` 派生，而是指向**静态安装的 acbl**（`ACB_UNIT_ACBL_PREFIX` 默认 `/usr/local` → `$prefix/bin/acb-launcher`；落在 profile/generations 内的显式路径会被拒绝）。`acbctl unit install` 只渲染 `share/acb/units/` 里的模板。

---

## 2. 目标模型

### 2.1 角色

```text
  安装来源：nix-config（NixOS）/ nix profile / cargo / 预编译 tar.gz（gnu 或 musl）
                    │  静态入口（不在 payload 里，不随换代改变）
                    ▼
            ┌───────────────┐   读 current.json，启动该代的 bridge
            │     acbl      │──────────────────────────────┐
            └───────┬───────┘                              │
        ┌───────────┼───────────────┬────────────────┐     ▼
        ▼           ▼               ▼                ▼   payload generation
   acb-bridge    dsh（预设）    drivers（预设）   opencode（预设）
   核心组件       可选            可选              可选
   ↑ 来自 payload generation    ↑ 用户安装，ACB 只给预设脚本
```

acbl 是唯一的常驻者；bridge 与预设组件都在它的进程树里。**acbl 不在 payload 里**，所以换代不会换掉监管者。

### 2.2 组件分类

| 类别 | 谁提供产物 | 谁定义怎么起 | 升级语义 |
|---|---|---|---|
| **acbl 自身** | ACB 包，独立安装（NixOS 由 nix-config，且只提供包） | 运行时由 acbl 生成 unit（§4.9）/ init / 手工启动 | 低频：系统 switch 或替换二进制后重启 acbl |
| **核心组件** `bridge` | payload generation（ACB 构建） | ACB（supervisor 核心） | 有：LKG、回滚、升级链 |
| **预设组件** `dsh` / `drivers` / `opencode` | dsh / opencode 由用户安装；drivers 的实现来自 payload | ACB 提供预设清单与启动脚本；用户可启用、覆盖、禁用 | 无：不参与 LKG，不触发升级链 |
| **自定义组件** | 用户 | 用户写命令与声明 | 无 |

### 2.3 责任边界

- **acbl 负责**：拉起、健康探针、重启/退避、停止、状态落盘、日志归属、组件间启动顺序。
- **payload 负责**：提供 `bin/acb-bridge`、harness 适配层、drivers 扇出脚本与插件，并声明契约版本（§6.4）。
- **ACB 升级链负责**：payload 的版本切换、LKG、升级信箱、回滚。
- **用户负责**：外部组件的安装、升级、配置内容、密钥（组件自己的 env 文件）。
- **acbl 不做**：安装外部组件、解析它们的配置文件、替它们做版本回滚、调用 `nix` / `npm`。**`systemctl` 是唯一例外**：只有 `acbctl unit install/remove` 这个显式运维动作可以 `daemon-reload`（§4.9）；正常监督与升级路径绝不调用 systemctl。

### 2.4 acbl 的安装与分发（静态入口）

**静态入口 = init/unit 永远指向一个稳定路径，不指向 generation。** 这样 payload 换代时，unit 里的 `ExecStart=` 不需要改，监管者也不会被替换。

| 场景 | 来源 | 入口路径 | 更新方式 |
|---|---|---|---|
| **NixOS** | 本仓库 flake 的 `acbl` package（nix-config 以 input 引用，§2.5.7）；**目标态 unit 由 acbl 运行时生成（§4.9）**，nix-config 不声明 ACB 单元（§2.4.1） | `/run/current-system/sw/bin/acb-launcher`（稳定入口，不写 store 路径） | `nixos-rebuild switch`；随系统升级，低频 |
| 非 NixOS + 有 nix | 本仓库 flake / `nix profile` | `~/.nix-profile/bin/acb-launcher` | `nix profile upgrade` |
| 非 NixOS + 无 nix | cargo 构建 | `cargo build --release` 后复制到 `/usr/local/bin/acb-launcher` | 人工替换 + 重启 acbl |
| 非 NixOS + 无 nix | 预编译 `tar.gz`（gnu 或 musl，C-9） | 解包到 `/opt/acbl`；入口是 `/opt/acbl/bin/acb-launcher` | 解包新 tar.gz + 重启 acbl（规格见 §2.5） |
| Windows | 暂不支持 | — | C-5 |

分发单元（一个 acbl 包）包含：`acb-launcher`、`acb-supervisor`、`acbctl`、预设清单与脚本（`presets/`）、systemd unit 模板。unit 模板已通过 `include_str!` 编进 `acbctl`（`crates/acb/src/unit.rs:24-48`），因此它们天然属于 acbl 包，不属于 payload。

payload 只包含会迭代的内容：

```text
<generation>/root/
  bin/acb-bridge
  harnesses/dsh/src/driver.mjs
  harnesses/dsh/dsh-mcp.patch.yml
  harnesses/opencode/plugin/index.js
  harnesses/opencode/plugin/acb-wire.mjs
  harnesses/shared/tools.mjs
  deploy/start-drivers.sh        # 纯扇出执行器，见 §5.10
  acb-contract.json             # 契约声明（§6.4，B-12 已实现）
```

**为什么不把 acbl 放进 payload：**

1. 监管者的生命周期必须比它监管的对象长；每次换代都换监管者，就必然要重启监管者，升级链会退化成“重启整栈”。
2. acbl 是低频变更的稳定件，payload 是高频迭代件；两者混在一个 generation 里，只能按高频的那个一起动。
3. 回滚 payload 时不应该把监管者也回滚到旧版本——旧 acbl 可能不认识新的状态目录/契约，反而制造故障。

**迁移提示（从现有部署切到新结构）**：先安装 acbl，把 unit 的 `ACB_UNIT_LAUNCHER_BIN` 指到静态入口，再从 payload 里移除 acbl 二进制。过渡期可以保留一代“什么都带”的 payload，但 unit 必须已经指向外部 acbl。

#### 2.4.1 nix-config 的职责边界（硬约束）

**nix-config 只提供 `acbl` 这一个包，以及它自身需要的、与 ACB 无关的系统配置；不得声明任何 ACB 相关 service / target / timer。**

| 项 | 现状（2026-11-08） | 目标态 |
|---|---|---|
| nix-config 提供 | `acbl` package + 与 ACB 无关的系统配置 | 不变，且**仅此一项** |
| ACB 相关单元 | 声明了 `acb-bridge.service`、`acb-stack.target`、`dsh-acb`、`acb-drivers`、`acb-opencode-serve` 共五个定义 | **全部移除**（操作顺序见 §5.8） |
| unit 来源 | nix-config 模块生成 | **acbl 运行时生成**（§4.9），落盘位置由 `--scope` 决定 |
| 是否 enable | 随 `nixos-rebuild switch` 生效 | 安装只落盘，**默认不 enable**（launcher 不自启，§4.3） |

- **“不写死”与“不自启”是两个维度**：来源（谁生成 unit）归 acbl；时机（是否 enable）由部署者决定。因此允许“安装但不启用”——`acbctl unit install` 落盘后保持 disabled，部署者需要时显式 `systemctl --user start acb-launcher`。
- **组件不生成独立 unit**：dsh / drivers / opencode / 侧车都是 acbl 的子进程（§5），systemd 只看见 acbl 一个服务。
- 例外仅限“ACB 无关的系统配置”（user lingering、cgroup 配额、网络、防火墙等）；任何引用 ACB 二进制、状态目录、端口或组件名的单元定义都属于越界。
- 目标态与现状之间靠 §5.8 的三段迁移连接；在此之前，nix-config 里的五个定义仍是**现状**，不是契约的一部分。

#### 2.4.2 `supervisor.toml` 所有权：归 acbl（方案 A，定稿）

**定论：`supervisor.toml` 归 acbl，位置 `state/supervisor/supervisor.toml`，acbl 是唯一写者。** nix-config 只保留 `acbl` 包与临时 unit，**不再渲染 supervisor 配置**。

| 项 | 迁移期（现状） | 目标态 |
|---|---|---|
| 配置文件位置 | nix store 里的只读渲染产物 | `state/supervisor/supervisor.toml`（acbl 写） |
| unit 的 `--config` | 真实生效路径 | **降级为过渡兼容**，仅用于读取与比对；目标态不传 |
| 组件开关写回 | 做不到（store 只读） | acbl 直接写自己的配置文件 |

**过渡期语义必须写死**：外部只读配置下组件开关无法写回，`component enable` / `component disable`（以及控制台的等价写操作）**必须报错或明确拒绝**，不能假装成功。拒绝时给非 0 退出码与明确原因（如 `config_read_only`），并打印当前配置文件的来源路径。

**派生规则（替代原来由 nix 传常量）**：

| 字段 | 推导来源 |
|---|---|
| `program` / `args` | 优先读 `current.json`；无记录时按 profile 当前代推导 `bin/acb-bridge` + `--config <generation config>` |
| `health_url` | 从 bridge 配置的 `[console] listen` 推导 host:port，再拼 `/healthz` |
| `health_token_file` | 沿用自动路径规则（`$XDG_RUNTIME_DIR/acb-health.token`，见 §1.2） |
| `state_dir` | 按 XDG 推导（与现有 `run.rs` 一致） |

**推导结果必须在日志 / journal 里写明来源**（`current.json` / profile 当前代 / bridge `[console].listen` / 默认 XDG 路径），禁止静默：推导错误是“监管者启动错对象”，没有对照物就查不出来。

**首次切换的等价性要求**：acbl 生成的配置必须与迁移前的外部配置**语义等价**。实现阶段必须拿现场那份 store 文件逐字段比对（`program` / `args` / `health_url` / `state_dir` / policy），提供一次性 dry-run 输出 diff；不允许“看起来一样”就切。

**迁移顺序（与 §5.8「先搬后删」同构）**：

1. acbl 先能自持有配置并等价运行（此阶段允许 `--config` 存在，用于比对，不用于生效）；
2. 再删 nix-config 的 supervisor 渲染；
3. 最后删 unit 里的 `--config` 传参。

**风险**：先删传参再验证等价，等于在**启动入口**上做无对照实验——推导一旦写错，故障表现是“监管者起不来”，而此时已经没有旧配置可回退比对。

**P1 已实现的事实**（写入契约以固定现状）：`[[component]]` 全字段解析、`deny_unknown_fields`、默认值（探针 `5s × 5`、`restart` 默认 `on-failure`、`max_attempts = 5`、退避 `1/2/4/8/30`）、`acbctl component list/status` 只读、空表零回归。

**未决项（显式保留，不在此裁定）**：依赖目标目前**同时允许 `bridge` 与 `stack`**（§5.4 把 bridge 当隐式节点、stack 当伪目标），是否收窄为只允许 `stack` 尚未决定；收窄会改变 `acb-drivers` / `acb-opencode-serve` 的现有映射写法（§5.4 / §5.9）。

### 2.5 安装包规格（C-9）

**状态：nix 包已实现（§2.5.7）；非 nix 的 `tar.gz` 规格已出，实现未开始。** 分发分两条路，不能混用：

- **nix 环境**：本仓库 flake 产出 `acbl` 与 `acb-payload` 两个 package，nix-config 以 flake input 引用，**不产出 `tar.gz`**（§2.5.7）。
- **非 nix 环境**：只产出 `tar.gz`，没有 deb / rpm / apk / Windows 包；服务注册由安装脚本或 `acbctl` 完成，不再有包管理器 `postinst` 兜底。

两条路共用同一份 workspace 构建，但工件形态与安装方式不同：nix 包由 store 管理生命周期，`tar.gz` 由安装脚本管理。

#### 2.5.1 制品清单

非 nix 环境只出两个 `tar.gz` 制品，文件名必须带 target triple，避免用户下错：

| 制品 | 链接方式 | 面向 |
|---|---|---|
| `acbl-x86_64-unknown-linux-gnu.tar.gz` | acbl 动态链接 glibc，**钉死最低基线 2.28**（RHEL 8 / Debian 10 起）；更保守可用 2.17 | 主流 glibc 发行版（Debian/Ubuntu/Fedora/RHEL/openSUSE 的非 Nix 路径） |
| `acbl-x86_64-unknown-linux-musl.tar.gz` | acbl **全静态**（`+crt-static`） | Alpine 等 musl 发行版；也作为“任意 Linux 解压即用”的兜底 |

- **架构默认 x86_64；aarch64 是否同期产出：待定。** 构建矩阵、签名与校验和都按 target triple 分开，不能一个包通吃。
- **不再考虑 deb / rpm / apk**：它们的主要价值是包管理器登记与 `postinst`，而我们明确不要包管理器碰状态目录（§2.5.5）。
- Windows 不在范围内（C-5）。

#### 2.5.2 链接方式：两种制品 + 一条硬约束

**gnu 制品**：最低 glibc 基线 **2.28**（RHEL 8 / Debian 10）。基线不是写在 README 就算数——**构建环境必须真正老于基线**：用 `cargo-zigbuild` 钉 `--target x86_64-unknown-linux-gnu.2.28`，或在 Debian 10 / RHEL 8 容器里构建。在新系统上构建再声称兼容 2.28 是无效的；CI 用 `objdump -T` 检查未解析符号与 GLIBC 版本标记。

**musl 制品**：acbl 自身全静态，运行期不依赖目标机任何库。acbl 的依赖面很小（`serde` / `toml` / `sha2` / `libc`；`acb-kernel` 的 `git2` 已 `default-features = false`，`rusqlite` 用 `bundled`），构建期需要 C 编译器，运行期不需要系统库。

**关键约束：bridge 在 musl 上不能做成全静态。** bridge 的插件体系是 `.so` + `dlopen`，而静态 musl 二进制不支持运行期动态加载；因此 Alpine 形态必须是 **“静态 acbl + 动态链接 musl 的 bridge”**：

| 产物 | 目标 | 链接 |
|---|---|---|
| acbl（launcher / supervisor / acbctl） | `x86_64-unknown-linux-musl` | 静态（`+crt-static`） |
| payload 里的 `bin/acb-bridge` | `x86_64-unknown-linux-musl` | **动态 musl（显式 `-C target-feature=-crt-static`）**，运行期需要目标机 `libc.so` |

这条同时约束 payload 的构建目标：**acbl 与 payload 必须按同一 libc 家族配对交付**（gnu 对 gnu、musl 对 musl）；混用会在 `dlopen` 或符号解析时失败。发布说明必须写清“acbl 制品 ↔ payload 构建目标”的配对表。

#### 2.5.3 目录布局与安装步骤

tar.gz 可重定位，默认用户级不写 `/usr`、不建系统用户；`--scope system` 时可选创建 `acb` 系统用户与空的 `/var/lib/acb`：

```text
acbl-x86_64-unknown-linux-{gnu,musl}/
  bin/acb-launcher  bin/acb-supervisor  bin/acbctl
  share/acb/units/          # unit 模板（供 acbctl unit install 渲染）
  share/acb/presets/        # 预设清单与脚本
  install.sh
  SHA256SUMS
  README.md  LICENSE
```

推荐安装到 `/opt/acbl`（系统级）或 `~/.local/opt/acbl`（用户级）：解包到目标父目录后，归档里的 `acbl-x86_64-unknown-linux-<triple>/` 即为前缀。

```sh
# 系统级
sha256sum -c SHA256SUMS
sudo tar -xzf acbl-x86_64-unknown-linux-musl.tar.gz -C /opt
sudo /opt/acbl/install.sh --prefix /opt/acbl --scope system --with-install-section 0

# 用户级：把 -C /opt 换成 ~/.local/opt，--scope system 换成 user
```

`install.sh` 只做四件事：

1. 校验前缀与目标架构（gnu / musl 选错时给出明确提示，不静默继续）；
2. 把 `bin/` 与 `share/acb/` 放到位；
3. 调 `acbctl unit install --scope user|system --with-install-section 0|1 --root <unit-dir>`，由 `acbctl` **渲染** `share/acb/units/` 里的模板，而不是直接复制模板；
4. 打印后续步骤（配置、密钥、`profile seed`、启动命令），**不执行它们**。

- **unit 文件从哪来**：模板随 tar.gz 分发在 `share/acb/units/`，最终 unit 由 `acbctl unit install` 渲染。没有包管理器，就没有 `postinst`；`install.sh` 与手工执行 `acbctl unit install` 等价。
- **是否 enable / start**：`acbctl unit install` 只渲染并落盘 unit，**不执行 `systemctl enable` / `daemon-reload`**（保持 installer 不触碰服务状态）。`--with-install-section 1` 决定是否渲染 `[Install]` 段；enable 由部署者显式执行：
  - user scope：`systemctl --user daemon-reload && systemctl --user enable --now acb-launcher-user.service`
  - system scope：`sudo systemctl daemon-reload && sudo systemctl enable --now acb-launcher.service`
  默认（不传或 `0`）只写文件，不 enable、不 start。三种启动方式的选择权仍归部署者（§4.3）。
- **unit 必须是 `KillMode=process`**（§4.7.1），否则 `systemctl stop` 会连带杀掉 bridge。
- **A-8 实现说明**：`--with-install-section` 取代原先名不副实的 `--autostart`；`--scope user|system` 决定 stack target 的 `WantedBy`。升级 acbl 同样不重启服务。

#### 2.5.4 升级与卸载：状态目录永不删除

**升级**：用新 tar.gz 覆盖 `bin/` 与 `share/acb/`，再 `daemon-reload`。默认**不重启 acbl**，需要 `--restart` 才执行。理由同上：重启会换掉正在观察 bridge 的进程，新 acbl 若与当前 payload 契约不兼容会退出 78。升级 tar.gz ≠ 重启服务。acbl 重启时 bridge 继续运行，由新 acbl adopt（§4.7）。

**卸载**：`install.sh uninstall` 删除 `bin/`、`share/acb/` 与它渲染出的 unit 文件；**保留**：

| 保留项 | 位置 | 为什么 |
|---|---|---|
| 配置 | `/etc/acb/` 或 `${XDG_CONFIG_HOME}/acb/` | 重装后可直接复用 |
| 状态根 | `/var/lib/acb/` 或 `${XDG_DATA_HOME}/acb/` | `current.json`、`lkg.json`、journal 决定运行版本与回滚目标 |
| profile / LKG | 状态根下的 `profile/` 与 `state/supervisor/` | **不得删除**：删掉等于毁掉 LKG 语义与历史；重装后 acbl 读 `current.json` 恢复到卸载前的版本 |

`install.sh` **没有**删除状态目录的开关；README 可以写手工 `rm -rf` 的后果，但不提供自动化路径。没有 `postrm`，也就没有 `purge` 语义——卸载永远是非破坏性的。

#### 2.5.5 硬边界：安装只装 acbl

**显式约束（不可协商）：acbl 安装 / 升级 / 卸载只处理 acbl 自身；不得创建、修改、迁移、删除 payload 与 profile / LKG 状态目录中的任何内容。**

- 不得执行 `acbctl profile seed` / `add` / `switch` / `rollback`；
- 不得创建或修改 `profile/`、`profile/current`、`current.json`、`lkg.json`、`supervisor.toml`、`upgrade/`、`db/`；
- 不得删除状态根（卸载也不行）；
- 唯一允许的写入是：创建空前缀与共享目录；`--scope system` 时可选创建 `acb` 系统用户、空的 `/var/lib/acb` 并设属主 `acb:acb` 0750（仅空目录，不含任何状态文件）。

理由：升级链的正确性建立在“指针只有一个写者（acbl）”与“LKG 只由 supervisor 提升”上。安装脚本一旦写这些文件，就会在 acbl 不知情的情况下切换或删除版本，journal 对账会把安装动作误判成崩溃恢复，回滚目标与 LKG 语义同时被污染。**安装器是 acbl 的分发者，不是 acbl 状态的参与者。**

#### 2.5.6 依赖与交付顺序

```text
A-8（unit 入口指向静态 acbl） → B-12（契约 v1 + payload 校验收窄） → C-9（打包实现与发布）
```

- **A-8 已实现**；B-12（契约 v1 + payload 校验收窄）已合入；nix 包（§2.5.7）已实现。
- 剩余 C-9 工作：非 nix 的 `tar.gz` 构建矩阵（gnu 基线、musl 动态 bridge）与 §2.5.5 的边界评审。
- 发布物必须附：版本号、target triple、链接方式、最低 glibc 基线、支持的 `contract` 区间、SHA256 校验和。

#### 2.5.7 nix 包（NixOS 主路径）

nix 环境不走 `tar.gz`。本仓库 flake 直接产出两个 package，nix-config 以 flake input 引用：

| package | 内容 | 服务谁 |
|---|---|---|
| `acbl` | `bin/acb-launcher`、`bin/acb-supervisor`、`bin/acbctl`、`share/acb/units/`（unit 模板 + `acb-launcher.service` 静态样例） | 静态 acbl 入口，独立于 payload |
| `acb-payload`（别名 `payload`） | `bin/acb-bridge`、`harnesses/`、`deploy/start-drivers.sh`、`acb-contract.json` | `acbctl profile seed/add` 的 payload |

`acbl` 严格只含 acbl 侧内容；bridge / harnesses / drivers 一律不进 `acbl`，它们属于 `acb-payload`。

```sh
nix build .#acbl
nix build .#acb-payload
```

nix-config 引用方式（input 形式，取 `acbl` package）：

```nix
{
  inputs.acb.url = "github:<owner>/<repo>/<ref>";

  # 在模块里：
  # environment.systemPackages = [ inputs.acb.packages.${pkgs.system}.acbl ];
}
```

- **unit 的 `ExecStart` 必须写 `/run/current-system/sw/bin/acb-launcher`，不写 `/nix/store/...`。** NixOS 上 `/usr/local` 不可写，`acbctl unit install` 的默认前缀（`/usr/local/bin`）只适用于普通 Linux；NixOS 的稳定路径由 `environment.systemPackages` 暴露。**目标态 unit 由 acbl 运行时生成（§4.9）**；过渡期（§5.8 阶段 A）仍可能由 nix-config 生成，阶段 B 删除（§2.4.1）。
- **为什么不写 store 路径**：store 路径带 hash，acbl 每次升级都会变；把它写进 unit，等于每次升级都要改 unit 并重启服务，违背 §2.4 的静态入口约定。稳定路径让 unit 与 acbl 版本解耦。
- `acbl` 里的 `acb-launcher.service` 是普通 Linux 静态样例（`/usr/local/bin`），NixOS 不直接使用；NixOS 目标态同样由 `acbctl unit install` 渲染生成（§4.9），**不再从 nix-config 模块产出**，且仍必须是 `KillMode=process`（§4.7.1）。
- `acb-payload` 的口径与 B-12 的 `validate_payload` 一致，`acb-contract.json` 随包产出在 payload 根，不能再依赖 nix-config 手工补齐。

**两条路各自服务谁：**

| 路径 | 面向 | 生命周期 |
|---|---|---|
| nix 包（本节） | NixOS；以及已有 nix 的非 NixOS 主机（flake input / `nix profile`） | 由 store 与系统/ profile 管理，随 `switch` / `upgrade` 更新 |
| `tar.gz`（§2.5.1–2.5.3） | 没有 nix 的 Debian / Ubuntu / Fedora / RHEL / Alpine 等 | 由 `install.sh` 解包覆盖，人工控制更新 |

同一份源码共用 workspace 构建，但两条路的产物形态与安装方式不同，不能把一个环境的工件搬到另一个环境使用。

**nix 构建不跑测试的取舍（`doCheck = false`）**：nix 包用 release profile 构建，而 workspace 里多个 crate 的 dev-dependency 以 `features = ["test-fixtures"]` 引用 `acb-kernel`；release 下 `cargo check/test` 会把这个 feature 打开，命中 `crates/kernel/src/config.rs` 的守卫 `#[cfg(all(feature = "test-fixtures", not(debug_assertions)))]`。集成测试链接的是 lib，`cfg(test)` 对它不生效，只能以「release + feature」编译 lib，于是触发 `compile_error!`。因此 nix 构建只负责产出二进制，测试由本地/ CI 的 `cargo test`（debug profile）承担。

> **待办（切换后的结构性改动，本次不实现）**：把 fixture 入口迁到独立的测试支持 crate，让生产 crate 永不依赖 `test-fixtures`。之后该守卫才能只针对「生产构建误开 feature」这一种情况，而不是被正常的集成测试触发。

---

## 3. 差距清单

| # | 差距 | 现状证据 | 要改哪层 | 影响 |
|---|---|---|---|---|
| G1 | 只能管一个子进程 | `CommandSpec` 单 `program`；`ProcessRunner` 单 `Child`（`lib.rs:96-122, 266-319`） | supervisor + launcher（代码） | 全组合 |
| G2 | 没有组件表 / 预设装载 | `supervisor.toml` 只有 `[supervisor]`（`supervisor.toml.tmpl`） | launcher（代码 + 配置 schema） | 全组合 |
| G3 | 停止只 kill 直接子进程，脚本的后台孙进程会成孤儿 | `lib.rs:293` 只 `child.kill()`；`opencode-serve.sh:37-38` 里 `serve &` | supervisor（代码，进程组/setsid） | 全组合，尤其无 systemd |
| G4 | 只有桥有一个健康探针 | `health_url` 单值（`supervisor.toml.tmpl:5`） | supervisor + launcher（代码） | 全组合 |
| G5 | 没有每组件 env / cwd | 子进程继承 launcher 环境（`lib.rs:273-287`）；脚本自己 source env（`start-dsh.sh:25-32`、`opencode-serve.sh:34`） | supervisor + 预设 schema | 全组合 |
| G6 | 重启策略是全局的 | `[supervisor.policy]` 一组参数（`supervisor.toml.tmpl:13-27`） | supervisor（代码） | 全组合 |
| G7 | 没有组件状态与按组件日志 | 只有 `run.json`（`run.rs:400-422`）和 `supervisor.log` | launcher + `acbctl status`（代码） | 全组合 |
| G8 | 没有启动顺序 / 依赖 | 无 | launcher（代码） | 有依赖的组合 |
| G9 | 升级只覆盖 bridge | LKG 是单个 `CommandSpec`（`lib.rs:751-770`） | **保持**，但要写清边界 | 全组合 |
| G10 | 预设脚本里仍有 nix 默认路径 | `opencode-serve.sh:16,25`；`start-drivers.sh:17` | 预设脚本（默认值/参数化） | 非 NixOS |
| G11 | 启动 acbl 的默认方式未定 | 文档/部署层 | 安装器参数（§4.3 三种并列） | 全组合 |
| G12 | 组件表只在启动时读一次 | launcher 启动即 `load_config`（`main.rs:35-41`） | launcher（按 §4.6 做按钮热加载，**不做自动**） | 全组合 |
| G13 | ~~unit 默认把 launcher 指向 `profile/current/bin`~~ **已实现（A-8）** | `ACB_UNIT_ACBL_PREFIX`（默认 `/usr/local`）→ `$prefix/bin/acb-launcher`；显式 `ACB_UNIT_LAUNCHER_BIN` 落在 profile/generation 内时报错（`unit.rs`） | `acbctl unit`（已完成） | 全组合 |
| G14 | payload 校验仍要求 acbl 侧的文件 | 原 `validate_payload` 要求 `deploy/units/systemd`、`deploy/run/supervisor.toml.tmpl`、`deploy/start-dsh.sh`、`deploy/opencode-serve.sh`（`profile.rs:1599-1611`）；**B-12 已把清单收窄为 payload 自有的 9 项**（§6.4） | `profile`（已实现，分支 `feat/b12-contract`） | 全组合 |
| G15 | 没有契约版本声明与校验 | 原 payload 无契约文件；三个校验点都不比较版本。**B-12 已实现 payload 根 `acb-contract.json` + 原因码 + 三处校验**（§6.4） | `profile` + `launcher`（已实现，分支 `feat/b12-contract`） | 全组合 |

G9 不是缺陷：**外部组件不参与 LKG 回滚是刻意设计**。桥是 ACB 自己的产物，外部组件是用户的产物，把两者绑进同一个回滚单元会让“桥回滚”变成“用户的 dsh 也被换掉”，责任不清。

---

## 4. 进程模型建议

### 4.1 常驻者与进程树

- **acbl 与 bridge 都是常驻进程，但所有权不同。** acbl（`acb-launcher`）是观察者 + 恢复者；bridge 的生命周期独立于 acbl（§4.7）。acbl 不退化成“只切指针就退出”的执行器。
- **bridge 不在 acbl 的进程树里**：用独立 session（Unix `setsid` / Windows 独立 Job）启动；acbl 停止或崩溃时 bridge 继续服务，新 acbl 启动后先探测身份再 adopt。
- 预设组件（dsh / drivers / opencode）仍由 acbl 启动并放在它的进程树里；acbl 重启时它们随之重启，不具备独立身份。
- bridge：acbl 用现有 supervisor 路径启动，保留健康探针、LKG、回滚。
- 预设组件：acbl 为每个组件 **单独建进程组**（Unix `setsid` / `process_group(0)`），停止时 `SIGTERM` 到进程组，超时 `SIGKILL` 到进程组。这样脚本里 `serve &` 的孙进程不会漏掉。
- 每个组件独立的状态机：`starting → healthy → unhealthy → restarting → failed`。组件 `failed` 只影响自己，不退出 acbl。
- 全局只有一张“组件表”，不引入 systemd 式的 unit 依赖图；`after` 仅表达启动顺序（可选）。

### 4.2 75/78 的去向（收窄）

| 场景 | 退出码 | 谁处理 |
|---|---|---|
| payload 升级提交指针 | **不退出** | acbl 原地停旧 bridge、提交、起新 bridge；进程不重启 |
| bridge 回滚到 LKG | **不退出** | acbl 原地停失败版本、写回 LKG 指针、起旧 bridge |
| acbl 自身崩溃 / 被系统 switch 重启 | 非 75 | init 的 `Restart=on-failure`（A-8 定稿）或 `acbctl run` 父循环拉起；bridge 不受影响，新 acbl 先探测身份再 adopt（§4.7） |
| 配置/指针/journal 不可恢复 | **78** | 不再重拉，等人处理 |
| 回滚后的 LKG 再失败 | **78** | 同上 |
| 启动时契约版本不兼容 | **78** | 明确报错：acbl 不认识当前 generation，必须人工换 payload |
| 外部预设组件失败并达到自己的上限 | **不退出** | acbl 标记 `failed`，继续管其它组件 |
| 用户显式 stop / SIGTERM | **0** | 只退出 acbl：bridge 脱钩继续运行，下次启动 adopt；整栈停止是单独动作（§4.7.3） |

**75 的新定义：兼容保留值，升级路径不再发出。**

- 旧语义是“指针已提交 / 已回滚，请重启我”。新结构里 acbl 不参与换代，没有“换自己”的场景，所以升级与回滚都不再退出 75。
- 码值保留，unit 模板里的 `RestartForceExitStatus=75` 继续留一版，只为兼容尚未升级的旧 acbl；新代码不得依赖 75 完成任何升级步骤。
- 如果未来真的出现“acbl 自身的启动环境变了、必须重启进程才生效”的场景，可以重新启用 75，但必须单独设计并写回本文。当前没有这样的生产路径：`[[component]]` 热加载、payload 换代、预设切换都不需要。

**78 的新定义保持不变**：配置、指针、journal、契约任一项不可恢复时，acbl 放弃自动恢复并退出 78，init 不得再自动拉起。它描述的是“现场坏了、需要人”，不是“升级失败”。

**规则：75/78 只描述 acbl 自身。** 预设组件永远不能把整栈拖进 75/78，否则一个 dsh 崩溃就会触发桥回滚，语义错位。

### 4.3 谁启动 acbl：三种并列方式

用户给出的方向是三类并列，安装器按参数选择，不做环境探测：

| 方式 | acbl 崩溃 / 78 后由谁重拉 | 日志归谁 / 怎么轮转 | cgroup 隔离 | 升级 payload 时 acbl 是否重启 |
|---|---|---|---|---|
| **A. 用户终端手工启动**（前台 `acb-launcher`） | **没有人**。78 后停在终端等用户处理；崩溃后用户自己重跑 | 终端输出归用户；要留存就自己重定向（如 `> log 2>&1`），轮转由用户/外部工具做 | **无**（和用户 shell 同 cgroup） | 否（只原地重启 bridge） |
| **B. systemd 开机自启**（`enable --now acb-launcher-user.service` / 系统级） | systemd：`Restart=on-failure` 拉起异常退出；`RestartForceExitStatus=75`；`RestartPreventExitStatus=78` 停在 failed。正常 stop（退出 0）不会被重拉 | journald（stdout/stderr）或模板里的 `StandardOutput=append:` 文件；轮转由 journald / logrotate 负责 | **有**（独立 service cgroup，`KillMode=process`；stop 不连带 bridge） | 否 |
| **C. systemd 只做进程守护、不开机自启**（装 unit 但不 `enable`，或 `ACB_UNIT_INSTALL_SECTION=0`） | 同 B，但只有 `systemctl start` 之后才存在 | 同 B | **有**（同 B） | 否 |
| **D. 无 systemd 自管**（runit / s6 / openrc / 容器 entrypoint / `acbctl run --mode=launcher`） | 用户的 init 或 `acbctl run` 父循环：普通崩溃重拉、78 停止（容器里 78 转成正常停止） | 由 init / 容器日志驱动；`acbctl run` 下是 `run.json` + launcher 自身输出；轮转由 init / runtime 负责 | **取决于 init**（runit/s6 通常有；裸前台没有） | 否 |

**共同点：升级链在四种方式下都不重启 acbl。** 它只在 acbl 进程内停旧 bridge、切指针、起新 bridge。方式之间的差别只剩“acbl 自己崩溃/放弃时谁把它拉起来、日志去哪、有没有 cgroup”。

安装器要预留的接口（避免以后返工）：

- `ACB_MANAGED_BY`：`manual | systemd-user | systemd-system | container | custom-init | acbctl-run`，写进 `capabilities.json`，**只用于诊断与生成文档**，不参与升级准入。
- `ACB_UNIT_INSTALL_SECTION`：0/1，决定是否渲染 `[Install]`（对应方式 B / C）；只影响渲染，不执行 enable。
- `ACB_UNIT_SCOPE`：`user | system`，决定 unit root、`loginctl enable-linger`、以及系统级 `User=` 的必要性。
- `ACB_LOG_TARGET`：`journal | file | inherit`，决定 unit 的 `StandardOutput=` 和安装器是否创建日志目录；日志轮转责任随 target 明确归属。
- `ACB_UNIT_LOG_DIR` / `ACB_UNIT_ENV_FILE`：日志与密钥路径，安装器只写模板占位，不在 unit 里写死。
- `acbctl unit install --with-install-section=0|1 --scope=user|system`：一次性把 A/B/C 的差异表达清楚；D 由文档与示例覆盖，不塞进同一开关。

**`restart_owner` 的结论：取消，`self` 档取消。**

- 升级不再需要“外部把 acbl 拉起来”，所以 `restart_owner != "none"` 这个升级门禁没有存在意义。
- `self` 档表达“acbl 重拉自己”，在新结构里不存在：进程不能可靠地重启自己；acbl 的版本变化由系统层（`nixos-rebuild switch` / 包升级 / 人工替换）负责，崩溃由 init 负责。
- `capabilities.json` 不再写 `restart_owner`，改写 `managed_by`，仅作诊断；升级准入条件改为“`launcher_pid` 存活 + 信箱可写”，不再检查重启方。
- 旧字段读到就忽略（不报错），避免旧 acbl 与新 `acbctl` 组合时互相卡住。

### 4.4 升级时谁被重启

分两条完全独立的路径：

| 路径 | 触发 | 换掉什么 | 重启谁 | 频率 |
|---|---|---|---|---|
| **payload 升级** | `acbctl upgrade` / 控制台 / `profile switch` | generation 里的 bridge + harness + drivers 实现 | 只重启 bridge；dsh / drivers / opencode 继续跑 | 高频 |
| **acbl 升级** | `nixos-rebuild switch` / 包升级 / 人工替换二进制 | acbl 自身（含预设、unit 模板） | 重启 acbl；**bridge 不重启**（新 acbl adopt），预设组件随 acbl 重启 | 低频 |

- payload 升级默认不动预设组件：只停 bridge、切指针、原地起新 bridge。
- 如果某个预设声明了 `after = ["bridge"]` 或 `restart_on_bridge_upgrade = true`，acbl 在 bridge 重新就绪后再重启它；这是组件的选择，不是全局行为。
- 升级失败回滚时同理：只回滚 bridge，不动外部组件。
- **acbl 升级不再是全栈中断**：bridge 在 acbl 停止与重启期间继续服务，新 acbl 通过 adopt 接管（§4.7）；只有显式整栈停止才让 bridge 一起停。升级链永远不替换 acbl。

### 4.5 启动顺序

- 组件默认并行启动，不引入强依赖图。
- `after = ["name"]` 只保证“对方进入 healthy 后再启动我”；对方 `failed` 时我不启动，标 `blocked`（不是 `failed`）。
- 环依赖在启动时直接报配置错误（78），不猜测。
- 启动顺序只在 acbl 自己的进程树里生效；payload 升级只换 bridge，不重排组件表。

### 4.6 热加载：只有按钮，不自动

**不做自动热加载。** 文件变了不自动生效；控制台提供“重新加载组件表”按钮，用户点击后尝试一次热加载。

加载对象**只有一个**：

- `supervisor.toml` 的 `[[component]]` 表（含每个组件的 `preset` / 来源 / 启用开关 / 覆盖字段）。

不能通过这个按钮热加载的东西：

- bridge 的 `[supervisor]` 字段（`program` / `args` / `health_url` / policy）——那是核心组件，走升级链；需要改动时重启 acbl；
- profile / `current.json` / LKG / 升级请求；
- 密钥与环境文件内容（改了要重启对应组件或 acbl）；
- 插件、wake / delivery / 会话等业务策略；
- systemd unit、日志路径、acbl 自身的启动方式。

收敛动作（按组件名 + 规格指纹对比新旧表）：

| 变化 | 动作 |
|---|---|
| 新增组件 | 启动它 |
| 删除组件 | 停止它（放在最后一步） |
| 规格变更 | 先停旧、再起新；新起失败则回滚到旧规格 |

**失败必须保留旧表继续运行，不允许出现“停一半”的中间态。** 分两阶段：

1. **校验阶段（无副作用）**：解析、schema 校验、多来源互斥检查、变量替换检查。任一失败 → 直接报错，运行中的组件一个不动。
2. **应用阶段（可回滚）**：
   - 先启动所有新增组件；任一失败 → 回滚已启动的新组件，旧表继续跑。
   - 再逐个重启变更组件；某个新规格在 `startup_grace_secs` 内没就绪 → 停新、起回旧规格，并中止后续变更。
   - 最后停止被删除的组件（只有前面都成功才执行）。
   - 补偿动作本身失败时，组件保持在“旧规格已停、新规格未起”的状态会违反约束，因此必须退化为“标 `failed` 并保留旧规格的启停意图”，由状态文件暴露给人处理，不静默吞掉。

按钮语义：同一时间只允许一个重载任务；重复点击幂等（用表指纹判断“无变化”）；返回逐组件结果（`added` / `removed` / `restarted` / `unchanged` / `failed` / `rolled_back`），并写一条审计事件。

### 4.7 bridge 的独立生命周期与接管（adopt）

**角色重新定义：acbl 是观察者 + 恢复者，不是所有者。** bridge 的进程生命周期独立于 acbl：acbl 停止、崩溃、升级期间，bridge 继续服务（7700 仍有响应）。acbl 只负责“发现它、接管它、在它死后拉起它”，不负责“我的死必须带走它”。

#### 4.7.1 进程脱离要求

| 平台 | 要求 | 原因 |
|---|---|---|
| Unix（systemd） | launcher unit 用 `KillMode=process`，**不能** `control-group` / `mixed` | 默认 `control-group` 会在 `systemctl stop acb-launcher` 时杀掉整个 cgroup，bridge 作为同一 cgroup 的子进程会被一起杀 |
| Unix（acbl 自己） | bridge 用 `setsid` / 新 session 启动，与 acbl 的进程组脱钩；acbl 不因自身退出向 bridge 发信号 | 无 systemd（#2/#4/#6/#8）时没有 cgroup 兜底，只能靠进程组分离 |
| Windows | Job Object **不能**设 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`；bridge 用独立 Job 或完全脱管 | 设了 kill-on-close，acbl 的 Job 句柄一关，bridge 就被系统杀掉（C-5 的等价要求） |

补充：

- bridge 的 stdout/stderr 不能与 acbl 的终端生命周期绑定；systemd 下写入 journald，无 systemd 时由 acbl 重定向到 `<state>/logs/bridge.log`，acbl 退出后文件句柄仍由 bridge 持有。
- bridge 自行处理 SIGTERM/SIGINT；acbl 停止时**不转发**信号；只有显式“整栈停止”才转发。
- `KillMode=process` 的代价：systemd 不再替 acbl 清理遗留子进程，孤儿清理责任回到 acbl 的显式停止流程与 bridge 自己的生命周期。

#### 4.7.2 接管（adopt）语义

acbl 启动时**必须先探测、再决策**，禁止无条件起新 bridge，否则会出现两个 bridge 抢 7700。

1. **读本地真值**：`state/supervisor/current.json` 得到应运行的 generation id；`state/supervisor/bridge.json`（新增，见 4.7.4）得到上次记录的实例身份。
2. **探活与身份校验**：请求 bridge 的身份端点，拿回 `instance_id` / `generation_id` / `started_at_ms` / `contract`。
3. **比对与决策**：
   - `generation_id` 与 `current.json` 一致，且 `instance_id` 与 `bridge.json` 记录一致 → **adopt**：不重启，直接进入观察（健康探针 + 稳定窗口 + LKG 照常）。
   - `generation_id` 与 `current.json` 不一致 → **不接管、不杀**：说明有另一代或另一个实例在跑（升级中途或人工操作）。标 `conflict`，写事件，退出 78，等人处理。
   - 7700 可连但身份端点不可用 / 不匹配 → **视为未知实例**：不启动第二个、不杀，标 `conflict` 并退出 78。宁可停机等人，也不制造双实例。
   - 端口无响应且无存活身份 → 按 `current.json` **启动**新 bridge，写新的 `instance_id`。
4. **禁止用 pid 猜测身份**：pid 会被复用，`/proc/<pid>` 只能证明“某个进程占了这个号”，不能证明“它是我上次起的那个 bridge”。身份必须来自 bridge 自己声明的 `instance_id`（ULID 或随机 128-bit）+ `generation_id` + `started_at_ms` 三元组；pid 只作诊断字段。
5. **双实例防护**：以“身份端点 + 端口探测”为正证据；acbl 侧再加 `<state>/supervisor/adopt.lock`（`flock`，只做 acbl 之间的互斥，防止两个 acbl 同时接管）。bridge 侧不需要额外锁——绑定 7700 本身就是唯一性约束。

adopt 成功后与“自己启动”走同一套观察逻辑：健康探针、稳定窗口、LKG 提升只在**重新确认健康满窗口**后发生，不因为 adopt 就立即提升。

**A-8 已落地的过渡形态**（完整形态仍属 B-12）：

- bridge 由 `ProcessRunner` 拉起后原子写 `<state_dir>/bridge.instance.json`，含 `instance_id`、pid、`/proc/<pid>/stat` starttime、exe 路径、程序与参数；pid 只作诊断，身份由 starttime + exe + 参数 + `instance_id` 共同判定。
- acbl 启动时读该记录：无记录但健康探针通过 → conflict(78)；pid 不在或 starttime/exe 不匹配 → 视为空缺，按 `current.json` 启动；全部匹配 → adopt，不重复拉起。
- 两个 acbl 的互斥复用 `ActivationWriter` 对 `current.json` 的 `flock`，并**覆盖整个决策窗口**：从启动对账、探测、决定 adopt/拉起，到 `Supervisor::new` 完成首次 start 才释放；因此第二个 acbl 不会在第一个尚未拉起 bridge 时同时判定“没有 bridge”。首次 start 后必须释放，否则后续升级提交/回滚在同一进程内重新加锁会自锁。单独的 `adopt.lock` 不再需要。
- **TODO（B-12）**：把本地记录比对替换/补充为 bridge 自报的 `/internal/instance`；调用点在 `crates/supervisor/src/instance.rs` 与 `crates/launcher/src/main.rs` 的 detect 分支。B-12 已落 `acb-contract.json` 的 `contract` / `min_acbl` 校验，但不含实例端点。

#### 4.7.3 崩溃 vs 正常停止

| 事件 | acbl | bridge | 谁恢复 |
|---|---|---|---|
| **acbl 崩溃**（panic / OOM / `kill -9`） | 进程消失 | **继续运行**，7700 不断 | init 按 `Restart=on-failure` 重拉 acbl；新 acbl adopt 现有 bridge |
| **acbl 正常停止**（`systemctl stop`、升级 acbl、SIGTERM） | 前台进程退出 0 | **继续运行**，服务不中断 | `Restart=on-failure` 不会重拉退出 0 的 acbl；升级：系统层/安装脚本启动新 acbl，adopt 旧 bridge；手工 stop：等人再 start |
| **acbl 退出 78** | 放弃自动恢复 | **继续运行**（除非 bridge 本身也坏了） | 人处理；bridge 在此期间仍服务，但无人拉起 |
| **bridge 崩溃（acbl 在跑）** | 存活 | 退出 | acbl 按组件策略重启 bridge |
| **bridge 崩溃（acbl 不在）** | 不在 | 退出 | **没有人**；见 4.7.5 的无守护窗口 |
| **升级 payload** | 存活 | 旧停、新起 | acbl 原地完成（§4.4） |

**显式整栈停止**是单独动作：`acbctl stack stop`（或 `acbl --stop-components`）先停 bridge 与预设组件，再退出 acbl；只有这个动作会让 bridge 随 acbl 一起停。默认 SIGTERM / `systemctl stop` 只停 acbl。

#### 4.7.4 身份文件与端点（契约 v1 增补）

bridge 启动成功后原子写 `<state>/supervisor/bridge.json`：

```json
{
  "schema": 1,
  "instance_id": "br-01J8...",
  "generation_id": "gen-...",
  "pid": 23456,
  "started_at_ms": 1780000000123,
  "contract": 1,
  "listen": "127.0.0.1:7700"
}
```

并提供身份端点（与健康端点分开，避免把健康检查当身份认证）：

```http
GET /internal/instance
200 {"instance_id":"br-01J8...","generation_id":"gen-...","started_at_ms":1780000000123,"contract":1}
```

- 端点只监听环回地址；不是管理 API，不需要写权限。
- `bridge.json` 只是缓存与诊断，**不是身份真值**；真值以端点返回为准。文件与端点不一致时按端点，并重写文件。
- **这两项不在契约 v1 里**（B-12 只落声明与校验，bridge 侧尚未实现），属于 adopt 落地后的新增项；落地时按 §6.4 的只增不改规则决定升 `contract` 还是作为可选新增字段。没有 `/internal/instance` 的 payload 在 adopt 落地前只能按“未知实例”处理（4.7.2）。

#### 4.7.5 无守护窗口（接受的代价）

acbl 停止期间 bridge 仍在服务，但**如果 bridge 在这段时间死亡，没有任何进程会拉起它**。这是“acbl 不拥有 bridge”的直接代价。

- **当前选择：接受并写明。** 系统层/安装脚本重启 acbl 的窗口通常只有秒级；这段时间 bridge 死亡属于双重故障，人工介入可接受。
- **不引入隐藏看门狗**：不在 bridge 里内嵌“发现 acbl 不在就自愈”的逻辑，也不装第二个 unit 去守护 bridge——那会重新制造“两个所有者”，与 4.7.2 的双实例防护冲突。
- **备选（未决策）**：若后续要求“acbl 停机期间 bridge 也必须有守护”，可选 systemd `Restart=always` 直接托管 bridge（回到 legacy 拓扑），或独立轻量看门狗 unit；两者都要重新定义所有权，本文**不采纳**，标为未决策。

### 4.8 代码改动清单（只列，不实现）

supervisor 目前是 owner 模型：`ProcessRunner` 持有 `Child`，`stop()` 直接 kill，launcher 退出即视为整栈结束。改成 observer / adopt 需要以下改动：

| # | 文件 / 位置 | 改动 | 目的 |
|---|---|---|---|
| M1 | `crates/supervisor/src/lib.rs`（`ProcessRunner` 266-319、`stop` 289-299） | 拆成 `stop_child()` 与 `shutdown()`；`Drop` 不再杀子进程；bridge 用 `setsid` / `process_group(0)` 启动 | 让 bridge 的生死脱离 acbl |
| M2 | `crates/supervisor/src/lib.rs`（日志重定向 273-287） | 子进程 stdout/stderr 改为文件或 journald 流，不继承 acbl 终端 | acbl 退出后日志仍可写 |
| M3 | 新增 `crates/supervisor/src/adopt.rs` | 实现 4.7.2 的探测 → 身份比对 → adopt / start / conflict(78)；含 `adopt.lock` | 唯一接管入口 |
| M4 | `crates/launcher/src/main.rs`（启动 82-107、退出 389-436） | 启动先 adopt 再决定是否 spawn；SIGTERM 只停自身、不转发给 bridge；新增显式整栈停止路径 | 崩溃 / 正常停止不带走 bridge |
| M5 | `crates/launcher/src/main.rs`（capabilities 489-525） | 写 `bridge.json` 缓存；`capabilities.json` 增 `bridge_instance_id` / `adopted` 诊断字段 | 可观测 |
| M6 | `crates/bridge`（HTTP 路由） | 新增 `GET /internal/instance`；启动原子写 `bridge.json` | 身份可验证 |
| M7 | `crates/acb/src/unit.rs`（模板 24-48；`ACB_UNIT_*` 539-549）与 unit 模板 | `acb-launcher*.service` 改 `KillMode=process`；`ACB_UNIT_LAUNCHER_BIN` 默认静态入口；补 `TimeoutStopSec` | systemd 不连带杀 bridge |
| M8 | `crates/acb/src/profile.rs` / `crates/acb/src/contract.rs` | B-12 已收窄必带项并落三处校验；adopt 落地时再把 `/internal/instance` / `bridge.json` 作为新增项纳入（是否升 `contract` 见 §6.4） | 契约与 adopt 衔接 |
| M9 | `deploy/run/supervisor.toml.tmpl` | 新增 `detach = true`、`identity_url = "http://127.0.0.1:<port>/internal/instance"` | 配置表达 |
| M10 | Windows（C-5） | Job Object 不设 kill-on-close；独立 Job 或脱管 | 等价语义 |

以下部分**已在 A-8 落地**（其余 M 项仍待后续）：

| 项 | 落地内容 | 备注 |
|---|---|---|
| M3 | 新增 `crates/supervisor/src/instance.rs` + `adopted.rs`：探测 → 身份比对 → adopt / start / conflict(78) | 身份先按本地 `bridge.instance.json`（pid + `/proc` starttime + exe + 参数），不是端点 |
| M4 | launcher 启动先 detect 再选 runner；正常停止只退出自身，不杀 bridge | 崩溃路径靠 unit `KillMode=process` + `Restart=on-failure` |
| M7 | unit 模板移到 `share/acb/units/`，`ExecStart` 指向静态 acbl；`KillMode=process`；`Restart=on-failure` + `Force=75` / `Prevent=78` | 安装入口：`ACB_UNIT_ACBL_PREFIX`（默认 `/usr/local`） |
| 未做 | M1（setsid）、M2（日志重定向）、M5（`capabilities.json` 字段）、M6（`/internal/instance`）、M9（配置项）、M10（Windows） | M6 属 adopt 后续；M10 属 C-5 |
实现阶段的验证：`kill -9` acbl 后 bridge 仍在 7700 响应；新 acbl 启动后 adopt、不产生第二个进程；bridge 属于别的 generation 时 acbl 拒绝接管并 78；`systemctl stop acb-launcher` 后 bridge 仍健康。

### 4.9 acbl 运行时注册 systemd 单元（新增能力，未实现）

**定论：unit 由 acbl 生成，不由 nix-config 声明；安装与启用是两件事。** 能力现状为零——今天 unit 仍由 nix-config 模块产出（§2.4.1）。

| 动作 | 行为 | 明确不做 |
|---|---|---|
| `acbctl unit install [--scope user\|system] [--with-install-section 0\|1]` | 用编进 acbctl 的模板（`crates/acb/src/unit.rs:24-48`）渲染出 `acb-launcher.service`（含可选 `[Install]`），原子写入 unit root；内容带生成标记 `# Generated by acbctl; do not edit` | **不 enable、不 start**；不改 `nixos-rebuild` 产物 |
| `acbctl unit remove` | 只删除带生成标记、且登记在 acbl 状态目录的 unit；先 `disable` 若它曾被 enable | 不碰 nix-config、不碰用户手写 unit |
| `acbctl unit status` | 输出 unit 路径、`generated_by=acbl`、`enabled/active`、落盘时间与模板版本 | 不改任何状态 |

- **只有这三个命令可以调用 `systemctl`（仅 `daemon-reload` 与 `disable`）**，且必须由运维显式触发；launcher 正常监督、升级、adopt 路径一律不碰 systemd（§2.3）。
- **幂等与漂移**：重复 install 内容一致则不写、不做 `daemon-reload`；发现同名 unit 但没有生成标记 → `unit_conflict`，拒绝覆盖（防止盖掉 nix-config 回退期的旧单元）。
- **`[Install]` 只影响渲染**：`--with-install-section 1` 允许部署者之后 `enable`；默认 `0`，对应“允许安装但不启用”。这与“nix-config 不写死”互不影响：来源归 acbl，时机归部署者。
- **升级 acbl 不自动重写 unit**：稳定入口（§2.4）不变时 unit 内容不变；模板版本变化由 `unit status` 暴露，运维显式重跑 install。
- **作用域限制**：`--scope system` 需要 root；`user` 作用于当前用户 `.config/systemd/user`。acbl 不自动选择 scope，也不做环境探测（§4.3）。
- **与组件的关系**：组件**没有**自己的 unit（§2.4.1）。即使某组件不适合 acbl 进程模型，也不在 v1 里改成 systemd 托管；那会重新引入两套生命周期。

---

## 5. 组件管理契约 v1（可评审稿）

状态：契约稿，**组件管理目前 0 实现**。本节把设计意图固化成可评审的 v1 契约；实现另起分支，按 §5.11 分阶段落地。

### 5.0 基线事实与目标差距

| 事实（本轮实测） | 现状 | 契约处理 |
|---|---|---|
| `supervisor.toml` 只解析 `[supervisor]`；`crates/supervisor/src/lib.rs` 无 components | 组件表写进去也不会生效 | v1 新增 `[[component]]`，实现方必须按 §5.1 完整解析 |
| 仓库无 `deploy/presets/`、无 `presets.toml`；`acbctl` 无 component / preset 子命令 | 预设体系不存在 | 本节定义目录、清单与只读命令 `acbctl component ...` |
| serde 默认忽略未知字段 | 往 `supervisor.toml` 写 `[[component]]` 会被**静默忽略** | 强制所有清单结构 `#[serde(deny_unknown_fields)]`，启动 / 重载时校验；未知字段报 `component_unknown_field` 并拒绝启动 |
| 三个组件现在是 nix-config 的 systemd user 单元 | `dsh-acb`：无 bridge 依赖、`PartOf=acb-stack.target`、`Restart=always`；`acb-drivers`：`Requires=bridge`、`After=bridge+dsh`、`PartOf=bridge+stack`；`acb-opencode-serve`：`After/Wants=bridge`、`PartOf=stack` | 按 §5.10 映射成组件表；迁移按 §5.8 先 mask 再启用；最终五个定义全部从 nix-config 移除（§2.4.1） |
| nix-config 仍声明 5 个 ACB 定义 | `acb-bridge.service`、`acb-stack.target`、`dsh-acb`、`acb-drivers`、`acb-opencode-serve` | 目标态 nix-config 只留 `acbl` 包；unit 由 acbl 运行时生成（§4.9） |
| snowluma 侧车（rootless podman compose）无人管理 | 用户管理器重启后容器消失，桥侧重试 43 次才连回 | **v1 最小形态**：`type = "oneshot"` + 预设脚本，按配置的 `file` / `compose_bin` 做 `up -d` / `down`；只到“拉起 / 关闭 / 端口探活”（§5.10.2） |
| `acb-opencode-serve` 的启动脚本在 payload（`deploy/opencode-serve.sh`） | 与“预设随 acbl 分发”冲突 | **定论：v1 把 opencode 启动脚本移入 acbl 预设**（`presets/opencode/opencode_<source>.sh`），payload 不再携带；B-12 已把它移出必带项 |
| launcher 不自启（无 `[Install]` / `WantedBy`，不 enable） | 用户管理器重启后整栈不会自动恢复 | 明确接受：由部署者显式启动 acbl；见 §5.4、§5.8 |
| 实测：bridge 冷启动到可服务 23–25s；探针 5s × 连续 10 次阈值安全；launcher 退出会连带停 bridge；payload 升级仍走 75 → systemd 重拉 | 当前是 owner 模型 | 组件契约以这套基线为兼容起点；§4.7 的 adopt 是目标态、尚未实现，orphan 语义按 §5.5 分两步落地 |

### 5.1 清单格式：`[[component]]`

**位置**：`supervisor.toml`（目标态由 acbl 自持有于 `state/supervisor/supervisor.toml`，见 §2.4.2）。bridge 继续用 `[supervisor]`；`[[component]]` 是可选顶层数组，没有它时行为与今天完全一致。不引入第二个 `components.toml`：一个文件只有一个写者，避免启用状态与 bridge 配置分叉。

**未知字段硬要求**：`Component` / `process` / `readiness` / `restart` / `depends` / `log` 全部 `deny_unknown_fields`。解析失败时报 `components[i].<field>`，acbl **拒绝启动**，不允许“写错字段但静默跑”。

字段（必填标 *）：

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `name`* | string | — | 组件名，全局唯一；不得为 `bridge`（保留给 `[supervisor]`）；`[a-z][a-z0-9-]*` |
| `enabled` | bool | 预设的 `default_enabled`（内置一律 false）；自定义组件为 false | 同一组件只允许一个 `enabled = true`；`type = "oneshot"` 且 `file` 为空时一律不启用 |
| `type` | `"simple" \| "oneshot"`（预留 `"notify"`） | 预设的 `type`（内置默认 `simple`） | 可省略并继承预设；显式写则必须与预设一致。`simple` = 前台常驻（对应 `Type=simple`），dsh / opencode / drivers；`oneshot` = 一次性命令（对应 `Type=oneshot` + `RemainAfterExit=yes` 语义），snowluma；`notify` 预留第三值（acbl / bridge 已在用 sd_notify），**v1 不实现**；见 §5.3 |
| `file` | string | `""` | 仅 `type = "oneshot"`：compose 文件绝对路径；**空 = 不启用**，路径只能来自配置，禁止写死在预设脚本 |
| `compose_bin` | string[] | `[]` | 仅 `type = "oneshot"` 且必填（空 = 未配置，报错）：目标机真实可用的 compose 工具；`podman-compose` 与 `podman compose` 是不同工具，**不设默认值、不自动回退、不猜** |
| `stop_action` | `"down" \| "leave_running"` | `"leave_running"` | 仅 `type = "oneshot"`：停止时关闭项目还是保持运行；理由见 §5.10.2 |
| `preset` | string | — | 引用 `presets.toml` 的 `id`；与 `command` 二选一 |
| `command` | string[] | — | 自定义组件的可执行文件 + 参数；不接受 shell 字符串 |
| `args` | string[] | `[]` | 追加给预设脚本的位置参数；用 `command` 时忽略 |
| `env` | map<string,string> | `{}` | 按 key 合并到预设 / 自定义环境，最后覆盖 |
| `[component.process]` | table | — | 进程执行参数：`cwd`、`env_file`、`kill_mode = "process" \| "group"`（默认 `group`，仅 `type = "simple"` 生效） |
| `[component.readiness]` | table | 见 §5.3 | 探针：`kind = "tcp" \| "http" \| "process"`、`host`、`port`、`path`、`interval_secs`、`failures_threshold`、`success_threshold`、`startup_grace_secs`；**`type = "oneshot"` 必须配探针**，缺失解析期拒绝；`simple` 缺省退回 `process` |
| `[component.restart]` | table | `policy = "on-failure"` | `policy`、`max_attempts`（默认 5）、`backoff_secs`（默认 `[1,2,4,8,30]`）、`backoff_cap_secs`（默认 60）；达到上限 → `failed` 终态；见 §5.3 |
| `[component.depends]` | table | `{}` | `after` / `requires` / `part_of` / `wants`，见 §5.4 |
| `[component.log]` | table | `target = "file"` | `target`、`path`；见 §5.6 |

`source` / `script` 不是 `[[component]]` 字段：它们属于预设条目，组件只引用 `preset`；状态输出里显示派生的 `source`（预设来源或 `custom`）。用户在 `[[component]]` 写 `source` / `script` 按未知字段报错。

```toml
[[component]]
name = "dsh"
preset = "dsh_nix"
enabled = true
env = { DSH_BIN = "/run/current-system/sw/bin/dsh" }
[component.depends]
part_of = ["stack"]
[component.readiness]
kind = "tcp"
host = "127.0.0.1"
port = 3080

[[component]]
name = "my-sidecar"
enabled = true
command = ["/usr/local/bin/my-sidecar", "--port", "7788"]
[component.process]
cwd = "/var/lib/my-sidecar"
kill_mode = "group"
[component.readiness]
kind = "tcp"
host = "127.0.0.1"
port = 7788

[[component]]
name = "snowluma"
type = "oneshot"
preset = "snowluma_oneshot"
enabled = true
file = "/absolute/path/to/snowluma-compose.yml"   # 空 = 不启用
compose_bin = ["podman-compose"]                   # 或 ["podman", "compose"]，按目标机实际
stop_action = "leave_running"
[component.depends]
part_of = ["stack"]
[component.readiness]
kind = "tcp"
host = "127.0.0.1"
port = 53001
```

校验错误码（启动 / 重载共用）：`component_unknown_field`、`component_duplicate_name`、`component_reserved_name`、`component_missing_preset_or_command`、`component_preset_unknown`、`component_preset_component_mismatch`、`component_multiple_sources`、`component_dependency_unknown`、`component_dependency_cycle`、`component_script_missing`、`component_script_not_executable`、`component_env_unknown_var`、`component_readiness_required`（oneshot 未配探针）、`component_readiness_invalid`；oneshot 专属：`component_oneshot_file_missing`（`file` 为空却启用）、`component_oneshot_file_not_found`、`component_oneshot_compose_bin_missing`、`component_oneshot_compose_bin_not_found`、`component_oneshot_stop_action_invalid`。

与 `[supervisor]` 的关系：

- `[supervisor]` 仍是唯一的核心组件（bridge）：健康探针（`health_url` + `health_token_file`）、LKG、升级链只作用于它；**bridge 探针不在 `[[component]]` 里**，两套探针 v1 并存（§5.3）。
- 组件重启策略不能突破 acbl 的全局风暴上限（例如 60s 内总重启次数）；`[component.restart]` 只收紧，不放大。
- 组件不参与 `current.json` / `lkg.json` / 升级请求；见 §5.9。
- 组件 `name = "bridge"` 直接拒绝，避免两套生命周期。

### 5.2 预设布局与脚本契约

目录（随 acbl 安装，不随 payload）：

```text
deploy/presets/
  presets.toml
  dsh/dsh_nix.sh  dsh/dsh_npm.sh ...
  opencode/opencode_nix.sh  opencode/opencode_npm.sh
  drivers/drivers_acb.sh     # 只指向 payload 的执行体
  snowluma/snowluma_oneshot.sh  # type = "oneshot"：只做 up -d / down
```

命名：`id = <component>_<source>`，脚本路径 `presets/<component>/<component>_<source>.sh`。`source` 词表固定：`nix` / `npm` / `guix` / `scoop` / `homebrew` / `distro` / `container` / `acb`；未知来源报错。同一组件同一时刻只能启用一个来源。

`type = "oneshot"` 的预设（v1 只有 `snowluma_oneshot`）不描述“怎么常驻一个前台脚本”，而是“用配置里给出的 compose 工具与文件，对项目执行 `up -d` / `down`”。脚本只做三件事：校验 `file` 与 `compose_bin`、执行 `<compose_bin...> -f <file> up -d`、按 `stop_action` 决定停止时是否 `down`。命令名不猜：`podman-compose` 与 `podman compose` 是两个不同工具，配置写哪个就用哪个，找不到即报错。**`oneshot` 不适用本节的“前台常驻”契约**：脚本执行完即退出 0，不 `wait`、不接管 PID，见 §5.10.2。

`presets.toml` 只登记“存在哪些预设”与默认值，不记录启用状态：

```toml
schema = 1
[[preset]]
id = "dsh_nix"
component = "dsh"
type = "simple"
source = "nix"
script = "dsh/dsh_nix.sh"
description = "dsh 由 nix 提供"
default_enabled = false
[preset.process]
cwd = "@ACB_UNIT_STATE_DIR@"
env_file = "@ACB_UNIT_ENV_FILE@"
kill_mode = "group"
[preset.readiness]
kind = "tcp"
host = "127.0.0.1"
port = 3080
interval_secs = 5
failures_threshold = 5
startup_grace_secs = 60
[preset.restart]
policy = "on-failure"
backoff_secs = [1, 2, 4, 8, 30]
backoff_cap_secs = 60
max_attempts = 5
```

**脚本调用契约**（预设脚本与自定义 `command` 统一适用）：

| 项 | 约定 |
|---|---|
| 调用 | 直接 exec，不经过 shell 字符串拼接。预设脚本收到 `$1 = component name`，其后是 `args` 数组 |
| 注入环境 | `ACB_COMPONENT`、`ACB_PRESET_ID`（自定义为空）、`ACB_COMPONENT_STATE_DIR`、`ACB_COMPONENT_LOG_FILE`、`ACB_ENV_FILE`；`component.env` 最后覆盖 |
| 变量替换 | `presets.toml` 只允许 `@ACB_UNIT_*@` 白名单（`STATE_DIR` / `ENV_FILE` / `OPENCODE_PORT` 等）；未知变量报错，不做任意 shell 展开 |
| 前台 | 必须前台常驻，**禁止 `&` / daemonize / `nohup` / 自建 session**；脚本的子进程必须留在同一进程组并被 `wait` |
| 退出码 | 0 = 收到 SIGTERM 后正常退出；非 0 = 失败，交给重启策略；禁止用 75 / 78 |
| 信号 | SIGTERM / SIGINT：在 `stop_timeout_secs`（默认 45）内转发给子进程并退出；SIGHUP 忽略（v1 不支持热重载） |
| 日志 | stdout/stderr 由 acbl 重定向到 `ACB_COMPONENT_LOG_FILE`，脚本不要自己再起日志文件 |
| 边界 | 不得调用 `systemctl` / `nix` / `sudo`；不得写 `profile/`、`current.json`、`lkg.json`、`upgrade/`；只允许写自己的 `state/components/<name>/` |

为什么禁止 daemonize：acbl 的停止、孤儿判定、双实例防护全部依赖“组件是 acbl 的直接子进程 / 可识别的 session”。脚本自己 fork 脱离后，acbl 既杀不到也认不出，退化成 systemd 时代的 `PartOf` 猜谜。

### 5.3 就绪与健康

区分两个概念：

- **启动完成（ready）**：readiness 条件首次满足。
- **可服务（healthy）**：readiness 连续满足 `success_threshold` 次（默认 3 次 × 5s = 15s）。

**探针按组件配置**，位于 `[[component]]` 的 `[component.readiness]`：

| 字段 | 类型 | 默认 | 适用 |
|---|---|---|---|
| `kind` | `"tcp" \| "http" \| "process"` | `process` | `simple` 缺省 `process`；`type = "oneshot"` 必须显式给出且不能为 `process` |
| `host` | string | `127.0.0.1` | tcp / http |
| `port` | u16 | — | tcp / http 必填 |
| `path` | string | `/healthz` | 仅 http |
| `interval_secs` | u64 | 5 | 全部 |
| `failures_threshold` | u32 | 5 | 全部 |
| `success_threshold` | u32 | 3 | 全部 |
| `startup_grace_secs` | u64 | 60 | 全部 |

完整示例（opencode）：

```toml
[[component]]
name = "opencode"
type = "simple"
preset = "opencode_nix"
enabled = true

[component.readiness]
kind = "tcp"
host = "127.0.0.1"
port = 4097
interval_secs = 5
failures_threshold = 5
```

**TOML 写法确认**：`[[component]]` 创建数组表元素，紧随其后的 `[component.readiness]` 会挂到**最近一个** `[[component]]` 元素上，这是合法 TOML，不会产生歧义。约束是它必须出现在对应 `[[component]]` 之后、下一个 `[[component]]` 之前；如果跑到数组表之前，语义就变成“给数组键本身加子表”，解析失败。内联写法 `readiness = { kind = "tcp", host = "127.0.0.1", port = 4097 }` 同样可解析，文档统一用子表写法。

**`type = "oneshot"` 必须配探针**：缺少 `[component.readiness]`、或 `kind` 不是 `tcp` / `http` → 解析期拒绝，报 `component_readiness_required`（fail-closed）。`type = "simple"` 可以不配，缺省退回 `kind = "process"` 的进程存活判定。

三种探针：

| kind | 判定 | 适用 |
|---|---|---|
| `process` | 子进程存活（`waitpid` 未返回） | 仅 `type = "simple"`；无端点脚本兜底 |
| `tcp` | 连接 `host:port` 成功 | dsh（3080）、opencode（4097）、drivers 代表端口、snowluma（53001） |
| `http` | `GET http://host:port/path` 返回 2xx | 有健康端点的组件；**v1 不带鉴权** |

**`http` 探针 v1 不带鉴权**：需要 Basic Auth 的服务（如 opencode `/api/info`）v1 先用 `tcp`；等 `ACB_ENV_FILE` / `secrets resolve` 到位后再支持带凭据的探针。**禁止把 token 写进组件配置**（配置会进 store、会被审计）。

**bridge 的探针不在组件表内**：仍是 `[supervisor]` 的 `health_url` + 可选 `health_token_file`（缺省自动读 `$XDG_RUNTIME_DIR/acb-health.token`，见 §1.2 与 `lib.rs:193`）。两套探针 v1 并存，原因是 bridge 探针绑 LKG 提升，合并会牵动升级链；**将来是否统一是明确的未决项**，v1 不做。

**drivers 没有健康端点**，v1 判定方式写死为：

1. 脚本级：`start-drivers.sh` 改成“任一 driver 子进程退出 → 脚本整体退出”，让 acbl 的重启策略能看见单进程死亡（否则 `wait` 永不返回，死一个驱动无人知道）。
2. 组件级：`readiness = tcp`，探**配置里声明的最低 MCP 端口**（如 3101）；没有会话时退回 `process` 存活判定。
3. 不新增 per-driver 组件、不做 per-driver 健康（见 §5.9）。

阈值与 bridge 现有参数的关系：

- bridge：探针 5s、连续 10 次成功才进稳定窗口（约 50s，与 `stable_window_secs = 60` 对齐）；冷启动 23–25s，`startup_grace` 必须 ≥ 30s，推荐 60s。
- 组件：`interval_secs = 5`、`failures_threshold = 5`（即 **5s × 5 = 25s**）；`startup_grace_secs` 默认 60s（覆盖 23–25s 冷启动并留余量）；`success_threshold` 默认 3 次。
- **组件用 5s × 5，与 bridge 的 5s × 10 有意不同**：bridge 的 10 次绑定 LKG 提升，误判代价是回滚；组件没有 LKG、重启便宜，5 次足够，且受全局风暴上限约束。

失败语义：`startup_grace` 内未 ready → `failed`（按 `restart.policy` 处理）；ready 后连续失败达阈值 → `unhealthy` → 按策略重启。

**restart 语义按 `type` 分挂**——这是本契约最容易静默失效的地方：

- `type = "simple"`（**默认**）：脚本必须前台常驻（§5.2 的约束不变：禁 `&`、禁 daemonize、禁自建 session）；「活着」= 进程存在；重启判定挂在**进程退出**上。
- `type = "oneshot"`：脚本是**一次性命令**，允许正常 exit 0（`up -d` 就是这种）；「活着」**只由 probe 判定**；重启判定挂在**探针连续失败**上。**不能用退出码判断 oneshot 的存活**——按 `on-failure` 的老定义会得到“exit 0 → 不重启 → 永不守护”，但容器其实还在后台跑，这是静默失效。
- `type = "notify"`：**预留第三值**（acbl / bridge 已在用 sd_notify），v1 不实现；预留位置就是这个枚举，命名向 systemd 靠拢，避免以后发明新词。

**关键前提**：acbl 记录自己是否主动停止组件（`stop_intent`），因此能区分“我让它停的”和“它自己退出的”。这是 acbl 相对 systemd 的优势，也是 `stopped_early` 能成立的原因。

`simple` 的重启判定挂在进程退出：

| 值 | 行为 |
|---|---|
| `always` | 任何**非 acbl 主动停止**的退出都重拉（含 exit 0）；`max_attempts` 用尽 → `failed` |
| `on-failure`（**默认**） | 非 0 退出 / 被信号杀死 → 重拉；**exit 0 且非 acbl 主动停止 → 不重拉，进入 `stopped_early`，并在 `component status` 可见**；`max_attempts` 用尽 → `failed` |
| `never` | 只在 acbl 启动时拉起一次；之后退出只记状态，不重拉 |

`oneshot` 的重启判定挂在探针：

| 值 | 行为 |
|---|---|
| `always` | **探针连续失败**（5s × 5 次）即重跑脚本，受 `max_attempts` 约束；脚本 exit 0 不算失败 |
| `on-failure` | 与 `always` 在本契约里**没有实际差异**：oneshot 不看退出码、只看探针；acbl 主动停止对两者都抑制后续拉起（与 systemd `stop` 一致）。保留该值只为配置语义一致；要“停止干预”只能用 `never` |
| `never` | 只在 acbl 启动时跑一次；之后无论探针如何都不再干预（snowluma 用这一档） |

**与 systemd 的有意偏离（刻意的，不是疏忽，不要“修正”）**：systemd 对 `Type=oneshot` **忽略 `Restart=`**；本契约**不忽略**，`restart` 在 `oneshot` 下依然生效，判定挂在探针连续失败上。理由：oneshot 组件唯一的存活依据是探针——`up -d` 之后容器在后台跑、脚本本身早就退出；若跟随 systemd 忽略重启，`podman compose down` 之后就是永久静默死亡。因此这里的 `Restart` 语义与 systemd 不同，后来者不得把它改回“忽略”。

`stopped_early` **只属于 `simple`**：`oneshot` 正常 exit 0 是常态（`up -d` 就该退出 0），不算 early stop，也不产生 `stopped_early`；它的存活只看探针。

- **有界重试**：`max_attempts` 默认 5、`backoff_secs` 默认 `[1,2,4,8,30]`、`backoff_cap_secs` 默认 60；达到上限进入终态 `failed`，写 `component_failed`，不再自动拉起；`acbctl component status` 显示已尝试次数与 `last_error`。
- **默认 `on-failure` 而不是 `always`**：组件不在 LKG 里，崩溃后**没有可回退的版本**；无限重启会把“配置写错 / 二进制缺失”伪装成“服务不稳定”，日志刷满还定位不到根因。有界重试 + `failed` / `stopped_early` 才能把问题停在可诊断的位置。
- **`never` 的语义**：只负责“开头那一下”——acbl 启动时 `spawn` / `up -d` 一次，之后不干预；退出只记状态，探针失败也不重拉。**snowluma 这类 oneshot 用 `never`**：acbl 只负责开头的 `up -d`，容器是否活着归 podman / 用户；由于 `down` 无差别，自动重拉反而可能误伤（§5.10.2）。

**与 LKG 的边界（硬约束，跨章节一致）**：

- 组件失败**绝不触发 78**、**绝不写 `current.json`**、**绝不参与 profile 回滚**、**绝不影响 bridge 的健康判定与 LKG 提升**。
- 组件状态只在 `acbctl component status` 与 `state/components/<name>.json` 里体现；bridge 的 10 次阈值 / 60s 稳定窗口 / LKG 与组件无关。
- **组件版本不纳入 LKG 的后果**：组件升级、组件回滚都不在 v1；组件崩到 `failed` 后 acbl **不代劳**，用户按 `last_error` 与日志自行处理。下次 acbl 启动是否再试由策略决定：`never` 仍只试一次，`on-failure` / `always` 重新计数。

### 5.4 顺序与级联

| systemd | 组件表 | 语义 |
|---|---|---|
| `After=a b` | `depends.after = ["a","b"]` | 只保证启动顺序；a/b 失败不阻止本组件启动 |
| `Requires=a` | `depends.requires = ["a"]` | 硬依赖：a 未 ready 则不启动本组件（`blocked`）；a 停止 / 失败则连带停止本组件 |
| `PartOf=a` | `depends.part_of = ["a"]` | 停止传播：a 停 → 本组件停；不隐含启动依赖 |
| `Wants=a` | `depends.wants = ["a"]` | 软依赖：尽量先起 a；a 失败仍启动本组件（opencode 对 bridge 用这个） |

- 启动按 `after` / `requires` / `wants` 的拓扑序；环 → `component_dependency_cycle`，拒绝启动。
- 停止按启动序的**逆序**；`part_of` 目标停止时级联；每个组件 `stop_timeout_secs` 默认 45s，超时后按 `kill_mode` 杀进程组。
- `type = "oneshot"` 不适用“杀进程组”：`stop_action = "down"` 时执行配置里的 `down`，超过 `stop_timeout_secs` 只记事件、不强杀容器；`leave_running` 时停止是空操作并立即成功（§5.10.2）。
- bridge 是隐式节点：`requires = ["bridge"]` 必须等 bridge ready；`after = ["bridge"]` 只排序。`acb-drivers` = `requires=["bridge"]` + `after=["bridge","dsh"]` + `part_of=["bridge","stack"]`；`acb-opencode-serve` = `wants=["bridge"]` + `after=["bridge"]` + `part_of=["stack"]`。
- `stack` 是伪目标，代表 acbl 自身：`part_of = ["stack"]` 在显式整栈停止时生效；**默认 `systemctl stop acb-launcher` 不触发组件停止**（与 §4.7 的 bridge 独立生命周期一致）。当前实现仍是 owner 模型（launcher 退出连带停子进程），adopt 落地后才是契约态。
- **未决**：依赖目标是否从「`bridge` + `stack` 两者」收窄为「只允许 `stack`」。v1 暂时两者都允许（bridge 是隐式节点、stack 是伪目标）；收窄会改变 `acb-drivers` / `acb-opencode-serve` 的映射写法，待裁定（§2.4.2 / §5.9）。
- launcher 不自启的后果：用户管理器重启后，组件与 bridge 都不会自动回来；部署者必须显式 `start acb-launcher`（§5.8 不把它当作可自动检测的问题）。

### 5.5 孤儿、身份与 adopt

组件复用 §4.7 的 instance / adopt 思路，但**不能假设 bridge 那套已经实现**；`type = "oneshot"` 是例外（无 PID、无进程组、容器归属不可知，见 §5.10.2）。下表是契约 v1 定稿的**目标语义**；P0–P3 仍按 owner 模型运行，P4 才补齐：

| 场景 | 契约目标（v1 定稿） | 当前差距 |
|---|---|---|
| 组件自身崩溃（acbl 在跑） | acbl 按 `restart.policy` 重拉；写 `component_restarting` 事件 | 未实现 |
| acbl 崩溃 / 被重启 | 组件用独立 session 启动，**不随 acbl 退出**；新 acbl 读 `state/components/<name>.json` 的 `instance_id` + 探针，匹配则 adopt | 当前 launcher 退出会连带停 bridge，组件同理会死；adopt 未实现 |
| 端口被未知进程占用 | 标 `conflict`，**不盲起**；只影响该组件，不把整栈打 78 | 未实现 |
| launcher 不在时组件死亡 | 无人拉起（无守护窗口，§4.7.5） | 与 bridge 同 |

身份文件 `state/components/<name>.json`：`schema`、`instance_id`、`pid`、`started_at_ms`、`preset_id`、`config_sha256`、`ports`。**pid 只作诊断**，身份以 `instance_id + config_sha256` 为准；pid 复用不能作为 adopt 依据。

**`type = "oneshot"` 不写进程身份**：没有进程可核对，`instance_id` / `pid` / `started_at_ms` 不适用，也不做 adopt。`state/components/<name>.json` 只记 `last_run_at_ms`、`last_result`、`attempts`、`probe_url`（以及 `phase`），存活以 probe 为唯一依据（§5.3）。

`KillMode=process` 的泛化：

- acbl 的 unit 必须 `KillMode=process`（§4.7.1），否则 systemd stop 会连带杀组件。
- acbl 自己停止组件时，按 `kill_mode = "group"` 杀**组件自己的进程组**，而不是整个 acbl cgroup。
- 组件脚本禁止自建 session；需要多进程的（drivers、opencode `serve &`）必须留在脚本的进程组里，由 `kill_mode = "group"` 统一收尾。

**分两步落地**：先用现有 owner 模型把组件表跑通（组件随 acbl 退出），再做 session 脱离 + adopt；契约从 v1 起就写死“组件生命周期独立于 acbl”，实现分阶段补齐，不能把 owner 模型写成契约。

### 5.6 日志与可观测性

- 每个组件一个 append 文件：`${ACB_LOG_DIR:-<state>/logs}/<name>.log`，保留现在每单元一个文件的排障体验；`[component.log] target = "file" | "journald"`，默认 `file`。
- v1 **不做内置轮转**；交给 logrotate / 外部工具，文档给出推荐配置。理由：轮转涉及保留策略与磁盘配额，属于部署决策。
- 组件日志不混进 `supervisor.log`；acbl 只把生命周期事件写进自己的 journal / 事件流。
- `acbctl component status <name>` 必须直接给出日志路径与最近错误。

### 5.7 状态与审计

- 状态文件：`state/components/<name>.json`（原子写），字段：`phase`（`starting` / `ready` / `unhealthy` / `restarting` / `stopped_early` / `failed` / `stopped` / `blocked` / `conflict`）、`restart_policy`、`attempts`、`pid`、`instance_id`、`restarts`、`last_error`、`ready_at_ms`、`failed_at_ms`、`log_path`；oneshot 额外记 `last_run_at_ms`、`last_result`、`probe_url`。
- **`stopped_early` 只属于 `type = "simple"`**：非 acbl 主动停止的 exit 0。它表示“进程没了，但按 `on-failure` 不重启”，必须在 `component status` 与日志里可见，**不能静默**；这是 acbl 知道自己是否主动停止（`stop_intent`）才成立的状态（§5.3）。`type = "oneshot"` 正常 exit 0 是常态，不产生 `stopped_early`。
- **`failed` 是终态**：`restart.policy` 用尽 `max_attempts`（或 `never` 下首次失败）后进入；**不触发 78、不写 `current.json`、不参与 LKG**（§5.3）。只有 acbl 重启或运维显式重载组件表才重新计数；`acbctl component status` 必须显示 `attempts` 与 `last_error`。
- 事件（写入 §9 的事件流）：`component_starting`、`component_ready`、`component_unhealthy`、`component_restarting`、`component_stopped_early`、`component_failed`、`component_stopped`、`component_blocked`、`component_adopted`、`component_conflict`；`component_restarting` 带 `attempt`，`component_stopped_early` 带 `exit_code`，`component_failed` 带 `attempts` 与 `last_error`。
- `acbctl` v1 只读入口：`component list`、`component status [name]`、`component logs <name>`（打印路径，不流式跟随）。写操作留给控制台长期项（§5.12）。
- 退出码：0 成功；2 未知组件；3 配置文件 / 组件表校验失败。

### 5.8 迁移与防双起（操作顺序保证）

**定论：先搬、后删、最后补注册能力。** 顺序不可交换，尤其不能“先删单元再搬脚本”——那是整栈裸奔，详见本节末尾。

#### 阶段 A：搬（组件先由 acbl 跑通，nix-config 五个定义暂不动）

1. `systemctl --user disable --now dsh-acb acb-drivers acb-opencode-serve`。
2. `systemctl --user mask dsh-acb acb-drivers acb-opencode-serve`；`ss -ltnp` 确认 3080 / 3101 / 4097 无监听。
3. 在 `supervisor.toml` 写 `[[component]]`（先全部 `enabled = false`），用现有 unit 重启 acbl；确认 `deny_unknown_fields` 校验通过。
4. 按 §5.11 顺序逐个 `enabled = true`，每开一个看 `component status` 与日志，再开下一个。
5. **A 的验收（B 的前置）**：三个端口由 acbl 的子进程提供；三个 systemd 组件单元处于 inactive + masked；`acb-bridge.service` / `acb-stack.target` 仍在，但只负责启动 acbl。
6. 回退 A：acbl 组件全部 `enabled = false` 并重启 acbl → `systemctl --user unmask ...` → 确认端口释放后 `enable --now`。

**snowluma 与其它三个组件的不同**：它本来就没有 nix-config 单元，所以没有 `disable/mask` 步骤；“搬”对它等于**首次纳入**——在 A 的其余组件跑通后，先确认 compose 项目当前只有 ACB 使用，再按 §5.10.2 配置 `file` / `compose_bin` / `stop_action`，用 `up -d` 交给 acbl；若已有手工容器，推荐维护窗口先 `down` 一次，避免 `down` 语义误伤。它同样不能在三个组件搬完前先删任何 nix-config 定义。

#### 阶段 B：删（移除 nix-config 的五个定义）

1. 从 nix-config 删除 `acb-bridge.service`、`acb-stack.target`、`dsh-acb`、`acb-drivers`、`acb-opencode-serve` 五个定义，`nixos-rebuild switch`。
2. **B 的验收**：`systemctl --user list-unit-files | grep -E 'acb|dsh-acb'` 为空；`systemctl --user list-units --all | grep acb` 为空；acbl 仍在运行并继续托管桥与组件。
3. 若 B 删除 `acb-bridge.service` 后需要重启主机：**必须人工启动 acbl**（`acb-launcher`），整栈靠它起；这一窗口没有自动拉起，是“launcher 不自启”的已知代价。
4. 回退 B：从 git 恢复 nix-config 五个定义 → `nixos-rebuild switch` → `enable --now`；回退前先停 acbl 的组件，避免双起。

#### 阶段 C：补注册能力（acbl 生成自己的 unit）

1. 实现 §4.9 的 `acbctl unit install/remove/status`；模板来自 acbl 包，不来自 nix-config。
2. `acbctl unit install --scope user --with-install-section 0`（默认不 enable）；`systemctl --user cat acb-launcher.service` 能看到生成标记。
3. **C 的验收**：unit 文件不在 nix store、不在 nix-config 模块产物里；`systemctl --user is-enabled` = `disabled`；显式 `start` 能拉起 acbl；用户管理器重启后整栈不会自动恢复（预期行为）。
4. 回退 C：`acbctl unit remove`；若已 enable 则先 `disable`。不触碰 nix-config。

**配置所有权迁移（§2.4.2）与本节的顺序同构**：acbl 先自持有 `state/supervisor/supervisor.toml` 并跑出等价行为，再删 nix-config 的渲染，最后删 unit 的 `--config`。它不阻塞 A/B/C，但**不能抢跑**——先删 `--config` 等于在启动入口上做无对照实验。

#### 为什么不能先删再搬

- 删掉 `acb-bridge.service` / `acb-stack.target` 后，**没有任何东西会启动 acbl**；删掉三个组件单元后，**也没有任何东西会启动组件**。此时脚本还不在 acbl 侧，整栈从“systemd 托管”直接掉到“无人托管”，用户管理器一重启就全没了（snowluma 今晚已经演示过这个后果，见 §5.10.2）。
- 回退成本也最高：没有 acbl 的组件表可关，只能靠 git 恢复 nix-config + 重新 switch，期间端口可能被残留进程占住，双起风险最大。
- **这里的正确性靠操作顺序保证，不是自动检测能兜住的**：systemd 与 acbl 都可能在对方不知情时启动；v1 不做跨管理器仲裁、不自动杀进程。双起唯一的正证据是**端口 + 身份**：端口被未知进程占用 → 该组件 `conflict`，不启动。
- 迁移不删除 `state/`、不碰 profile / LKG。

### 5.9 不在 v1 范围

- 组件版本不纳入 LKG、不参与回滚；组件升级 / 组件升级请求 / 组件 generation 全部不在 v1。
- 组件自动重启只到“有界重试 + `failed` 终态”：没有组件级回滚、没有版本回退、`failed` 后 acbl 不自动代劳，需用户按日志介入（§5.3 / §5.7）。
- Windows 服务注册（C-5）、容器内 PID 1 语义。
- per-driver 组件化（一会话一组件）、per-driver 健康与重启。
- 控制台写入（§5.12 长期项）、自动轮转、组件级鉴权。
- launcher 自启（明确不做：不加 `[Install]` / `WantedBy`）。
- 自动迁移检测 / 跨 systemd 仲裁。
- snowluma（`type = "oneshot"`）只到“拉起 / 关闭 / 端口探活”：容器日志采集、容器健康检查、per-container 组件化、`down` 之外的容器编排都不在 v1（§5.10.2）。
- bridge 探针与组件探针是否统一：**未决**；v1 保持 `[supervisor]` 的 `health_url` + `health_token_file` 与 `[component.readiness]` 两套并存，合并牵动 LKG 与升级链（§5.3）。
- 依赖目标是否收窄为只允许 `stack`：**未决**；当前 §5.4 同时接受 `bridge`（隐式节点）与 `stack`（伪目标），收窄涉及现有映射改写（§2.4.2）。

**仍不在 v1 的补充（2026-11-08）：**

- acbl 运行时注册 systemd 单元（§4.9）：能力不在组件契约 v1 内交付，作为迁移阶段 C 单独排期（§5.8）。
- nix-config 中五个 ACB 定义的移除：是迁移操作（§5.8 阶段 B），不是本契约的实现项。

### 5.10 内置组件映射（v1）

| 组件 | preset | 依赖 | readiness | restart | 日志 | 备注 |
|---|---|---|---|---|---|---|
| `dsh` | `dsh_nix`（默认 false） | `part_of=["stack"]` | `tcp 127.0.0.1:3080` | `on-failure` | `dsh.log` | 无 bridge 依赖，最简单，先做 |
| `drivers` | `drivers_acb` | `requires=["bridge"]`、`after=["bridge","dsh"]`、`part_of=["bridge","stack"]` | `tcp` 最低 MCP 端口；无会话时 `process` | `on-failure` | `drivers.log` | 执行体在 payload，预设只指向 |
| `opencode` | `opencode_nix` / `opencode_npm` | `wants=["bridge"]`、`after=["bridge"]`、`part_of=["stack"]` | `tcp 127.0.0.1:4097` | `on-failure` | `opencode.log` | 脚本从 payload 移入 acbl 预设；`serve &` → `kill_mode="group"` |
| `snowluma` | `snowluma_oneshot`（`type="oneshot"`） | `part_of=["stack"]` | `tcp 127.0.0.1:53001` | `never` | `snowluma.log` | `file`/`compose_bin` 只来自配置；无 PID adopt、`down` 无差别，见 §5.10.2 |

- `drivers_acb` 是 `source = "acb"`：预设条目随 acbl，执行体 `deploy/start-drivers.sh` 随 payload。理由：它扇出的是 payload 的 harness / driver 实现，与 payload 同版本；其它组件的“怎么起”与 payload 无关。
- `opencode` 的 `opencode_<source>.sh` 必须吸收现有 `deploy/opencode-serve.sh` 的逻辑（端口 4097、密码、`serve &` 的进程组收尾），并显式变量优先、找不到二进制报错（对应 A-1/A-2/A-7）。
- `dsh` 的会话列表、MCP 端口与 preset 名仍由配置 / 库单一来源提供；`start-drivers.sh` 退化为纯扇出执行器。

#### 5.10.1 现有脚本的代码事实（迁移依据）

这些事实决定了 §5.10 的映射，不是可选项：

| 事实 | 证据 | 对契约的影响 |
|---|---|---|
| drivers 是单脚本扇出，会话写死在数组里 | [start-drivers.sh](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/start-drivers.sh#L69-L74)（4 条：`dsh-qas:3101`、`dsh-dev:3103`、`dsh-snowluma-test:3104`、`dsh-group2:3102`） | v1 仍是单组件；**会话表必须迁到配置 / 库**（与 `[[sessions]]` 同源），不留在预设脚本 |
| 每个驱动连 `ACB_ENDPOINT=127.0.0.1:7700` | [start-drivers.sh](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/start-drivers.sh#L85) | `requires = ["bridge"]` 的真实原因：桥没起，驱动连不上 |
| token 由 `acbctl secrets resolve --session` 运行时解析 | [start-drivers.sh](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/start-drivers.sh#L63) | 预设脚本不携带密钥；只注入 `ACB_ENV_FILE` / `ACB_COMPONENT_*` |
| 脚本以 `wait` 收尾，不返回 | [start-drivers.sh](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/start-drivers.sh#L97) | 任一驱动死亡对 acbl 不可见；v1 必须先改成“任一子进程退出即整体退出”（§5.3） |
| dsh 直接 `exec` 前台常驻，端口 3080 | [start-dsh.sh](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/start-dsh.sh#L34) | 天然满足前台契约，readiness 用 `tcp 3080` |
| opencode 用 `serve ... &` + `trap` + `wait`，端口默认 4097 | [opencode-serve.sh](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/deploy/opencode-serve.sh#L21-L60) | 必须 `kill_mode = "group"`；脚本从 payload 移入 acbl 预设 |

#### 5.10.2 snowluma 侧车（rootless podman compose，`type = "oneshot"`，v1 最小形态）

**现状**：`deploy/snowluma-compose.yml` 是原样拷贝，靠 compose 工具手工起；`restart: unless-stopped` 只在容器引擎内部有效。用户管理器重启 / podman socket 丢失后容器消失，桥侧表现为反复重连（今晚实测重试 43 次才连回）。它目前**无人管理**——dsh / drivers / opencode 至少还有 systemd 单元，它连单元都没有。

**v1 形态**：作为 `type = "oneshot"` 组件 + 预设脚本 `snowluma_oneshot.sh`，命令固定为配置里给出的形态：

```text
<compose_bin...> -f <file> up -d     # 启动 / 幂等拉起
<compose_bin...> -f <file> down      # 仅 stop_action = "down" 时
```

- **路径只来自配置**：`[[component]]` 的 `file`，默认空 = 不启用；**禁止写死在预设脚本里**。
- **命令名不假设**：`compose_bin` 必填，按目标机实际写——`["podman-compose"]` 与 `["podman", "compose"]` 是两个不同工具；为空报 `component_oneshot_compose_bin_missing`，找不到报 `component_oneshot_compose_bin_not_found`，**不设默认值、不自动回退、不猜**。
- 预设脚本只做三件事：校验 `file` / `compose_bin`、执行 `up -d`、按 `stop_action` 决定停止动作。**这是 §5.2 前台契约的唯一例外**：oneshot 脚本执行完即退出 0，不常驻、不 `wait`；进程死亡不等于组件失败，状态由端口探活决定。

**能力上限（必须写死，不许包装成更强的承诺）**：

- acbl **无法判断容器是谁启动的**。`up -d` 幂等，重复执行不会产生第二个项目；`down` 会**无差别关闭该 compose 项目**。
- 因此停止语义只能定义为「**按策略关闭**」，**不得声称“只关自己启动的实例”**。`stop_action = "down"` 就是关闭整个项目，包括别人手工起的同名项目。
- **不能做 PID adopt、不能杀进程组、不能读容器内进程身份**；§5.5 的 instance / adopt 对 oneshot 组件不适用。身份只有“配置的 `file` + compose project 名 + 端口”。
- readiness **只能 `tcp`**，当前端口 `53001`；没有 HTTP 健康端点，也不进容器。起没起、能不能连，就是唯一判据。
- `restart.policy` 的语义退化为“策略允许时，端口探活失败就重跑 `up -d`”，不是“重启子进程”；snowluma 默认 `never`，探活失败只记状态、不重拉，避免 `down`/`up -d` 的自动误伤。

**`stop_action` 取舍**：

| 值 | 行为 | 适用 |
|---|---|---|
| `leave_running`（**默认，推荐**） | acbl 停止 / 崩溃 / 重启时不执行 `down`，容器继续运行；下次启动 `up -d` 幂等接上 | 优先保 QQ 侧可用性；acbl 重启是低频但不可预测，`down` + `up -d` 会让容器冷启动并中断会话；`up -d` 本身很便宜 |
| `down` | acbl 显式停止时执行 `down`，项目被关闭 | 需要“停 acbl = 停整栈”的维护窗口；代价是中断 QQ 侧，且会关掉同项目里别人起的容器 |

默认选 `leave_running` 的理由：launcher 崩溃 / 升级重启不是日常路径，但每次 `down` 都会真实打断会话；而 `up -d` 幂等，重启后不需要额外拉起。代价是 acbl 停止后容器可能继续跑——这是有意接受的“acbl 不是容器所有者”。

**权限边界（硬约束，保留）**：

- rootless podman 属于**启动它的那个用户**的命名空间。acbl 只有在**同一 UID、user scope、能访问 `$XDG_RUNTIME_DIR/podman/podman.sock`** 时才能调用；system scope、换用户、在容器内运行 acbl 都**进不去**（`podman ps` 都可能被拒），此时只能报错，**不允许降级到 root podman**。
- acbl 不进容器命名空间、不 exec 进容器、不管卷、镜像与网络；容器实际守护者是 podman，compose 只是前端。
- 停止不靠杀进程组；`leave_running` 时脚本不发任何停止命令，`down` 时只执行配置里那一条。

**定位：过渡方案。** 如果 snowluma 将来提供官方 systemd 单元或 podman quadlet，应换成那个（由 systemd / podman 负责守护），本预设只用于填补“无人管理导致会话重启后侧车消失”的空档。

**与 §5.8 迁移顺序的关系**：snowluma 本来就没有 nix-config 单元，所以“搬”对它等于“**首次纳入**”，没有 `mask` 步骤；但同样受“先搬后删”约束——三个组件没在 acbl 侧跑通前，不把 snowluma 的启动责任从手工 compose 切走。首次纳入前先确认该项目是否只有 ACB 使用；若已有手工容器，推荐在维护窗口先 `down` 一次，再由 acbl `up -d`，避免 `down` 语义误伤无法辨认的实例。

**v1 范围只到“拉起 / 关闭 / 端口探活”**：不做容器日志采集（`podman logs` 仍归用户）、不做容器健康检查、不做版本 / LKG、不做 per-container 组件化。

### 5.11 分阶段实施与验收

| 阶段 | 范围 | 验收标准 |
|---|---|---|
| **P0 解析与校验** | `deny_unknown_fields`、`[[component]]` 解析、`component list/status`；不 spawn | 无 `[[component]]` 时行为与今天一致；未知字段直接报错；不产生任何子进程 |
| **P1 `dsh`** | 预设 `dsh_nix`、`tcp` readiness、`on-failure`、`part_of=["stack"]` | 冷启动 ≤60s ready；kill dsh → 按退避重启；`component status` 与 `dsh.log` 可用；停 acbl 后 dsh 按当前 owner 模型退出 |
| **P2 `drivers`** | `requires` / `after` / `part_of`、`kill_mode="group"`、扇出脚本“任一子进程退出即整体退出” | bridge 未 ready 时 `blocked`；bridge 起来后启动；kill 任一 driver → 整体重启；无孤儿 |
| **P3 `opencode`** | 脚本移入 acbl 预设、`serve &` 分组收尾、`wants` bridge | 4097 被占 → `conflict` 不盲起；停 acbl → opencode 停止；重启后不出现双实例 |
| **P4 adopt** | 组件 session 脱离 + 身份文件 + adopt / conflict | kill acbl → 组件继续跑；新 acbl adopt，不产生第二个；与 §4.7 的 bridge adopt 同批验收 |
| **P5 snowluma（v1 oneshot）** | `type = "oneshot"`、`file` / `compose_bin` / `stop_action`、`up -d` / `down`、`tcp 53001`、`restart.policy = "never"` | `file` 为空不启用；`compose_bin` 缺失报错且不猜；`leave_running` 与 `down` 两种策略行为符合定义；停 acbl 不误伤；探活失败只记状态、不自动重拉 |

每阶段都**不**改 LKG / 升级链；组件 `failed` 不触发 78，也不写 `current.json`。P0–P4 全部验收通过，是 §5.8 阶段 B（删 nix-config）的前置；P5 属 v1，但没有 nix-config 依赖，可与 B 并行。

**当前实现进度**：P1 已落地——`[[component]]` 全字段解析、`deny_unknown_fields`、默认值（探针 `5s × 5`、`restart` 默认 `on-failure`、`max_attempts = 5`、退避 `1/2/4/8/30`）、`acbctl component list/status` 只读、空表零回归（§2.4.2）。

### 5.12 控制台：短期只读，长期受限编辑

**短期（v1）：只读展示。** 展示预设集合（`presets.toml`）、当前启用项（`supervisor.toml`）、状态与日志路径（`state/components/<name>.json`）。

**长期：允许编辑，但边界写死。** 只允许三类动作：

1. 从预设集合里选择（只能选 `presets.toml` 登记的 `id`）；
2. 启用 / 停用组件；
3. 覆盖白名单字段（如 `env` 允许的 key、`cwd`、`readiness` 的 `host` / `port` / `interval_secs` / `failures_threshold`），逐项校验。

明确**不允许**：任意命令 / 脚本路径 / 上传脚本；新增预设（只能走仓库 PR）；修改 `[supervisor]`、profile、LKG、升级请求；修改密钥与环境文件内容；修改业务策略、systemd unit、日志路径、acbl 启动方式。

长期编辑的前置：鉴权（管理员 token + 角色校验，未授权 403）、二次确认（展示 diff）、变更审计（谁/何时/旧值/新值/结果，密钥脱敏）、服务端校验（多来源互斥、白名单、脚本存在性、变量可替换）、失败回滚（校验失败不动运行组件，应用失败回滚旧表）、并发控制（同一时间一个编辑 / 重载任务）。

### 5.13 取舍汇总

| 取舍 | 选择 | 被否方案 | 理由 |
|---|---|---|---|
| 组件表位置 | `supervisor.toml` 的 `[[component]]` | 独立 `components.toml` | 一个文件一个写者；启用状态与 bridge 配置不会分叉 |
| 组件生命周期归谁 | acbl 托管，systemd 只启停 acbl | systemd 直接管三个组件 | 两套生命周期与 `PartOf` 靠猜，停止 / 孤儿 / 双起无法统一 |
| drivers 粒度 | 单组件管 N 个驱动进程 | 一会话一组件 | 共享同一份输入，拆开无独立重启收益；映射与健康成本高 |
| opencode 脚本位置 | 移入 acbl 预设 | 留在 payload | “预设随 acbl”是结构前提；B-12 已把它移出 payload 必带项 |
| launcher 自启 | 不自启，显式 start | enable / `WantedBy` | 用户明确不自启；代价是管理器重启后整栈不自动恢复，已在 §5.0/§5.8 写明 |
| 组件失败阈值 | 5 次 × 5s | 复用 bridge 的 10 次 | 组件没有 LKG，重启廉价；10 次只属于 bridge 的稳定窗口 |
| orphan 语义 | 先 owner 模型跑通，再 session 脱离 + adopt | 一次性做完 adopt | P1/P2 不被 P4 阻塞；契约先写目标态，实现分阶段 |
| 迁移防双起 | 操作顺序保证 + 端口 / 身份冲突检测 | 自动检测 / 自动杀进程 | 跨 systemd 与 acbl 的仲裁不可靠，自动杀进程风险更高 |
| nix-config 职责 | 只提供 `acbl` 包，不声明任何 ACB 单元 | 继续在 nix-config 声明 service / target | 用户硬约束；两套来源必然分叉（§2.4.1） |
| unit 来源与时机 | 来源归 acbl（运行时生成），时机归部署者（默认不 enable） | 写死 nix-config + 随 switch 生效 | 来源与时机是两件事，混在一起就无法“装了但不自启”（§4.9） |
| 迁移顺序 | 搬 → 删 → 补注册能力 | 先删再搬 / 一步到位 | 先删 = 整栈裸奔，回退成本最高（§5.8） |
| snowluma 侧车 | v1 纳入，`type = "oneshot"`，只到拉起 / 关闭 / 端口探活 | 继续手工 compose 或等官方 unit | 填补无人管理空档；但 acbl 无法判断容器归属、`down` 无差别，能力上限写死（§5.10.2） |
| 组件自动重启 | `restart.policy` 默认 `on-failure` + 有界 `max_attempts` → `failed` | 默认 `always` / 无限重试 | 组件不在 LKG，崩溃无可回退版本；无限重启会把配置错误伪装成不稳定（§5.3） |
| oneshot 的重启 | snowluma 用 `never`，只负责开头一次 `up -d`；`always`/`on-failure` 在本契约无差异 | `always` 自动重跑 `up -d` | `down` 无差别，自动重拉可能误伤别人起的项目（§5.10.2） |

---

## 6. 与 nix、升级链的关系

### 6.1 nix 路线：先独立构建，成功再替换

- `nix build .#acb-payload -o result` 在独立目录产出；成功后再 `profile add` → `profile switch`，失败不动当前 `current.json`。这与现有 `profile` 的 staging + 原子 rename 一致。
- ACB **不调用 `nix profile`**：`store` 策略用 gcroot pin + `/nix/store` 软链，`copy` 策略用本地快照。是否另外用 `nix profile install` 管理你自己的工具链，是用户的选择，ACB 不依赖它。
- 如果未来要支持“nix profile 作为产物来源”，应作为新的策略（例如 `profile-nix`），不和现有 `store` / `copy` 混在一起。

### 6.2 升级链只管 payload 里的 bridge

现有 `docs/plan/acb-upgrade-chain.md` 的 mailbox、`current.json`、LKG 只覆盖 bridge，而且**升级不再重启 acbl**：

- 升级动作全部发生在 acbl 进程内：停旧 bridge → 提交指针 → 起新 bridge。没有“退出 75 等外部重拉”这一步。
- 预设组件没有 generation、没有 LKG、没有升级请求；
- 组件的版本由用户自己管理（nix / npm / 包管理器）；
- 升级链不因为“组件表变了”而触发；组件表改动可通过控制台“重新加载组件表”按钮热加载（§4.6），或重启 acbl 生效。

### 6.3 acbl 独立安装：unit 指向静态入口

unit 的 `ExecStart` 必须指向**系统层安装的 acbl**，不能指向 `profile/current/bin` 里的二进制：

- **A-8 已实现**：默认 `ACB_UNIT_ACBL_PREFIX=/usr/local` → `$prefix/bin/acb-launcher`；也接受显式 `ACB_UNIT_LAUNCHER_BIN`，但拒绝落在 `profile/generations/` 的路径。`acbctl unit doctor` 仍会检查 `ExecStart` 程序存在。
- 部署校验：`acbctl unit doctor` + `test -x <静态入口>`，并额外确认 `ExecStart` 路径不在 `profile/generations/` 下。
- payload 不再需要 `acbctl` / `acb-launcher` / `acb-supervisor`；`profile seed` 的必带清单已由 B-12 相应收窄（G14）。
- **已知未改（C-9 硬前置）**：launcher unit 的 `WorkingDirectory` 仍沿用 `@ACB_UNIT_REPO_DIR@`，bridge 子进程继承该 cwd。A-8 不动它，避免改变 bridge 的运行时 cwd；打包前必须完成 §6.5.1 的审计，把所有持久化路径改为从显式配置的根推导，与 cwd 解耦。

### 6.4 acbl ↔ payload 契约版本化（B-12 已实现）

契约的声明权威是 **payload 根目录的 `acb-contract.json`**，不是 acbl 生成的 `meta.json` /
`manifest.json`：只有 payload 自己知道它实际包含什么、需要多新的 acbl。`meta.json` 只记 acbl
侧的 generation 元数据。实现见 `crates/acb/src/contract.rs`，参考模板 `deploy/acb-contract.json`。

```json
{
  "schema": 1,
  "contract": 1,
  "min_acbl": "0.1.0",
  "layout": [
    "bin",
    "bin/acb-bridge",
    "harnesses",
    "harnesses/dsh/src/driver.mjs",
    "harnesses/dsh/dsh-mcp.patch.yml",
    "harnesses/opencode/plugin/index.js",
    "harnesses/opencode/plugin/acb-wire.mjs",
    "harnesses/shared/tools.mjs",
    "deploy/start-drivers.sh"
  ],
  "commands": [
    { "name": "bridge", "program": "bin/acb-bridge", "args": ["--config"] }
  ]
}
```

| 字段 | 含义 | 校验与失败原因码 |
|---|---|---|
| `schema` | 契约文件自身的格式版本 | 必须为 `1`，否则 `contract_schema_unsupported` |
| `contract` | acbl ↔ payload 接口版本 | acbl 只支持 `1..=1`（v1）；越界 → `contract_incompatible` |
| `min_acbl` | payload 需要的最低 acbl 版本（点分数字；acbl 三个二进制同版本） | 高于当前 acbl → `acbl_too_old`；非数字 → `contract_malformed` |
| `layout` | payload 自述包含的相对路径 | 声明了却不存在 → `payload_missing_path`；绝对路径或 `..` → `contract_malformed` |
| `commands` | payload 自述的命令集合 | 必须含 `bridge`（缺失 → `payload_missing_command`），程序为 `bin/acb-bridge` 且首参 `--config`（否则 `payload_bad_command`）；命令程序不存在 → `payload_missing_path`，路径非法 → `contract_malformed` |

其余原因码：`contract_unreadable`（契约文件读不了或 JSON 解析失败）。

**固定必带项**（由 `profile` 独立校验，不依赖声明）：`bin/`、`bin/acb-bridge`、`harnesses/`、
`deploy/start-drivers.sh`、`harnesses/dsh/src/driver.mjs`、`harnesses/dsh/dsh-mcp.patch.yml`、
`harnesses/opencode/plugin/index.js`、`harnesses/opencode/plugin/acb-wire.mjs`、
`harnesses/shared/tools.mjs`。**不再校验** `deploy/units/systemd/`、
`deploy/run/supervisor.toml.tmpl`、`deploy/start-dsh.sh`、`deploy/opencode-serve.sh`：这些已
剥离到 acbl 或已废弃。缺项逐条列出 `<相对路径> (file|directory)`。

**校验点（写实）**：

1. `profile add`：契约不兼容 / `min_acbl` 过高 / 缺布局条目 → **拒绝 add，不产生 generation**，
   错误里带原因码与声明值。
2. `profile switch` 与升级预检（`resolve_generation_command`）：**在 pin 与停旧进程之前**校验；
   失败 → 拒绝切换，保留当前 generation，不进入 staging、不写指针。
3. launcher 启动读到 `current.json` 指向的 generation：**在 supervisor 拉起 bridge 之前**校验；
   失败打印 `acbl-launcher: refusing to start generation: generation <id>: reason_code=...`
   并退出 `78`，不会出现“旧 bridge 已停、新 bridge 未起”。

**兼容规则（v1）**：

- 只增不改：新增**可选**字段或可选文件不升 `contract`；删除、改名、语义变化必须升。
- acbl 固定支持 `1..=1`；`N` 与 `N-1` 的并行窗口等 v2 出现时再开。
- `presets.toml` 的 schema 版本与 `contract` 独立。
- **`bridge.json` 与 `GET /internal/instance` 不在契约 v1 里**（B-12 只落声明与校验，bridge 侧
  身份端点尚未实现）。它们是 adopt 落地后的新增项：若做成必需字段，按“删除 / 改名 / 语义
  变化”规则升 `contract`；若作为可选能力，可作为新增可选字段不升版本。归属留给 adopt 落地
  时定（§4.7.4）。
- **旧 payload 没有 `acb-contract.json` 时**：实现按 `contract = 1` 过渡放行，并在每个校验点打两条 `WARNING`。这是过渡兜底、也是旁路（删除契约文件即可回到 legacy 放行）；设置 `ACB_CONTRACT_STRICT=1` 后缺文件按 `contract_unreadable` 拒绝（默认关闭）。payload 打包链路稳定产出契约、现场 generation 全部带文件后，再把严格模式改为默认开启。


### 6.5 两条升级路径对照

| 路径 | 触发 | 换什么 | 重启谁 | 频率 | 走升级链吗 |
|---|---|---|---|---|---|
| payload 升级 | `acbctl upgrade` / 控制台 / `profile switch` | generation 里的 bridge、harness、drivers 实现 | 只重启 bridge（acbl 原地） | 高频 | 是 |
| acbl 升级 | `nixos-rebuild switch` / 包升级 / 替换二进制 | acbl 自身、预设、unit 模板 | 重启 acbl；bridge 不重启（新 acbl adopt），预设组件随 acbl 重启 | 低频 | **否** |

这直接回答“守护者为什么需要重启”：**它不需要因为 payload 换代而重启；只有在它自己的版本/启动方式变了时才重启**。

### 6.5.1 C-9 硬前置：bridge 的 cwd / 相对路径审计

unit 的 `WorkingDirectory` 会原样传给 bridge 子进程（launcher 不另设 `current_dir`）。打包 acbl 到无仓库环境前，必须按下面的代码事实逐项审计；**不能只写“需要改”**。

**已知依赖（代码事实）**：

| 依赖点 | 位置 | 相对 cwd 的行为 |
|---|---|---|
| `[storage].path` 默认 `state/acb.db` | `crates/kernel/src/config.rs:33-35`、`271-273`；`config.example.toml:113-115` | 注释明确“相对路径按启动时的工作目录算” |
| `[file_relay].dir` 默认 `state/relay` | `crates/kernel/src/config.rs:52-53`、`566` | 收文件落盘位置随 cwd |
| `[memory].worktrees_root` 默认 `state/worktrees` | `crates/kernel/src/config.rs:55-58`、`103`；`memory.rs:65-66` | `ensure_bare_repo(location)` 直接 `PathBuf::from`，相对值按 cwd 解析 |
| `kernel.task` 工具参数 `cwd` | `crates/kernel/src/task.rs:57-84` | 相对值按 bridge cwd 解析；`create_dir_all` 同样相对 |
| 组件 / 适配器 `path`（`.so`、wasm host、脚本） | `config.example.toml:81`、`85`、`129` | 示例写“绝对路径”，但未见启动时强制校验；相对值会落到 cwd |
| drivers / harness 脚本 | `deploy/start-dsh.sh`、`deploy/opencode-serve.sh`、`start-drivers.sh` | 脚本自己 source env、用 `REPO` 绝对路径；若配置里给相对脚本路径，仍按 cwd 解析 |

**审计结论（C-9 硬性口径）**：

- **原则：cwd 只是启动位置的偶然产物，不得用于推导任何持久化路径。** 所有持久化路径（`storage`、`file_relay`、`memory.worktrees_root`、组件与适配器 `path`、drivers / harness 脚本路径等）必须从一个**显式配置的根**推导，与 cwd 无关。
- 具体做法：配置新增/复用显式的根（例如 `ACB_STATE_ROOT` 或 `[paths].root`），未配置时取安装时确定的绝对默认值；解析出的路径必须是绝对路径。`state/acb.db` 这类相对默认值要么改为从根拼出，要么在启动时直接报错，不再静默按 cwd 解析。
- `acbctl unit doctor` 增加兜底检查：配置里出现相对持久化路径、或路径落在 payload/generation 内时报错；这只作为兜底，不能替代显式根。
- **不采用方案 B（把 `WorkingDirectory` 换成状态根）**：它会把 `state/acb.db` 变成 `<状态根>/state/acb.db`，多套一层；同时改变现网已有的 `acb.db`、`relay/`、`worktrees/` 位置。这种变更不能静默切换，必须搬移数据或保留兼容期，代价和风险都高于“显式根推导”。保留 `WorkingDirectory` 现状也不意味着允许它参与路径推导。
- **验收**：`readlink /proc/<bridge-pid>/cwd` 只作诊断；从 `/`、`/tmp`、payload 根分别启动 bridge，db / relay / worktrees / 组件路径的落点必须完全一致，且不落在 payload 或被只读挂载挡住；`kernel.task` 传相对 `cwd` 时仍以显式配置的根解析；doctor 能拒绝含相对持久化路径的配置。
- **状态**：A-8 只记录、不改 `WorkingDirectory`；本审计是 C-9 的硬前置，未完成前不得发布非仓库环境的 acbl 包。

---

## 7. 落地拆分（只列步骤，不改代码）

| 步骤 | 内容 | 依赖 | 验证 |
|---|---|---|---|
| P0 | **拆分分发**：acbl 独立包（NixOS 由 nix-config 只提供包）+ payload 收窄 + `acb-contract.json` 声明与校验（B-12 已实现） | 无 | acbl 不在 generation 里；unit 指向静态入口；不兼容 payload 被拒绝切换；旧 payload 在过渡期仍能启动 |
| P1 | 组件表 schema + `supervisor.toml` 解析（bridge 之外的 `[[component]]`） | P0 | 配置文件能加载；非法字段报错；不启动任何新组件时行为与现在一致 |
| P2 | supervisor 多组件 Runner：每组件独立 `Option<Child>`、进程组、独立停止 | P1 | 起 2 个假组件，kill 一个不影响另一个；脚本后台孙进程随组停止 |
| P3 | 每组件健康探针 + 退避 + 独立状态文件 | P2 | 组件失败重试到上限后标 failed，acbl 不退出；`state/components/*.json` 可读 |
| P4 | 预设清单 + 多来源装载 + 互斥 + 覆盖规则 | P1 | 同一组件两个来源同时启用会被拒绝；启用/禁用/覆盖生效；用户自写组件不用 preset 也能跑 |
| P5 | 预设脚本去 nix 默认路径（dsh/drivers/opencode，含按安装方式拆分的脚本）；drivers 会话列表改从配置/库读取，脚本退化为纯扇出 | P4 | 非 NixOS 上按变量能找到二进制；缺变量时明确报错；改会话只改配置、不改脚本 |
| P6 | `acbctl status` 展示组件表；日志分流；控制台只读展示 | P3 | 状态里能看到每个组件的 pid/健康/重启次数/最近错误 |
| P7 | systemd 适配器与自定义 init 文档对齐（三种启动方式） | P2 | 78 仍由外部单元识别；升级不再用 75；组件失败不触发 75/78；安装器参数覆盖 A/B/C |
| P8 | 控制台“重新加载组件表”按钮 + 校验/回滚语义 | P1 P4 | 非法新表不动运行中组件；变更失败回滚；返回逐组件结果并写审计 |
| P9 | acbl 运行时 unit 注册（§4.9）：`acbctl unit install/remove/status`，生成标记、漂移拒绝、默认不 enable | P7 | unit 由 acbl 生成且不在 nix store；重复 install 幂等；无标记同名 unit 报 `unit_conflict`；`is-enabled` 保持 disabled |
| P10 | nix-config 清理（操作，非代码）：按 §5.8 阶段 B 删除五个定义 | P0–P4 全部验收 + P9 可用 | nix-config 仅剩 `acbl` 包；`list-unit-files` 无 ACB 单元；整栈由 acbl 托管 |

---

## 8. 决策与仍待决

**已定：**

1. **acbl 独立安装**：不进 payload；NixOS 由 nix-config 引用 flake 的 `acbl` package，非 NixOS 用 nix profile / cargo / 预编译 tar.gz（gnu 或 musl，§2.4 / §2.5 / §2.5.7）。
2. **payload 升级不重启 acbl**：只切 payload、原地重启 bridge；acbl 自身的升级走系统层，只重启 acbl，bridge 继续运行并被新 acbl adopt（§4.2 / §4.7 / §6.5）。
3. **75 退役、78 保留**：75 只作为兼容保留值，不再由升级路径发出；78 表示“现场坏了、需要人”（§4.2）。
4. **`restart_owner` 取消**：不再作为升级门禁；`self` 档取消，改 `managed_by` 诊断（§4.3）。
5. **契约版本化（B-12 已实现）**：声明在 payload 根 `acb-contract.json`；acbl 只支持 `contract = 1`，三处校验点分别拒绝 add / 拒绝切换 / 启动退 78（§6.4）。
6. **谁启动 acbl**：三类并列，安装器参数选择——A 用户终端手工、B systemd 开机自启、C systemd 只守护不自启；无 systemd 由用户自管（D）。差异见 §4.3，接口用 `ACB_MANAGED_BY` / `ACB_UNIT_INSTALL_SECTION` / `ACB_UNIT_SCOPE` / `ACB_LOG_TARGET` 预留。
7. **热加载**：不做自动热加载；只做控制台按钮，加载对象是 `[[component]]`，失败保留旧表（§4.6）。
8. **drivers**：v1 作为单个组件（`drivers_acb`），理由是代码事实（§5.10.1）；单驱动死亡要通过“脚本整体退出”暴露给 acbl。**会话列表必须从配置/库读取（与 `[[sessions]]` 同一数据源），不得写死在 preset 脚本里**。
9. **控制台**：短期只读；长期允许“选预设 / 开关 / 白名单字段”，不允许任意命令，前置条件见 §5.12。
10. **bridge 生命周期独立于 acbl**：acbl 是观察者 + 恢复者；acbl 停止 / 崩溃 / 升级期间 bridge 继续运行；重启后先探测身份再 adopt，禁止盲起；systemd unit 用 `KillMode=process`（§4.7）。
11. **nix-config 只提供 acbl 包**：不得声明任何 ACB 相关 service / target / timer；五个现有定义全部移除（§2.4.1）。unit 由 acbl 运行时生成（§4.9），**安装与 enable 分离**，默认不 enable。
12. **迁移顺序固定为“搬 → 删 → 补注册能力”**：先让组件在 acbl 侧跑通，再删 nix-config，最后落盘 acbl 自己的 unit；禁止先删后搬（§5.8）。
13. **snowluma 侧车进 v1，但只做最小形态**：`type = "oneshot"` + 预设脚本，`file` / `compose_bin` 只来自配置，`stop_action` 默认 `leave_running`；能力上限是“拉起 / 关闭 / 端口探活”，不做 PID adopt、不声称只关自己启动的实例（§5.10.2）。
14. **组件重启默认 `on-failure` + 有界重试**（`max_attempts`、退避上限，用尽进 `failed` 终态），不是 `always`；`never` 只负责 acbl 启动时拉一次；组件失败不触发 78、不写 `current.json`、不参与 LKG（§5.3 / §5.7）。

**仍待决：**

1. **acbl 与 payload 的默认分发形态**：非 nix 环境仍是预编译 `tar.gz`（gnu / musl）的构建流水线、校验和与签名（C-4 / C-9）；nix 包已实现（§2.5.7）。
2. **默认推荐哪种启动方式**：三类都支持，但安装器默认给 A / B / C 哪一个？（建议 B，但需要用户确认是否接受 linger 与 journald）。
3. **系统级 vs 用户级**默认 scope，以及系统级 `User=` 是否由安装器强制生成。
4. **`ACB_CONTRACT_STRICT` 何时改为默认开启**：过渡放行保留到 payload 打包链路稳定产出契约、现场 generation 全部带文件为止（§6.4）。
5. **每会话拆组件**：drivers 何时从单组件拆成“每会话一个组件”，取决于是否需要单独重启某个会话。
6. **npm / guix / scoop 的预设脚本谁补**：v1 可以先只交付 `dsh_nix` / `opencode_nix` / `opencode_npm` / `drivers_acb`，其余欢迎 PR。
7. **控制台长期编辑的落地时间**：鉴权、二次确认、审计都具备后再开写入。
8. **Windows 上的进程组与信号**：没有 `setsid`/进程组语义，需要 Job Object 或“包装器负责杀整棵树”；在 C-5 里一起处理。
9. **无守护窗口是否另设看门狗**：当前接受“acbl 停机期间 bridge 死亡无人拉起”（§4.7.5）；是否引入 systemd 直接托管 bridge 或独立看门狗，未决策。
10. **aarch64 制品是否与 x86_64 同期产出**：C-9 默认只出 `x86_64-unknown-linux-{gnu,musl}`（§2.5.1）。

---

## 9. 一句话结论

**acbl 是独立安装的静态监管者，payload 是它监管的迭代内容：升级只切 payload、只重启 bridge，acbl 全程不动。** 因此不需要 `restart_owner`，也不需要“退出 75 等外部重拉”；75 退役为兼容值，78 保留。要变成组件管理者，缺的仍是多组件表、进程组、每组件健康/重启策略和预设装载；预设按安装方式拆分并随 acbl 安装，同一组件只能启用一个来源。代价是 acbl↔payload 的接口必须版本化：payload 根 `acb-contract.json` 声明 `contract` / `min_acbl` / `layout` / `commands`，acbl 只支持 `1..=1`（B-12 已实现），否则两个独立分发单元会悄悄失配。

**acbl 还是 bridge 的观察者与恢复者，不是所有者：** bridge 的生命周期独立于 acbl，acbl 停止 / 崩溃 / 升级期间 bridge 继续服务，新 acbl 启动先按身份端点探测再 adopt，禁止盲起第二个实例；无守护窗口是被接受的代价（§4.7）。分发上分两条路：nix 环境用本仓库 flake 的 `acbl` package（§2.5.7），非 nix 环境出 `tar.gz`——gnu 制品钉死最低 glibc 基线，musl 制品里的 acbl 全静态，但 **bridge 在 musl 上必须是动态链接**（`.so` 插件靠 `dlopen`，静态 musl 不支持动态加载），所以 acbl 与 payload 必须按同一 libc 家族配对交付（§2.5）。

**nix-config 的边界同时收紧：只提供 `acbl` 包，不声明任何 ACB 单元。** 现有 `acb-bridge.service`、`acb-stack.target`、`dsh-acb`、`acb-drivers`、`acb-opencode-serve` 五个定义最终全部删除；acbl 新增运行时 unit 注册（§4.9），但“生成 unit”与“enable”分离——默认只落盘不启用。迁移只能按“搬 → 删 → 补注册能力”的顺序，先删后搬等于让整栈失去所有启动者（§5.8）。
