# 部署组合矩阵：acbl 为中心，nix / systemd 可选

这一页回答两个问题：

1. **ACB 自己保证的那部分（独立安装的 acbl + payload 里的桥 + profile 机制）在什么系统上、怎么装、怎么起。**
2. **ACB 之外的组件（dsh / opencode / snowluma / 模型端点等）由谁提供、怎么配。**

先读 [§一 部署定位](#一部署定位先读)：这一轮把编排权明确交给了 launcher，nix 与 systemd 都降为“可选、由用户显式指定”的适配层。旧文档里“systemd 拉整栈”的写法不再推荐。

**拓扑前提（本轮新增）：**

- **acbl 独立安装，不进 payload。** 它是静态入口：unit / init 指向一个稳定路径（NixOS 上由 nix-config 提供与维护，随系统 switch 更新），不指向 generation。
- **payload 只装会迭代的内容**：`bin/acb-bridge`、harness 适配层、drivers 扇出脚本、插件。
- 因此**升级只切 payload、只重启 bridge**；acbl 全程不动。代价是 acbl↔payload 的接口要版本化（`acb_contract`），见 [acbl-component-manager.md](../plan/acbl-component-manager.md) §6.4。

判定口径，避免把“设计上应该行”当成“已经验证”：

- **跑起来（ACB 部分）** = launcher 进程存活、桥被它拉起、`acbctl status --mode=run` 正常、控制台端口可访问。
- **整栈跑起来** = 上面的 ACB 部分 + 用户自备的 harness / 协议端也连通。
- **已验证** = 在实机上按本文步骤跑过。
- **可照做（未验证）** = 代码路径和命令都存在，但本文没有在实机上完整跑过，第一次按它操作时请按“首次验证”对待。
- **取决于用户组合** = ACB 侧没有阻塞，整栈能否跑通取决于用户如何安装外部组件。
- **不完整** = ACB 自己保证的部分就有缺口，不建议作为正式部署。

Windows 不在八格内，当前完全未覆盖，见 §六 缺口清单 C-5。

---

## 一、部署定位（先读）

### 1.1 四层职责

| 层 | 定位 | 可选性 |
|---|---|---|
| **acbl**（`acb-launcher` / `acbctl` / `acb-supervisor`） | **唯一入口与监管者**。负责拉起 bridge、健康探针、失败回退（LKG）、升级链消费、状态目录管理。**独立安装，不在 payload 里**；NixOS 由 nix-config 提供，随系统 switch 低频更新 | 必需；安装来源可变（nix-config / nix profile / cargo / 包） |
| **payload**（generation） | **迭代内容**：`bin/acb-bridge`、harness 适配层、drivers 扇出脚本、插件，以及契约声明 | 必需；由 ACB 构建 |
| **systemd** | **启停适配器**。`acbctl unit install` 只生成“把 acbl 挂到 systemd 上”的单元，不承担编排 | 可选，由用户指定 |
| **nix** | **产物与快照来源**。只决定 `profile` 用 `store`（软链 `/nix/store` + gcroot）还是 `copy`（本地快照） | 可选，由用户指定 |

三条硬规则：

- **显式指定，不做环境探测。** 不会因为检测到 `/nix/store` 就自动切 `store`，也不会因为检测到 systemd 就自动注册单元。用不用、用哪个，写在命令和参数里。
- **acbl 与 payload 分开升级。** payload 由升级链换代；acbl 由系统层（`nixos-rebuild switch`、包升级、人工替换）换代。**升级链永远不替换 acbl**。
- **外部组件不在 ACB 职责内。** dsh / opencode / snowluma / node / 模型端点都由用户自己安装与托管，ACB 不托管、不探测、不承诺开箱即用。见 §五。

### 1.2 推荐形态（acbl 主导）

```
用户 init / systemd 适配器
        │  只启动一个东西：静态入口上的 acbl
        ▼
   acbl（独立安装：nix-config / nix profile / cargo / 预编译 tar.gz）
        │  读 current.json
        ├──► payload generation 里的 acb-bridge（核心组件，带升级链 / LKG）
        ├──► 可选预设组件：dsh / drivers / opencode（用户启用，acbl 托管）
        └──► 状态：current.json / lkg.json / supervisor.toml / components/*.json

payload 换代（bridge / harness / drivers）──── 升级链负责，只重启 bridge
acbl 换代（二进制 / 预设 / unit）────────── 系统层负责，只重启 acbl；bridge 继续运行
外部组件的本体（dsh / opencode / node 的安装）──── 用户自有
```

systemd 形态下的正确写法：

```sh
systemctl --user enable --now acb-launcher-user.service   # 唯一入口
```

- launcher 单元由 `acbctl unit install` 生成，`ExecStart` 指向**系统安装的 acbl 静态入口**：`ACB_UNIT_ACBL_PREFIX`（默认 `/usr/local`）→ `$prefix/bin/acb-launcher`；A-8 已实现，不再从 `profile/current/bin` 派生。也可用显式 `ACB_UNIT_LAUNCHER_BIN`，但落在 `profile/generations/` 内的路径会被拒绝。NixOS 上 `/usr/local` 不可写，必须由 nix-config 显式改成 `/run/current-system/sw/bin/acb-launcher`。
- **bridge 归 acbl 管**，bridge 的二进制来自 payload generation；dsh / drivers / opencode 的启动脚本由 ACB 提供可选预设，启用后也由 acbl 托管（当前尚未实现，见缺口 B-7~B-10）；不启用时用户用自己的 unit / 进程管理器负责。
- 升级链（`acbctl upgrade` 写请求信箱 → acbl 消费 → `profile switch` + 原地重启 bridge）只在 acbl 主导时成立，且只覆盖 bridge。**payload 换代时 acbl 不重启；acbl 自己换代时只重启 acbl，bridge 继续运行并由新 acbl 接管（adopt）。**
- **bridge 的生命周期独立于 acbl**：acbl 停止 / 崩溃 / 升级期间 bridge 继续服务；新 acbl 启动先按身份端点探测再 adopt，禁止盲起第二个。systemd unit 必须是 `KillMode=process`，否则 `stop` 会连带杀掉 bridge（设计见 [acbl-component-manager.md](../plan/acbl-component-manager.md) §4.7）。
- 组件管理器的设计见 [acbl-component-manager.md](../plan/acbl-component-manager.md)。

### 1.3 不符合新定位的形态（legacy，仅兼容保留）

`acb-stack.target` 目前是：

```ini
Wants=acb-bridge.service acb-drivers.service dsh-acb.service acb-opencode-serve.service
```

这是**“systemd 当总指挥”**的旧拓扑：它绕过 acbl 直接拉起桥和外部组件，等于放弃升级链、LKG 回退和统一状态目录。它和新定位冲突。

**旧的“legacy target / launcher 与 bridge 二选一”表述是否还成立？** 成立，但要换个说法：

- 二选一的对象不是“包形态”，而是 **bridge 的所有权**：要么 acbl 管 bridge（推荐），要么 systemd 管 bridge（legacy）。二者同时启用会起两个 bridge 抢 7700，这个冲突不因 acbl 独立安装而消失；acbl 的 adopt 逻辑遇到“端口被占但身份不匹配”会判 `conflict` 并退出 78，不会盲起第二个。
- `acb-stack.target` 里不会也不能含 acbl——acbl 现在是系统层的静态入口，init 直接拉起它即可。target 的问题因此更明确：它不是“另一套 launcher”，而是**绕过 acbl 的裸 systemd 编排**。
- 只有当你**明确**要“放弃升级链、由 systemd 直接管桥”时才启用它；同时启用时必须 `systemctl --user mask acb-bridge.service`。
- 正解是把 target 退化成 acbl 的别名（`Wants=acb-launcher-user.service`），外部组件改由可选预设或用户自己的 unit 管理，见缺口 C-1。

### 1.4 外部组件边界

ACB 保证的部分分两个分发单元：

- **acbl 包**：`acb-launcher`、`acbctl`、`acb-supervisor`、systemd unit 模板、以及**可选的预设启动脚本**（仓库源 `deploy/presets/`，安装后随 acbl 分发）。
- **payload**：`acb-bridge`、harness 适配层、drivers 扇出脚本、插件，以及 `acb_contract` 声明。

其余都是用户自备：

- **dsh**：会话 harness，常见来源有 nix-config 的 `dsh.nix`、llm-agents 之类的 flake、上游仓库自建；
- **opencode**：v2 server，常见来源有 npm、上游发布二进制、nix；
- **snowluma / OneBot 端**：QQ 协议侧，按上游文档安装（容器或二进制）；
- **node**：dsh 与 opencode 的运行时，发行版包 / nvm / nix 均可；
- **模型端点与 API key**：用户自己的账号与网络。

**预设只负责“怎么起”，不负责“装什么”。** 预设是可选模板：用户可以启用、覆盖或完全不选，自己写等价组件声明（设计见 [acbl-component-manager.md](../plan/acbl-component-manager.md)）。

§五 给出几组参考组合，标明每组的“ACB 保证 / 用户自备”。用户可自行增补组合，欢迎提 PR 维护。

---

## 二、支持矩阵

八格仍然按“系统 × 产物来源 × 启停适配器”排列，但每格都默认有 acbl（独立安装）；nix 与 systemd 只作为可选列出现。

| # | 系统 | nix（产物来源） | systemd（启停适配器） | ACB 保证部分 | 外部组件 | 备注 / 缺什么 |
|---|---|---|---|---|---|---|
| 1 | NixOS | 用 | 用 | ✅ **存量已验证**（旧 target 拓扑） | 用户自备（可复用 nix-config 的邻居模块） | ① 现行 nix-config 部署走的是 §1.3 legacy 拓扑；acbl 主导形态在 NixOS 上**未验证**。② nix-config 的 `ExecStartPre` 与 acb rev 的参数契约见 B-4（触发条件：升级 flake 输入时）。③ acbl 独立安装的结构在 nix-config 里尚未落地 |
| 2 | NixOS | 用 | 不用 | 🟡 可照做（未验证） | 用户自备；可选启用 ACB 预设 | acbl 前台跑可行；**预设组件托管尚未实现**（B-7~B-10），当前 dsh / opencode 要用户自己起；仓库示例 `no-systemd-run.sh` 用 `env -i` 会丢密钥（B-1）；开机自启由用户 init 决定 |
| 3 | NixOS | 不用（cargo） | 用 | 🟡 可照做（未验证） | 用户自备 → **取决于用户组合** | 用 `--strategy copy`；NixOS 上默认 `ACB_UNIT_SHELL` 恰好可用；外部组件怎么装决定整栈 |
| 4 | NixOS | 不用（cargo） | 不用 | 🟡 可照做（未验证） | 用户自备 → **取决于用户组合** | 同 #3 构建 + 同 #2 的 B-1；自启由用户 init |
| 5 | 其他 Linux | 用 | 用 | 🟡 可照做（未验证） | 用户自备；可选启用 ACB 预设 | 必须覆盖 `ACB_UNIT_SHELL`；模板 PATH 写死 NixOS 路径，需在 env 文件覆盖（A-1/A-2）；预设脚本自身也仍有 nix 默认路径（A-7） |
| 6 | 其他 Linux | 用 | 不用 | 🟡 可照做（未验证） | 用户自备 → **取决于用户组合** | 同 #5 + B-1 |
| 7 | 其他 Linux | 不用（cargo） | 用 | 🟡 可照做（未验证） | 用户自备 → **取决于用户组合** | 同 #5，且 payload 只能 `copy`；Rust 工具链需自备 |
| 8 | 其他 Linux | 不用（cargo） | 不用 | 🟡 可照做（未验证） | 用户自备 → **取决于用户组合** | 原判 🔴 的理由（“只有桥被守护”“没有 init 集成”）已按新定位改判：前者是职责边界，后者由用户 init 决定；ACB 侧真实缺口仍是 B-1 与 A-3 |
| — | Windows | 任意 | 任意 | 🔴 不支持 | — | 无服务托管、无 Unix 信号/权限模型适配（C-5） |

**矩阵结论：**

- **只有 #1 有存量运行证据**，而且只对 **legacy target 拓扑**成立；**acbl 主导的形态在八格里全部未验证**。
- **#2–#8 的 ACB 部分都是“可照做但未验证”**，共同的真实缺口是无 systemd 示例脚本丢密钥（B-1）和非 NixOS 默认值（A-1/A-2/A-3）。
- **acbl 的安装来源**：NixOS 仍由 nix-config 提供；#5–#8 用预编译 `tar.gz`，按 target triple 二选一——`x86_64-unknown-linux-gnu`（最低 glibc 2.28）或 `x86_64-unknown-linux-musl`（Alpine，acbl 全静态）；有 nix 但不走 NixOS 模块的，可直接用本仓库 flake 的 `acbl` package（§3.2.0）。没有 deb / rpm / apk（规格见 [acbl-component-manager.md](../plan/acbl-component-manager.md) §2.5）。
- **“整栈能不能跑”不是 ACB 的承诺**：凡是需要 dsh / opencode / snowluma / node 的组合，都标为“取决于用户组合”。

### 2.1 判定变化说明

| 格子 | 旧判定 | 新判定 | 原因 |
|---|---|---|---|
| #8（其他 Linux + 无 nix + 无 systemd） | 🔴 不完整 | 🟡 可照做（未验证） | “launcher 只守护桥”是职责边界而非缺陷；开机自启属于用户 init 的选择 |
| #3 / #5 / #7 及所有需要 dsh / opencode 的组合 | 🟡 可照做，备注“需自己提供 node / dsh / opencode” | 🟡 可照做，备注改为“**取决于用户组合**” | 外部组件的安装方式不再是 ACB 的缺口，只是组合差异 |
| 所有 systemd 组合的“整栈” | 隐含“target 拉 4 服务” | 拆成“ACB 部分（launcher）”+“外部组件（用户）” | 定位从 systemd 编排改为 launcher 编排 |
| #1 | ✅ 已验证 | ✅ 已验证（仅 legacy 拓扑） | acbl 主导形态没有实测证据，不能标绿 |

---

## 三、公共准备

八种组合只在“用什么构建产物、要不要挂 systemd”上不同，其余步骤一样。先把这一节跑通，再看 §四。

### 3.1 目录约定

本文统一用这些路径（全部可改，改了要同步改环境变量）：

```sh
export REPO=/opt/AgentChatBridge                 # 仓库目录
export ACB_DATA=$HOME/.local/share/acb           # 数据根
export PROFILE_DIR=$ACB_DATA/profile             # 第一代 profile 的根
export STATE_DIR=$ACB_DATA/state/supervisor      # current.json / lkg.json / supervisor.toml
export CONFIG=$ACB_DATA/config/config.toml       # 桥配置
export ENVFILE=$HOME/.config/acb/acb.env         # 密钥 env 文件（0600）
export LOG_DIR=$ACB_DATA/logs
export OPENCODE_DIR=$ACB_DATA/opencode           # opencode 工作目录（unit install 时仍为必填）
export PAYLOAD=$ACB_DATA/payload/$(date -u +%Y%m%dT%H%M%SZ)
export ACBL_BIN=$(command -v acb-launcher)      # 静态入口，来自系统安装的 acbl
export ACBCTL_BIN=$(command -v acbctl)

mkdir -p "$ACB_DATA"/{config,state,logs,payload,opencode} "$(dirname "$ENVFILE")"
chmod 700 "$ACB_DATA"
cp "$REPO/config.example.toml" "$CONFIG"
```

`storage.path`、`file_relay`、`connections` 等按 [配置手册](./config) 改。建议把 `storage.path` 写成绝对路径，例如 `$ACB_DATA/state/acb.db`。

### 3.2 安装 acbl 与组装 payload（两件事，分开做）

#### 3.2.0 先装 acbl（独立于 payload）

acbl 是静态入口，**不放进 payload**。按系统选一种来源：

```sh
# NixOS：由 nix-config 引用本仓库 flake 的 acbl package，随系统 switch 更新；
# unit 指向 /run/current-system/sw/bin/acb-launcher（不要写 store 路径）。
command -v acb-launcher acbctl

# 非 NixOS + 有 nix：取 flake 的 acbl package（已含 launcher / supervisor / acbctl）
nix build .#acbl -o result-acbl
# 或 nix profile install "$REPO#acbl"

# 非 NixOS + 无 nix：预编译 tar.gz（C-9 规格已出，包尚未产出）
# 按 target triple 二选一：x86_64-unknown-linux-gnu / x86_64-unknown-linux-musl
sudo tar -xzf acbl-x86_64-unknown-linux-musl.tar.gz -C /opt
sudo /opt/acbl/install.sh --prefix /opt/acbl --scope system --autostart 0

# 或 cargo 构建后复制到系统路径（没有 tar.gz / nix 时的临时路径）
cargo build --release
install -m755 target/release/{acb-launcher,acb-supervisor,acbctl} /usr/local/bin/

export ACBL_BIN=$(command -v acb-launcher)
export ACBCTL_BIN=$(command -v acbctl)
```

要求：unit / init 的 `ExecStart` 固定为这个静态入口；更新 acbl 只换入口指向的二进制，**不改 unit，也不改 payload**。

- **只有 tar.gz，没有 deb / rpm / apk**：`install.sh` 只解包与注册 unit，**不 enable / start、不 seed profile、不写状态目录**；是否开机自启由 `--autostart` 决定（§3.6）。unit 模板在 ACB 仓库的 `share/acb/units/`，由 `acbctl unit install` 渲染。
- **acbl 与 payload 必须同一 libc 家族**：gnu 制品（最低 glibc 2.28）配 gnu 构建的 bridge；musl 制品（acbl 全静态）配动态链接 musl 的 bridge——`.so` 插件靠 `dlopen`，bridge 不能全静态。

#### A. 用 nix 构建 payload（可选来源之一）

payload 只装会迭代的内容：

```text
bin/acb-bridge
harnesses/...
deploy/start-drivers.sh
acb-contract.json
```

```sh
nix build .#acb-payload -o result
# 非 NixOS 且没开 flake 时：
# nix --extra-experimental-features 'nix-command flakes' build .#acb-payload -o result

PAYLOAD=$(readlink -f result)
ls "$PAYLOAD"   # bin harnesses deploy acb-contract.json

# 配置里用了 lua provider 时再带上：
cp -r "$REPO/examples/lua" "$PAYLOAD/lua"
```

`acb-payload` 已按 B-12 收窄后的口径打包：不含 acbl 二进制、不含 `share/acb/units/`，`acb-contract.json` 在 payload 根。

#### B. 不用 nix，用 cargo（可选来源之一）

需要 Rust 1.85+、`pkg-config`、OpenSSL、zlib、libgit2（或走 vendored 特性）：

```sh
cd "$REPO"
cargo build --release

mkdir -p "$PAYLOAD/bin" "$PAYLOAD/deploy"
cp target/release/acb-bridge "$PAYLOAD/bin/"
cp -r harnesses "$PAYLOAD/"
cp deploy/start-drivers.sh "$PAYLOAD/deploy/"
cp deploy/acb-contract.json "$PAYLOAD/acb-contract.json"
cp -r examples/lua "$PAYLOAD/lua"        # 可选
```

**B-12 收窄后的口径**：`validate_payload` 只要求 `bin/`、`harnesses/`、`deploy/start-drivers.sh`，并校验 payload 根的 `acb-contract.json`（`crates/acb/src/profile.rs` / `contract.rs`）。`share/acb/units/`、`deploy/run/`、`deploy/start-dsh.sh`、`deploy/opencode-serve.sh` 已不属于 payload。

`profile seed` 要求 `bin/` 与 `harnesses/` 存在，**只把 `result/bin` 交给 seed 会直接失败**。

### 3.3 建第一代 profile

策略必须显式给，**不做自动判断**：

```sh
"$ACBCTL_BIN" profile seed "$PAYLOAD" \
  --strategy copy \
  --profile-dir "$PROFILE_DIR" \
  --state-dir "$STATE_DIR"

"$ACBCTL_BIN" profile current
"$ACBCTL_BIN" profile list
```

- `copy`：本地快照，任何目录都能用，**没有 nix 也能用**；第一代建议一律先用它。
- `store`：只有 payload 位于 `/nix/store` 时才可用（靠 store 不可变做回退）。`nix build .#acb-payload` 的产物就是 store 路径，直接用作 `--payload` 即可；不再需要 nix-config 自己用 `runCommand` 打包。

`profile seed` 会顺带写入初始 LKG（`$STATE_DIR/lkg.json`），已有则不动。

> **契约版本（B-12，已实现）**：payload 根的 `acb-contract.json` 声明 `schema`、`contract`、`min_acbl`、`layout`、`commands`；acbl 在 `profile add` / `switch` / 启动时校验，支持区间当前为 `1..=1`。缺文件时按 `contract = 1` 过渡放行；设置 `ACB_CONTRACT_STRICT=1` 后缺文件直接拒绝。`nix build .#acb-payload` 已把该文件打进 payload 根，手工组装时不要漏。

> **版本契约（B-4）**：本仓库当前代码要求显式 `--strategy`；nix-config 当前 pin 的 acb rev（`b9c7d4b`）**不认这个参数**，策略是自动推断的。所以：
> - 现在**不要**给 nix-config 的 `ExecStartPre profile seed` 加 `--strategy`，加了会让 `ExecStartPre` 失败；
> - **把 flake 的 acb 输入升级到带显式策略的版本时，必须同步补上 `--strategy`**，否则全新 profile 起不来。

### 3.4 密钥注入

密钥只走进程环境，不写进配置、不写进数据库。推荐 env 文件：

```sh
cat > "$ENVFILE" <<'EOF'
# 这些变量名要和 config.toml 里的 ${...} 占位符一致
ACB_CONSOLE_TOKEN=replace-me
ACB_SNOWLUMA_TOKEN=replace-me
ACB_MODEL_KEY=replace-me
NTFY_ACB_TOPIC=replace-me
# 非 NixOS 建议顺便覆盖 PATH（systemd 会先读 Environment= 再读 EnvironmentFile，后者优先）
PATH=/usr/local/bin:/usr/bin:/bin
EOF
chmod 600 "$ENVFILE"
```

配置里对应写成 `token = "${ACB_CONSOLE_TOKEN}"`。细节见 [配置手册](./config)。

- **launcher 前台**：启动前 `set -a; . "$ENVFILE"; set +a`，launcher 与桥都继承。
- **systemd 用户级**：把 `ACB_UNIT_ENV_FILE` 指向这个文件即可，systemd 会注入给 launcher。
- **systemd 系统级**：文件必须 root 可读；如果服务以 root 跑，0600 + root 所有。
- **不要**直接套用 `examples/deploy/no-systemd-run.sh` 当正式启动器：它用 `env -i` 只放行 `ACB_UNIT_*`，**密钥到不了子进程**（见缺口 B-1）。自己 source env 文件即可。

### 3.5 启动 acbl（所有组合的统一入口）

```sh
set -a; . "$ENVFILE"; set +a

exec "$ACBL_BIN" --config "$STATE_DIR/supervisor.toml"
```

也可以让 `acbctl` 生成 supervisor 配置并前台托管：

```sh
"$ACBCTL_BIN" run --mode=launcher \
  --config "$CONFIG" --state-dir "$STATE_DIR"
```

- `--mode=launcher` 是**部署形态**：拉起桥、探针、LKG 回退、消费升级请求。
- `--mode=direct` 只启动桥本身，**仅供排障**；没有监督、没有回退、不消费升级请求，不要当部署方案。
- `run` 不读 `ACB_UNIT_ENV_FILE`，密钥必须由外层环境提供（见 3.4）。
- 注意：`acb-launcher` 来自**系统安装的 acbl**，不是 `$PROFILE_DIR/current/bin`。payload 里没有它。

### 3.6 可选：把 acbl 挂到 systemd（启停适配器）

`acbctl unit install` 只写文件，不执行 `systemctl`。先导出安装参数：

```sh
export ACB_UNIT_LOG_DIR="$LOG_DIR"
export ACB_UNIT_PROFILE_DIR="$PROFILE_DIR"
export ACB_UNIT_CONFIG_FILE="$CONFIG"
export ACB_UNIT_ENV_FILE="$ENVFILE"
export ACB_UNIT_OPENCODE_DIR="$OPENCODE_DIR"
export ACB_UNIT_CONSOLE_PORT=3100
export ACB_UNIT_OPENCODE_PORT=4097
export ACB_UNIT_SHELL=/bin/bash                # NixOS 可省略（默认 /run/current-system/sw/bin/bash）
export ACB_UNIT_INSTALL_SECTION=1               # 1 才渲染 [Install] 段（不执行 enable）
export ACB_UNIT_LAUNCHER_BIN="$ACBL_BIN"       # 关键：指向静态入口，不是 profile/current/bin
# 或用前缀：export ACB_UNIT_ACBL_PREFIX=/opt/acbl   # → /opt/acbl/bin/acb-launcher

"$ACBCTL_BIN" unit install --root "$HOME/.config/systemd/user"
"$ACBCTL_BIN" unit doctor  --root "$HOME/.config/systemd/user"
# install 只渲染文件，不 enable/start；enable 命令见下一节。
systemctl --user daemon-reload
```

**正确形态只启用 launcher：**

```sh
loginctl enable-linger "$USER"                       # 用户级开机自启前提
systemctl --user enable --now acb-launcher-user.service
```

`unit install` 同时写出 `acb-launcher.service`（系统级）、`acb-launcher-user.service`（用户级）、`acb-bridge.service` 等。按你的 scope 只启用对应的 launcher，**其余一概不启用**。

只有你明确选择 §1.3 的 legacy 拓扑时，才这样：

```sh
systemctl --user enable --now acb-stack.target
systemctl --user mask acb-bridge.service   # 如果同时还启用了 launcher
```

**stop 不等于停桥**：`systemctl --user stop acb-launcher-user.service` 只停 acbl；bridge 继续服务，下次 `start` 时新 acbl 按身份端点探测后 adopt（[acbl-component-manager.md](../plan/acbl-component-manager.md) §4.7）。unit 模板必须带 `KillMode=process`，否则 systemd 会连带杀掉同一 cgroup 里的 bridge。

`ACB_UNIT_INSTALL_SECTION=0` 时不会渲染 `[Install]`，也不会有 enable；install 只落盘，enable 由部署者执行（见上面“正确形态只启用 launcher”）。

若确实不想写 `[Install]`，再手工挂接：

```sh
systemctl --user start acb-launcher-user.service
systemctl --user add-wants default.target acb-launcher-user.service
```

**系统级（不推荐）**：`--root /etc/systemd/system` 可以装，但模板不写 `User=`，服务会以 root 跑。要系统级就自己加 drop-in：

```sh
sudo systemctl edit acb-launcher
# [Service]
# User=acb
# Group=acb
```

并把 `ACB_DATA`、`CONFIG`、`ENVFILE` 的属主改成该用户。

### 3.7 可选：用自己的 init（runit / s6 / openrc / 容器 entrypoint）

acbl 自己就是监管者，所以 init 只需要做到三件事：

1. **前台运行** `"$ACBL_BIN" --config "$STATE_DIR/supervisor.toml"`（静态入口，通常是 `/usr/local/bin/acb-launcher` 或 `/run/current-system/sw/bin/acb-launcher`）；
2. 把 `$ENVFILE` 注入到它的环境（不要用 `env -i` 白名单）；
3. 处理退出码：`78` = 放弃自动恢复，需要人处理；普通崩溃按 init 的重启策略拉起。**升级不再使用 `75`**。

各 init 的重启策略按各自语法写即可。仓库目前没有通用 init 脚本（缺口 C-2），容器场景参考：

```dockerfile
ENV ACBL=/usr/local/bin/acb-launcher
ENTRYPOINT ["/usr/local/bin/acb-launcher", "--config", "/opt/acb/state/supervisor/supervisor.toml"]
```

### 3.8 验证（看 ACB 部分是否真的起来）

```sh
"$ACBCTL_BIN" status --mode=run --state-dir "$STATE_DIR"
curl -sS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:3100/
# systemd 适配器：
systemctl --user --no-pager status acb-launcher-user.service
```

外部组件是否连通，用各自手段验证（dsh 的 session、opencode 的 serve 端口等），不在本文承诺范围内。

### 3.9 后续升级

```sh
# 新 payload 组装好后
"$ACBCTL_BIN" profile add "$NEW_PAYLOAD" --strategy copy
"$ACBCTL_BIN" profile switch <generation-or-label>
```

`acbctl upgrade` 写请求信箱，**只有 acbl 在跑时才会被消费**。acbl 收到后停旧 bridge、切指针、原地起新 bridge——**它自己不退出、不重启**。acbl 自己的更新走系统层（nix-config switch / 包升级 / 替换 tar.gz 后重启 acbl），不经过升级链；acbl 重启期间 bridge 继续运行，新 acbl 启动后 adopt，不产生第二个 bridge。legacy target 拓扑下升级链不生效，只能手工 `profile switch` + 重启桥。

---

## 四、逐组合流程

每节只写“和 §三 不同的部分”；启动统一是 acbl（§3.5）。

### 4.1 组合 #1：NixOS + nix + systemd（✅ 存量已验证，legacy 拓扑）

**推荐直接用 nix-config 的模块**，不要手写。若要脱离 nix-config 手工装：

```sh
# 装 acbl（§3.2.0）：NixOS 上由 nix-config 提供；这里只检查
export ACBL_BIN=$(command -v acb-launcher)
export ACBCTL_BIN=$(command -v acbctl)

# 构建 + 组装 payload（§3.2 A）：acb-payload 已按收窄口径组装好
nix build .#acb-payload -o result
PAYLOAD=$(readlink -f result)

# 第一代 profile（§3.3）
"$ACBCTL_BIN" profile seed "$PAYLOAD" --strategy copy \
  --profile-dir "$PROFILE_DIR" --state-dir "$STATE_DIR"

# 挂 systemd 适配器（§3.6），只要 acbl
export ACB_UNIT_LAUNCHER_BIN="$ACBL_BIN"
"$ACBCTL_BIN" unit install --root "$HOME/.config/systemd/user"
loginctl enable-linger "$USER"
systemctl --user enable --now acb-launcher-user.service
```

注意两点：

- 这是 acbl 主导形态，**未在实机验证**；现有绿色只覆盖 nix-config 的 legacy target 拓扑。
- 关于 `profile seed` 的 `--strategy`：见 §3.3 的版本契约（B-4），现在别给 nix-config 加。

### 4.2 组合 #2：NixOS + nix + 无 systemd（🟡 未验证）

按 §3.2 A 构建、§3.3 seed 之后：

```sh
set -a; . "$ENVFILE"; set +a
"$ACBCTL_BIN" run --mode=launcher --config "$CONFIG" --state-dir "$STATE_DIR"
```

- 不要用 `examples/deploy/no-systemd-run.sh` 当正式启动器（密钥会丢，B-1）。
- 开机自启接你自己的 init（§3.7）。

### 4.3 组合 #3：NixOS + 无 nix + systemd（🟡 未验证）

按 §3.2 B 用 cargo 构建、§3.3 seed，然后 §3.6 只启用 launcher。NixOS 上可以沿用默认 `ACB_UNIT_SHELL`；也可以显式写 `/bin/bash`（NixOS 有兼容路径）。

### 4.4 组合 #4：NixOS + 无 nix + 无 systemd（🟡 未验证）

同 #3 构建与 seed，然后按 §3.5 前台启动 acbl；自启接 §3.7。

### 4.5 组合 #5：其他 Linux + nix + systemd（🟡 未验证）

```sh
# 装 acbl（§3.2.0）：非 NixOS + nix
nix profile install "$REPO#acbl"
export ACBL_BIN=$(command -v acb-launcher)

# 构建（§3.2 A）：acb-payload 已按收窄口径组装好
nix --extra-experimental-features 'nix-command flakes' build .#acb-payload -o result
PAYLOAD=$(readlink -f result)

# seed（§3.3）
"$ACBCTL_BIN" profile seed "$PAYLOAD" --strategy copy \
  --profile-dir "$PROFILE_DIR" --state-dir "$STATE_DIR"

# 挂 systemd 适配器（§3.6）：必须覆盖 shell；PATH 写进 env 文件覆盖模板默认
export ACB_UNIT_SHELL=/bin/bash
export ACB_UNIT_LAUNCHER_BIN="$ACBL_BIN"
"$ACBCTL_BIN" unit install --root "$HOME/.config/systemd/user"
loginctl enable-linger "$USER"
systemctl --user enable --now acb-launcher-user.service
```

非 NixOS 的重点：

- **`ACB_UNIT_SHELL` 必须覆盖**，否则模板默认的 `/run/current-system/sw/bin/bash` 不存在（A-1）。
- 模板里的 `PATH=<profile>/bin:/run/current-system/sw/bin:/run/wrappers/bin` 是写死的；在 `$ENVFILE` 里写 `PATH=/usr/local/bin:/usr/bin:/bin:/<profile>/bin`（`EnvironmentFile` 优先于 `Environment=`），或者用 `systemctl --user edit` drop-in 覆盖（A-2）。
- 外部组件（dsh / node / opencode）靠 PATH 或显式变量找到，装法是用户组合的事（§五）。

### 4.6 组合 #6：其他 Linux + nix + 无 systemd（🟡 未验证）

同 #5 构建/seed，然后 §3.5 启动；启动前额外 `export PATH=/usr/local/bin:/usr/bin:/bin:$PATH`，并确保外部组件（`NODE` / `ACB_OPENCODE_BIN` / dsh）按 §五 自备。

### 4.7 组合 #7：其他 Linux + 无 nix + systemd（🟡 未验证）

```sh
# 装 acbl（§3.2.0）：cargo 构建后安装
cd "$REPO" && cargo build --release
install -m755 target/release/{acb-launcher,acb-supervisor,acbctl} /usr/local/bin/
export ACBL_BIN=$(command -v acb-launcher)
export ACBCTL_BIN=$(command -v acbctl)

# 构建 payload（§3.2 B）
mkdir -p "$PAYLOAD/bin"
cp target/release/acb-bridge "$PAYLOAD/bin/"
cp -r deploy harnesses "$PAYLOAD/"

# seed
"$ACBCTL_BIN" profile seed "$PAYLOAD" --strategy copy \
  --profile-dir "$PROFILE_DIR" --state-dir "$STATE_DIR"

# 挂 systemd 适配器
export ACB_UNIT_SHELL=/bin/bash
export ACB_UNIT_LAUNCHER_BIN="$ACBL_BIN"
"$ACBCTL_BIN" unit install --root "$HOME/.config/systemd/user"
loginctl enable-linger "$USER"
systemctl --user enable --now acb-launcher-user.service
```

其余同 #5（PATH / 外部组件）。

### 4.8 组合 #8：其他 Linux + 无 nix + 无 systemd（🟡 未验证，原判 🔴）

构建/seed 同 #7，然后：

```sh
set -a; . "$ENVFILE"; set +a
export PATH="/usr/local/bin:/usr/bin:/bin:$PATH"
exec "$ACBL_BIN" --config "$STATE_DIR/supervisor.toml"
```

状态说明：

- ACB 部分：launcher + 桥可以这样跑，**但未实机验证**。
- 外部组件：用户自备（§五），整栈是否成立取决于组合。
- 开机自启：接用户 init（§3.7）。
- 唯一真实的 ACB 侧阻碍是示例脚本 B-1 和默认路径 A-3；按本节手写命令即可绕开。

---

## 五、外部组件与参考组合

**ACB 不负责这些组件的安装与升级；托管是“可选”。** 下面只给参考，不构成承诺。

### 5.1 组件边界

| 组件 | ACB 是否提供 | 用户需保证 | 参考安装方式（按其上游文档为准） |
|---|---|---|---|
| dsh | 否 | 可执行文件在 PATH 或 `dsh` 会话配置可找到；版本与 `harnesses/dsh` 兼容 | nix-config 的 `dsh.nix`、llm-agents 类 flake、上游仓库自建 |
| opencode v2 | 否 | serve 端口可达；`ACB_OPENCODE_BIN` 或 PATH 能找到 | npm、上游发布二进制、nix |
| snowluma / OneBot 端 | 否 | WebSocket 可达、token 一致 | 上游容器或二进制 |
| node | 否（脚本会调用） | 版本满足 dsh / opencode 要求 | 发行版包、nvm、nix |
| 模型端点 / API key | 否 | 网络与配额 | 用户自己的账号 |

### 5.2 参考组合

| 组合 | ACB 保证的部分 | 用户自备的部分 | 备注 |
|---|---|---|---|
| **R1 全 nix（NixOS）** | launcher、桥、profile、可选 systemd 适配器 | dsh / opencode / node 由 nix（nix-config 邻居模块或 llm-agents 类 flake）提供 | 最接近现有部署；注意 §1.3 legacy 拓扑与新形态的差别 |
| **R2 nix 产物 + 发行版外部组件（非 NixOS）** | nix 构建 payload、launcher、桥；systemd 适配器可选 | node / dsh / opencode 由 apt / dnf / guix / homebrew / npm 等提供 | 需要覆盖 `ACB_UNIT_SHELL` 与 PATH |
| **R3 全 cargo / 全手工（无 nix）** | cargo 构建 payload、launcher、桥；`--strategy copy` | 全部外部组件手工装（npm / 二进制） | 无 systemd 时接用户 init |
| **R4 容器 / 最小系统** | launcher 作为 entrypoint，桥在其下 | 镜像里预装外部组件；密钥由容器 secret 机制注入 | 无 systemd；`env` 不要白名单裁剪 |

**扩展组合**：用户可自行增补（例如 openrc + guix、homebrew + macOS、scoop + Windows）。欢迎提 PR 补矩阵行；文档只维护参考组合，不做全量承诺。

### 5.3 可选预设：按安装方式拆分的服务列表

ACB 计划提供 `deploy/presets/`，预设的维度是**安装方式**，不是组件名：同一个 dsh 可以有 `dsh_nix.sh`、`dsh_npm.sh`、`dsh_guix.sh`、`dsh_scoop.sh`；opencode 可以有 `opencode_nix.sh`、`opencode_npm.sh`。清单 `deploy/presets/presets.toml` 把它们登记成一张“带开关的服务列表”。

- **ACB 保证**：预设脚本的启动/停止/健康检查/重启策略由 acbl 执行；**预设清单与脚本随 acbl 安装分发，不随 payload**（仓库源在 `deploy/presets/`，安装后在 acbl 包的共享目录）。payload 只带 drivers 实际执行的扇出脚本与 harness 实现。
- **用户自备**：组件本体（dsh / opencode / node）仍要自己装；预设只负责“怎么起”，找不到二进制时明确报错。
- **互斥**：同一个组件同一时刻只能启用一个来源（例如 `dsh` 不能同时开 nix 和 npm 两个预设）；在 `supervisor.toml` 的 `[[component]]` 里用 `preset = "dsh_nix"` 选择，改了要重启 acbl 或点控制台的“重新加载组件表”。
- **控制台**：短期只读展示“有哪些预设、启用了哪个、健康如何”；长期只允许“选预设 / 开关 / 覆盖白名单字段”，不允许填写任意命令。
- **当前状态**：组件表、预设装载与热加载按钮**尚未实现**（缺口 B-7~B-10）；现在只能靠 systemd 各 unit 或用户自己的进程管理器。
- 设计见 [acbl-component-manager.md](../plan/acbl-component-manager.md)。

---

## 六、缺口清单

按“改默认值就能解决 / 需要改代码 / 需要新做机制”三档。组合编号对应 §二。**外部组件不再计入缺口**（见 §1.4）。

### A. 改默认值就能解决

| # | 缺口 | 现象 | 影响组合 |
|---|---|---|---|
| A-1 | `ACB_UNIT_SHELL` 默认写死 NixOS bash | 非 NixOS 安装后服务起不来 | #5 #6 #7 #8 |
| A-2 | unit 模板 PATH 写死 `/run/current-system/sw/bin`、`/run/wrappers/bin` | 非 NixOS 找不到外部组件；需 env 文件覆盖 | #5 #6 #7 #8 |
| A-3 | `no-systemd-run.sh` 默认 `/etc/profiles/...` 与 NixOS bash | 非 NixOS 直接跑会找不到路径 | #2 #4 #6 #8 |
| A-4 | `ACB_UNIT_OPENCODE_DIR` 是必填项 | 不用 opencode 也必须造一个目录 | #1–#8 |
| A-5 | `unit install` 同时写出系统级和用户级 launcher | 用户目录里多一份 `acb-launcher.service`，容易误启用 | #1–#8 |
| A-6 | 系统级模板没有 `User=` | 系统级部署默认 root 运行 | #1 #3 #5 #7 |
| A-7 | 预设脚本仍有 nix 默认路径（`opencode-serve.sh` 的 `/run/current-system/sw/bin` 与 `/etc/profiles/...`；`start-drivers.sh` 的 node 默认路径） | 非 NixOS 启用预设时找不到二进制；应改为显式变量优先、缺失即报错 | #5 #6 #7 #8（以及任何启用预设的组合） |
| A-8 | ~~`ACB_UNIT_LAUNCHER_BIN` 默认从 `profile/current/bin` 派生~~ **已实现** | 默认改为 `ACB_UNIT_ACBL_PREFIX`（`/usr/local`）→ `$prefix/bin/acb-launcher`；显式 `ACB_UNIT_LAUNCHER_BIN` 落在 profile/generations 内时报错。unit 同时固定 `KillMode=process` / `Restart=on-failure` | #1–#8 |

### B. 需要改代码

| # | 缺口 | 现象 | 影响组合 |
|---|---|---|---|
| B-1 | 无 systemd 示例用 `env -i` + `ACB_UNIT_*` 白名单 | env 文件里的密钥/模型 key 进不了 launcher/bridge；`acbctl run` 也不读 `ACB_UNIT_ENV_FILE` | #2 #4 #6 #8 |
| B-4 | nix-config 的 `ExecStartPre profile seed` 与 acb rev 的参数契约不一致 | **当前 pin 的 rev `b9c7d4b` 不认 `--strategy`（策略自动推断），此时加参数会让 `ExecStartPre` 失败**；升级到带显式策略的版本后若不同步补上，全新 profile 起不来 | #1（外部 nix-config）。**触发条件：升级 flake 的 acb 输入时，必须同步补 `--strategy`** |
| B-5 | file 密钥源没有一等公民实现 | 目前只能“文件当 env 文件”喂 systemd；配置里不能写 `source=file,path=...` | #3 #7（以及所有想脱离 nix 的组合） |
| B-6 | 仓库 flake 没有 profile-bootstrap 输出 | 手工 nix 部署只能先 `copy`，`store` 需要自己打 store payload | #1 #2 #5 #6 |
| B-7 | supervisor / launcher 只有一个子进程 | 只能管桥；dsh / drivers / opencode 无法由 acbl 托管 | #2 #4 #6 #8（所有想用预设的组合） |
| B-8 | 停止只 kill 直接子进程，没有进程组 | 预设脚本内部 `serve &` 的孙进程会成孤儿；无 systemd 时更明显 | 全组合（启用预设时） |
| B-9 | 没有组件表 / 预设清单 | `supervisor.toml` 只有 `[supervisor]`；无法按安装方式（nix/npm/guix/scoop…）登记预设，也无法保证“同一组件只启用一个来源” | 全组合（启用预设时） |
| B-10 | 没有组件表热加载机制 | 改了 `[[component]]` 只能重启 acbl；需要控制台按钮 + 校验 + 失败回滚，且不做自动热加载 | 全组合（使用预设/组件表后） |
| B-11 | drivers 的会话列表写死在 `start-drivers.sh:69-74` | 改一个会话要改脚本、动 payload 版本；违反“配置单一数据源、能进库的进库”。必须改为从配置/库读取（与 `[[sessions]]` 同一数据源），脚本退化为纯扇出执行器 | 所有使用 drivers 的组合（#1 及启用 `drivers_acb` 的组合） |
| B-12 | ~~payload 校验与契约版本未拆分~~ **已实现** | `validate_payload` 收窄为只查 payload 自身内容（`bin/`、`harnesses/`、`deploy/start-drivers.sh`），并校验 payload 根的 `acb-contract.json`（`profile add` / `switch` / launcher 启动三处）；`share/acb/units`、`deploy/run/supervisor.toml.tmpl`、`deploy/start-dsh.sh`、`deploy/opencode-serve.sh` 已归 acbl | #1–#8 |

> 原 **B-2“launcher 只守护桥”** 已按新定位降级为**职责边界**，见 §1.2 与 §五，不再作为缺陷。
> 原 **B-3“升级链与 4 服务拓扑互斥”** 是 §1.3 legacy 拓扑的直接后果，并入 C-1 一起解决。

### C. 需要新做机制

| # | 缺口 | 现象 | 影响组合 |
|---|---|---|---|
| C-1 | `acb-stack.target` 仍是“systemd 拉 4 服务”的总指挥拓扑 | 绕过 launcher，升级链/LKG 失效；与 launcher 同时启用会抢端口。应改为 launcher 的启停别名（或删除 target） | 所有 systemd 组合 |
| C-2 | 无 systemd 的 init 参考（runit/s6/openrc/容器 entrypoint） | 没有现成自启模板；用户需自己写（launcher 已自带监管与退出码契约） | #2 #4 #6 #8 |
| C-3 | 一键安装器（payload 组装 + seed + 单元注册 + env 模板） | 现在要手工拼 5 个变量、6 个目录，极易漏 | #1–#8 |
| C-4 | 发布流水线（构建矩阵 / 校验和 / 签名） | 两个 tar.gz 制品要在真正老于基线的环境构建、按 target triple 分开校验；没有 nix 的组合不该要求用户装 Rust 工具链。与 C-9 同批交付 | #3 #4 #7 #8 |
| C-5 | Windows 支持 | 无服务托管、无 Unix 信号/权限模型；当前完全没有路径 | 新组合 |
| C-6 | 密钥来源 provider 抽象（env/file/keyring）+ 配置只存来源类型与名字 | 当前实现是“配置里写 `${NAME}` 占位符”，不符合“配置不存密钥、不插值”的目标 | 全组合 |
| C-7 | 启动 acbl 的方式尚未做成安装器参数 | 三类方式（手工 / systemd 自启 / systemd 只守护）与无 systemd 自管，差异落在“崩溃/78 后谁重拉、日志归属、cgroup”上；需要 `--autostart` / `--scope` / `--log-target` 等显式接口。升级不再重启 acbl，`restart_owner` 已取消 | 全组合 |
| C-8 | 控制台组件编辑（长期） | 短期只读；长期只允许“选预设 / 开关 / 白名单字段”，需要鉴权、二次确认、变更审计与失败回滚 | 全组合（长期） |

---

### D. 待交付项（规格已出，实现未开始）

| # | 交付项 | 规格 | 前置依赖 | 状态 |
|---|---|---|---|---|
| C-9 | acbl 独立安装包：非 nix 环境只出 `tar.gz`，两个 target triple —— `x86_64-unknown-linux-gnu`（最低 glibc 2.28）与 `x86_64-unknown-linux-musl`（acbl 全静态）；NixOS / 有 nix 的环境走本仓库 flake 的 `acbl` package（已实现，§3.2.0）；rpm / apk / deb / Windows 全部不产出 | [acbl-component-manager.md §2.5](../plan/acbl-component-manager.md) | **A-8 → B-12 → C-9** | nix 包已实现；tar.gz 规格已出、实现未开始 |

C-9 的交付边界：安装只装 acbl，不碰 payload 与 profile / LKG 状态目录；卸载不删除状态根。aarch64 是否同期产出待定。

---

## 七、一句话结论

**acbl 是独立安装的静态监管者，payload 是它监管的迭代内容：nix 与 systemd 都只是用户可选的适配层；八格里只有 #1 的旧 target 拓扑有运行证据，acbl 主导形态全部未验证；外部组件（dsh / opencode / snowluma）的安装不属于 ACB 职责，整栈能否跑通取决于用户组合。**

acbl 的定位是“组件管理者”，但升级链只覆盖 payload 里的 bridge：bridge 归它管，外部组件可选地使用 ACB 预设（按安装方式拆分成 `dsh_nix` / `dsh_npm` / `opencode_nix`…，同一组件只启用一个来源）、也可由用户自写；组件表通过控制台按钮热加载、失败保留旧表。升级只切 payload、只重启 bridge，acbl 全程不动；acbl 自己的更新由系统层负责（NixOS 上就是 nix-config 的 switch，引用本仓库 flake 的 `acbl` package；非 NixOS 是 nix profile / 替换 tar.gz 后重启 acbl），而 bridge 在 acbl 重启期间继续服务、由新 acbl adopt——acbl 是观察者与恢复者，不是 bridge 的所有者。acbl 的稳定分发：NixOS 与有 nix 的环境用 `acbl` package，非 nix 环境出两个 `tar.gz`（gnu 钉死 glibc 2.28 基线、musl 全静态），且 musl 上 bridge 必须动态链接（`.so` 靠 `dlopen`）。组件托管尚未实现（B-7~B-10），tar.gz 制品尚未产出（C-9），设计见 [acbl-component-manager.md](../plan/acbl-component-manager.md)。

结构拆分已经落地：A-8（unit 入口指向静态 acbl、`KillMode=process`、adopt）与 B-12（payload 校验收窄、`acb-contract.json` 三处校验）均已实现。现在的主线缺口是组件托管（B-7~B-10）与非 nix 的 tar.gz 发布（C-9）。
