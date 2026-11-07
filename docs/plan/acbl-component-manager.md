# acbl 定位升级：从升级执行器到组件管理者

状态：2026-11-07 设计稿（第七轮结构修正）。**只写文档，不改代码、不重启、不提交。**

结构前提：

- **acbl 独立安装，不进 payload。** payload 只装会迭代的内容：`acb-bridge`、drivers、插件、harness 适配层。
- **NixOS 上 acbl 由 nix-config 提供与维护**，随 `nixos-rebuild switch` 更新，属于低频变更。nix-config 里保留 acbl 不是技术债，是设计。
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

1. **结构变了：acbl 与 payload 分开分发。** payload 只有 `bin/acb-bridge`、`harnesses/`、drivers 扇出脚本与插件；acbl（`acb-launcher` / `acbctl` / `acb-supervisor` / 预设 / unit 模板）单独安装，NixOS 上由 nix-config 提供（§2.4）。
2. **升级不重启 acbl。** acbl 在原进程里完成：停旧 bridge → 提交 `current.json` → 按新指针在同一进程树里起新 bridge。旧模型里的 `restarting` 状态和“退出 75 等外部重拉”一起退役。
3. **75 收窄为兼容保留值，78 保留。** 75 不再由升级路径发出；78 仍表示“配置/指针/journal 不可恢复，放弃自动恢复”。acbl 自身崩溃由 init 重拉，不用退出码表达（§4.2）。
4. **`restart_owner` 取消作为升级门禁，`self` 档取消。** 升级只要求“有一个活着的 acbl 在消费信箱”，不要求“有人会重拉 acbl”。`capabilities.json` 里的同类字段降级为 `managed_by` 诊断信息（§4.3）。
5. **组件管理者不变，但耦合解开。** acbl 仍管 bridge（核心组件）与可选预设组件（dsh / drivers / opencode）；bridge 重启只跟 payload/升级链走，不与 acbl 自身版本绑定。**acbl 自身升级只重启 acbl**：bridge 继续服务，由新 acbl 接管（adopt，§4.7）；预设组件没有独立身份，仍随 acbl 重启。
6. **契约版本化是新的硬要求（B-12 已实现）。** payload 根目录用 `acb-contract.json` 声明 `contract` / `min_acbl` / `layout` / `commands`，acbl 只支持 `1..=1`；不匹配就拒绝切换、保留当前版本（§6.4）。
7. **presets 是“怎么起”，不是“装什么”，且按安装方式拆分。** 预设放在 `deploy/presets/<component>/<component>_<source>.sh`（如 `dsh_nix` / `dsh_npm` / `opencode_nix`），由清单 `presets.toml` 登记成“带开关的服务列表”；同一组件只能启用一个来源。预设**随 acbl 安装分发**，不随 payload；组件本体（dsh / opencode / node）仍由用户安装（§5）。
8. **谁启动 acbl：三类并列，由安装器显式选择**——用户终端手工、systemd 开机自启、systemd 只守护不自启；无 systemd 时由用户自管。区别只在于“acbl 崩溃/78 后由谁重拉、日志归谁、有无 cgroup”（§4.3），不做环境探测。
9. **组件表不做自动热加载**，只提供控制台“重新加载组件表”按钮；失败保留旧表，不允许停一半（§4.6）。
10. **acbl 是观察者 + 恢复者，不是 bridge 的所有者。** acbl 停止、崩溃、升级期间 bridge 继续运行；新 acbl 启动先探测身份再接管，禁止盲目再起一个（§4.7）。

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
| 健康探针 | **单个** `health_url`；没有时用 `AlwaysHealthy` | `supervisor.toml.tmpl:5`；`lib.rs:125-128` |
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

另有一个要改的结构点：**当前 launcher 是从 payload 的 `bin/` 启动的**。`acbctl unit install` 的 `ACB_UNIT_LAUNCHER_BIN` 默认从 `bin_dir` 派生，而 `resolve_bin_dir` 优先解析到 `profile/current/bin`（`crates/acb/src/unit.rs:539-549`、`crates/acb/src/profile.rs:209-217`）。这等于把“监管者”放进了它要监管的 payload：每次换代都跟着换监管者，也就必须重启监管者。第七轮要做的就是把这件事拆开。

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
| **acbl 自身** | ACB 包，独立安装（NixOS 由 nix-config） | 安装时确定（unit / init / 手工） | 低频：系统 switch 或替换二进制后重启 acbl |
| **核心组件** `bridge` | payload generation（ACB 构建） | ACB（supervisor 核心） | 有：LKG、回滚、升级链 |
| **预设组件** `dsh` / `drivers` / `opencode` | dsh / opencode 由用户安装；drivers 的实现来自 payload | ACB 提供预设清单与启动脚本；用户可启用、覆盖、禁用 | 无：不参与 LKG，不触发升级链 |
| **自定义组件** | 用户 | 用户写命令与声明 | 无 |

### 2.3 责任边界

- **acbl 负责**：拉起、健康探针、重启/退避、停止、状态落盘、日志归属、组件间启动顺序。
- **payload 负责**：提供 `bin/acb-bridge`、harness 适配层、drivers 扇出脚本与插件，并声明契约版本（§6.4）。
- **ACB 升级链负责**：payload 的版本切换、LKG、升级信箱、回滚。
- **用户负责**：外部组件的安装、升级、配置内容、密钥（组件自己的 env 文件）。
- **acbl 不做**：安装外部组件、解析它们的配置文件、替它们做版本回滚、调用 `nix` / `systemctl` / `npm`。

### 2.4 acbl 的安装与分发（静态入口）

**静态入口 = init/unit 永远指向一个稳定路径，不指向 generation。** 这样 payload 换代时，unit 里的 `ExecStart=` 不需要改，监管者也不会被替换。

| 场景 | 来源 | 入口路径 | 更新方式 |
|---|---|---|---|
| **NixOS** | nix-config 提供 acbl 包与 unit | `/run/current-system/sw/bin/acb-launcher`（或模块生成的固定路径） | `nixos-rebuild switch`；随系统升级，低频 |
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
  deploy/start-drivers.sh        # 纯扇出执行器，见 §5.5
  acb-contract.json             # 契约声明（§6.4，B-12 已实现）
```

**为什么不把 acbl 放进 payload：**

1. 监管者的生命周期必须比它监管的对象长；每次换代都换监管者，就必然要重启监管者，升级链会退化成“重启整栈”。
2. acbl 是低频变更的稳定件，payload 是高频迭代件；两者混在一个 generation 里，只能按高频的那个一起动。
3. 回滚 payload 时不应该把监管者也回滚到旧版本——旧 acbl 可能不认识新的状态目录/契约，反而制造故障。

**迁移提示（从现有部署切到新结构）**：先安装 acbl，把 unit 的 `ACB_UNIT_LAUNCHER_BIN` 指到静态入口，再从 payload 里移除 acbl 二进制。过渡期可以保留一代“什么都带”的 payload，但 unit 必须已经指向外部 acbl。

### 2.5 安装包规格（C-9，待交付）

**状态：规格已出，实现未开始。** 只产出 `tar.gz`，没有 deb / rpm / apk / Windows 包；服务注册由安装脚本或 `acbctl` 完成，不再有包管理器 `postinst` 兜底。

#### 2.5.1 制品清单

只出两个制品，文件名必须带 target triple，避免用户下错：

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
acbl/                       # 解包后的目录名；target triple 只出现在文件名里
  bin/acb-launcher  bin/acb-supervisor  bin/acbctl
  share/acb/units/          # unit 模板（供 acbctl unit install 渲染）
  share/acb/presets/        # 预设清单与脚本
  install.sh
  SHA256SUMS
  README.md  LICENSE
```

推荐安装到 `/opt/acbl`（系统级）或 `~/.local/opt/acbl`（用户级）：解包到目标父目录后，归档里的 `acbl/` 即为前缀。

```sh
# 系统级
sha256sum -c SHA256SUMS
sudo tar -xzf acbl-x86_64-unknown-linux-musl.tar.gz -C /opt
sudo /opt/acbl/install.sh --prefix /opt/acbl --scope system --autostart 0

# 用户级：把 -C /opt 换成 ~/.local/opt，--scope system 换成 user
```

`install.sh` 只做四件事：

1. 校验前缀与目标架构（gnu / musl 选错时给出明确提示，不静默继续）；
2. 把 `bin/` 与 `share/acb/` 放到位；
3. 调 `acbctl unit install --scope user|system --autostart 0|1 --root <unit-dir>`，由 `acbctl` **渲染** `share/acb/units/` 里的模板，而不是直接复制模板；
4. 打印后续步骤（配置、密钥、`profile seed`、启动命令），**不执行它们**。

- **unit 文件从哪来**：模板随 tar.gz 分发在 `share/acb/units/`，最终 unit 由 `acbctl unit install` 渲染。没有包管理器，就没有 `postinst`；`install.sh` 与手工执行 `acbctl unit install` 等价。
- **是否 enable / start**：只有显式 `--autostart 1` 才写 `[Install]` 并执行 `systemctl enable`；默认只写文件 + `daemon-reload`，不启动。三种启动方式的选择权仍归部署者（§4.3）。
- **unit 必须是 `KillMode=process`**（§4.7.1），否则 `systemctl stop` 会连带杀掉 bridge。

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
A-8（unit 入口指向静态 acbl） → B-12（契约 v1 + payload 校验收窄，已完成） → C-9（打包实现与发布）
```

- 规格可先出（本节），**实现被 A-8 阻塞（B-12 已实现）**：unit 仍指向 `profile/current/bin` 时，安装出的 unit 会把 `ExecStart` 指回 payload；契约文件已由 B-12 落地，发布说明可直接引用 `contract` / `min_acbl` 区间。
- C-9 还依赖 2.5.2 的构建矩阵定案（gnu 基线、musl 动态 bridge）与 2.5.5 的边界评审通过。
- 发布物必须附：版本号、target triple、链接方式、最低 glibc 基线、支持的 `contract` 区间、SHA256 校验和。

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
| G13 | unit 默认把 launcher 指向 `profile/current/bin` | `ACB_UNIT_LAUNCHER_BIN` 默认从 `bin_dir` 派生（`unit.rs:539-549`） | `acbctl unit`（默认值改为“静态入口优先”） | 全组合 |
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
| acbl 自身崩溃 / 被系统 switch 重启 | 非 75 | init 的 `Restart=always` 或 `acbctl run` 父循环拉起；bridge 不受影响，新 acbl 先探测身份再 adopt（§4.7） |
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
| **B. systemd 开机自启**（`enable --now acb-launcher-user.service` / 系统级） | systemd：`Restart=always` 拉起普通崩溃；`RestartPreventExitStatus=78` 停在 failed | journald（stdout/stderr）或模板里的 `StandardOutput=append:` 文件；轮转由 journald / logrotate 负责 | **有**（独立 service cgroup，`KillMode=process`；stop 不连带 bridge） | 否 |
| **C. systemd 只做进程守护、不开机自启**（装 unit 但不 `enable`，或 `ACB_UNIT_AUTOSTART=0`） | 同 B，但只有 `systemctl start` 之后才存在 | 同 B | **有**（同 B） | 否 |
| **D. 无 systemd 自管**（runit / s6 / openrc / 容器 entrypoint / `acbctl run --mode=launcher`） | 用户的 init 或 `acbctl run` 父循环：普通崩溃重拉、78 停止（容器里 78 转成正常停止） | 由 init / 容器日志驱动；`acbctl run` 下是 `run.json` + launcher 自身输出；轮转由 init / runtime 负责 | **取决于 init**（runit/s6 通常有；裸前台没有） | 否 |

**共同点：升级链在四种方式下都不重启 acbl。** 它只在 acbl 进程内停旧 bridge、切指针、起新 bridge。方式之间的差别只剩“acbl 自己崩溃/放弃时谁把它拉起来、日志去哪、有没有 cgroup”。

安装器要预留的接口（避免以后返工）：

- `ACB_MANAGED_BY`：`manual | systemd-user | systemd-system | container | custom-init | acbctl-run`，写进 `capabilities.json`，**只用于诊断与生成文档**，不参与升级准入。
- `ACB_UNIT_AUTOSTART`：0/1，决定是否生成 `[Install]`（对应方式 B / C）。
- `ACB_UNIT_SCOPE`：`user | system`，决定 unit root、`loginctl enable-linger`、以及系统级 `User=` 的必要性。
- `ACB_LOG_TARGET`：`journal | file | inherit`，决定 unit 的 `StandardOutput=` 和安装器是否创建日志目录；日志轮转责任随 target 明确归属。
- `ACB_UNIT_LOG_DIR` / `ACB_UNIT_ENV_FILE`：日志与密钥路径，安装器只写模板占位，不在 unit 里写死。
- `acbctl unit install --autostart=0|1 --scope=user|system`：一次性把 A/B/C 的差异表达清楚；D 由文档与示例覆盖，不塞进同一开关。

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

#### 4.7.3 崩溃 vs 正常停止

| 事件 | acbl | bridge | 谁恢复 |
|---|---|---|---|
| **acbl 崩溃**（panic / OOM / `kill -9`） | 进程消失 | **继续运行**，7700 不断 | init 按 `Restart=always` 重拉 acbl；新 acbl adopt 现有 bridge |
| **acbl 正常停止**（`systemctl stop`、升级 acbl、SIGTERM） | 前台进程退出 0 | **继续运行**，服务不中断 | 升级：系统层/安装脚本启动新 acbl，adopt 旧 bridge；手工 stop：等人再 start |
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

实现阶段的验证：`kill -9` acbl 后 bridge 仍在 7700 响应；新 acbl 启动后 adopt、不产生第二个进程；bridge 属于别的 generation 时 acbl 拒绝接管并 78；`systemctl stop acb-launcher` 后 bridge 仍健康。

---

## 5. presets 的形态

### 5.1 目录与命名：按安装方式细分

预设的维度是**安装方式**，不是组件名。同一个 dsh 有 nix / npm / guix / scoop 四种来源，就对应四个预设脚本：

> **归属：预设随 acbl 安装分发，不随 payload。** 仓库里的源目录是 `deploy/presets/`；安装后位于 acbl 包的只读共享目录（例如 `share/acb/presets/`），运行时不从 generation 里读。payload 只带 drivers 实际执行的扇出脚本与 harness 实现。

```text
deploy/presets/
  README.md
  presets.toml              # “带开关的服务列表”清单
  dsh/
    dsh_nix.sh              # 用 nix（nix-config / llm-agents 等）装出来的 dsh
    dsh_npm.sh              # npm 安装
    dsh_guix.sh             # guix 安装
    dsh_scoop.sh            # Windows / scoop 安装
  opencode/
    opencode_nix.sh
    opencode_npm.sh
  drivers/
    drivers_acb.sh          # 指向 payload 里的扇出脚本，只有这一个来源
```

命名规则：

- 预设 id = `<component>_<source>`，脚本文件名与 id 一致。
- `source` 词表固定：`nix` / `npm` / `guix` / `scoop` / `homebrew` / `distro` / `container` / `acb`；不认识的来源报错，不允许自由发挥。
- 每个 `.sh` 只负责**怎么起**（解析二进制路径、设端口、等就绪），不负责安装。
- 预设与 `presets.toml` 属于 **acbl 安装包**，版本随 acbl 走；新增预设要走 ACB 仓库 PR，不通过 payload 换代下发。
- 用户自写的组件不放进 `deploy/presets/`，直接写组件表；那属于 `source = "custom"`。

### 5.2 清单：带开关的服务列表

`deploy/presets/presets.toml` 把脚本登记成条目；运行时开关在组件表里：

```toml
schema = 1

[[preset]]
id = "dsh_nix"
component = "dsh"
source = "nix"
script = "dsh/dsh_nix.sh"
description = "dsh 由 nix 提供（nix-config 的 dsh.nix / llm-agents 类 flake）"
default_enabled = false

[preset.process]
cwd = "@ACB_UNIT_STATE_DIR@"
env_file = "@ACB_UNIT_ENV_FILE@"
env = { DSH_PORT = "3080" }
kill_mode = "group"

[preset.health]
kind = "http"
url = "http://127.0.0.1:3080/healthz"
startup_grace_secs = 20

[preset.restart]
policy = "on-failure"
backoff_secs = [1, 2, 4, 8, 30]
max_consecutive_failures = 5

[[preset]]
id = "opencode_npm"
component = "opencode"
source = "npm"
script = "opencode/opencode_npm.sh"
description = "opencode v2 由 npm 安装"
default_enabled = false
```

字段规则：

- `id` 全局唯一；`component` 决定互斥组；`source` 只用于展示与解释，**不驱动任何安装动作**。
- `script` 相对 **acbl 预设根**（仓库里是 `deploy/presets/`，安装后是 acbl 包的共享目录），必须存在且可执行；缺失在启动/重载校验时报错。
- 变量替换复用现有 `@ACB_UNIT_*@` 白名单（如 `@ACB_UNIT_STATE_DIR@` / `@ACB_UNIT_ENV_FILE@` / `@ACB_UNIT_OPENCODE_PORT@`），不做任意 shell 展开；未知变量报错。
- 清单里只登记“存在哪些预设”，不记录“当前启用哪个”；启用状态在 `supervisor.toml`。

### 5.3 运行配置：组件表与互斥

`supervisor.toml` 增加可选的组件表（bridge 仍用现有 `[supervisor]` 字段，保持兼容）：

```toml
# 每个组件只启用一个来源
[[component]]
name = "dsh"
preset = "dsh_nix"
enabled = true
env = { DSH_BIN = "/run/current-system/sw/bin/dsh" }

[[component]]
name = "opencode"
preset = "opencode_npm"
enabled = true
env = { ACB_OPENCODE_BIN = "/usr/local/bin/opencode2" }

# 用户自写：不引用 preset，直接给命令
[[component]]
name = "my-sidecar"
enabled = true
[component.process]
command = ["/usr/local/bin/my-sidecar", "--port", "7788"]
cwd = "/var/lib/my-sidecar"
kill_mode = "group"
[component.health]
kind = "tcp"
url = "127.0.0.1:7788"
```

规则：

- `component.name` 与预设的 `component` 必须一致；不一致报错（防止把 dsh 的预设挂到 opencode 名下）。
- **同一组件只能启用一个来源**：同一个 `component` 出现两个 `enabled = true` 的条目 → 启动/重载校验失败，报错列出所有候选（如 `dsh_nix` / `dsh_npm`）。
- 未启用的来源可以同时存在（`enabled = false`），方便切换；切换 = 改 `preset` 字段（外加必要覆盖），由重载按钮或重启 acbl 生效。
- `preset` 与 `process.command` 二选一；同时给报错；用 `process.command` 即 `source = "custom"`，同样受“一组一件”约束。
- `enabled` 缺省取预设的 `default_enabled`（内置预设一律 `false`）。
- 覆盖语义：`env` 按 key 合并；`process` / `health` / `restart` 三张表整体覆盖。
- 组件名全局唯一；与 `bridge` 冲突直接报错；未知字段拒绝。

### 5.4 与 launcher 托管清单的对应

| 层 | 文件 | 谁写 | 作用 |
|---|---|---|---|
| acbl 包 | acbl 安装目录（`bin/` + `share/acb/presets/`） | ACB 发布 / nix-config | 监管者本体、`acbctl`、unit 模板、预设清单与脚本 |
| payload | `profile/generations/<id>/root/` | ACB 构建 | `bin/acb-bridge`、`harnesses/`、drivers 扇出脚本、插件；根目录含 `acb-contract.json` |
| 预设清单 | `<acbl>/share/acb/presets/presets.toml` | acbl 包（仓库源 `deploy/presets/`） | 有哪些“组件 × 安装来源”的预设、默认开关 |
| 预设脚本 | `<acbl>/share/acb/presets/<component>/<component>_<source>.sh` | acbl 包 / 用户 PR | “怎么起”的实现 |
| 运行选择 | `supervisor.toml` 的 `[[component]]` | 用户/部署工具/控制台（长期） | 启用哪个来源、覆盖哪些字段 |
| 运行时状态 | `state/components/<name>.json` | acbl | starting/healthy/restarting/failed、pid、最近错误 |
| 日志 | `log_dir/<name>.log` | acbl 重定向 | 每组件独立，避免混在 launcher 日志里 |
| 升级信箱 | `state/upgrade/*` | 现有机制 | 只针对 bridge，不扩展到组件 |

### 5.5 drivers 到底是什么（代码核实）

`acb-drivers.service` 跑的是 `deploy/start-drivers.sh`。它不是“第 4 个服务”那么简单，而是**按会话扇出的桥—dsh 适配器**：

| 事实 | 证据 |
|---|---|
| 单元先要桥：`After=acb-bridge.service`、`Wants=acb-bridge.service` | `deploy/units/systemd/acb-drivers.service.tmpl:4-5` |
| 每个驱动进程做三件事：连桥（环回口）+ 起本地 MCP 端点 + 驱动一个 dsh 会话 | `deploy/start-drivers.sh:4-6` |
| 会话写死在脚本里，一条会话一个进程：`dsh-qas:3101`、`dsh-dev:3103`、`dsh-snowluma-test:3104`、`dsh-group2:3102`（格式 `会话 id : MCP 端口 : preset 名`） | `deploy/start-drivers.sh:69-74` |
| 每个进程的启动环境：`ACB_ENDPOINT=127.0.0.1:7700`（桥）、`ACB_TOKEN`（该会话的接入 token）、`ACB_HARNESS=dsh`、`ACB_PRESET`、`ACB_MCP_PORT`、`DSH_BASE_URL=http://127.0.0.1:3080` | `deploy/start-drivers.sh:84-94` |
| 会话 token 由 `acbctl secrets resolve --session` 解析，脚本里不写死 | `deploy/start-drivers.sh:61-64` |
| 一条会话的 token 解析失败就跳过该会话，其它照起 | `deploy/start-drivers.sh:78-82` |
| 扇出后只 `wait`，没有 per-driver 重启/健康 | `deploy/start-drivers.sh:96-97` |
| dsh 本体是另一个进程：`start-dsh.sh` 以 `exec "$DSH_BIN" … --port 3080` 常驻 | `deploy/start-dsh.sh:34` |
| bridge 是 7700 的接入端；opencode 是独立的 4097 serve，与 drivers 无直接关系 | `deploy/opencode-serve.sh:21`；`supervisor.toml.tmpl` |

分工：

- **bridge**：唯一的 ACB 核心，管会话/路由/存储；driver 通过 `ACB_ENDPOINT=127.0.0.1:7700` 反向接入。
- **dsh**：外部 harness 本体（3080），driver 通过 `DSH_BASE_URL` 操作它。
- **drivers**：桥与 dsh 之间的会话级适配层；**一会话一驱动进程**，不是 harness、不是桥。
- **opencode**：另一个独立 harness，不走 drivers。

为什么 v1 作为**单个组件**：

1. 所有驱动共享同一份输入（**应当**来自配置/库的会话列表、token 来源、dsh 地址、MCP 端口段），一条变更就要整体重启，拆成 4 个组件不会带来独立重启收益。
2. 脚本本身是 fan-out + `wait`，systemd 视角已经是“一个 unit 管 N 个进程”；把它包成一个组件与现状一致，改动最小。
3. 拆成每会话组件需要新增“会话 → 组件”的映射、每会话健康探针和依赖，属于后续能力（§8 开放问题）。

代价与备注：单组件下**单个驱动死掉不会被 acbl 发现**（`wait` 不会因一个子进程退出而返回）。v1 用两个缓解手段：

- 健康探针至少探一个代表端口（或桥的会话状态）；
- 预设脚本改成“任一驱动退出就整体退出”，让 acbl 的组件重启策略接管（脚本级修复，不改 acbl 模型）。

**已定约束：会话列表不得写死在脚本里。** 当前 `start-drivers.sh:69-74` 把 4 条会话（`dsh-qas:3101` 等）写死在随 payload 分发的脚本里，改一个会话就要改脚本、动版本，与“配置单一数据源、能进库的进库”的原则冲突。改造后：

- 会话来源必须是**配置/库**，并且与现有会话定义（`config.toml` 的 `[[sessions]]` / 运行库里的会话记录）**同一数据源**，不另建一套驱动专用清单；
- 脚本退化为**纯扇出执行器**：只负责“读会话列表 → 每条会话起一个驱动进程 → 等待”，不再内置任何会话 id、端口或 preset 名；
- MCP 端口与 preset 名从会话定义或派生规则得到，仍由同一份配置决定，不在脚本里维护第二份。

在 v1 组件化里的位置：**不阻塞单组件形态**（单组件只要求“一个组件管 N 个驱动进程”），但属于 `drivers` 组件的改造范围——组件化落地时必须同批完成，不能把写死的会话表带进 preset 脚本。

### 5.6 失败与重启策略由谁定

- **预设给默认值**：它最了解自己要多久就绪、失败后重试是否安全（例如 opencode 首次初始化慢，`startup_grace_secs` 要大）。
- **用户可覆盖**：`restart.policy`、退避、上限、健康检查方式。
- **acbl 强制全局上限**：无论组件怎么配，acbl 都保留一个全局重启风暴保护（例如 60s 内总重启次数上限），避免多个组件的退避叠加把机器打满。
- **acbl 不替组件做业务判断**：组件 `failed` 后只记录并暴露状态；是否重新启用由用户或控制台操作。

### 5.7 内置服务列表（v1 建议）

这就是用户说的“带开关的服务列表”：每个组件一行，来源可切换，同一时刻只有一个开关亮。

| 组件 | 可选预设 id | 来源 | 现状脚本 | 关键参数 | 备注 |
|---|---|---|---|---|---|
| `dsh` | `dsh_nix` | nix | `deploy/start-dsh.sh`（偏 nix 实现） | `DSH_BIN`、端口 3080 | 脚本自己 source `deploy/acb.env`，预设改为显式 `env_file` |
| `dsh` | `dsh_npm` | npm | 待补 | `DSH_BIN`、端口 3080 | 需要 npm 全局安装路径 |
| `dsh` | `dsh_guix` | guix | 待补 | `DSH_BIN` | 路径来自 guix profile |
| `dsh` | `dsh_scoop` | scoop | 待补 | `DSH_BIN` | Windows；受 C-5 限制 |
| `opencode` | `opencode_nix` | nix | `deploy/opencode-serve.sh`（偏 nix 实现） | `ACB_OPENCODE_BIN`、4097、密码 | 脚本内部 `serve &`，必须 `kill_mode = "group"` |
| `opencode` | `opencode_npm` | npm | 待补 | 同上 | npm 安装的 `opencode2` |
| `drivers` | `drivers_acb` | acb | `deploy/start-drivers.sh` | `NODE`、会话列表（改造后从配置/库读取） | fan-out，必须 `kill_mode = "group"`；说明见 §5.5 |

- 只有 `drivers_acb` 的**执行体**是 ACB 自己的脚本（`source = acb`），它位于 payload；预设条目本身仍在 acbl 包里，只负责指向 payload 的扇出脚本。其余组件的预设都只是“按某个安装来源找二进制并起服务”。
- 现有三个脚本里的 nix 默认路径（`opencode-serve.sh:16,25`、`start-drivers.sh:17`）要改成“显式变量优先、找不到就报错”，与矩阵的 A-1/A-2/A-7 一致。drivers 脚本随 payload，dsh / opencode 预设随 acbl，二者改动落在不同分发单元。
- 每个组件同一时刻只能启用一个来源；用户切换来源就是换 `preset` 字段，必要时补一个 `env` 覆盖。

### 5.8 控制台：短期只读，长期受限编辑

**短期（v1）：只读展示。** 控制台只展示，不提供写入：

- 有哪些预设（来自 `presets.toml`）：`id` / `component` / `source` / `description` / `default_enabled`；
- 当前启用哪个：来自 `supervisor.toml` 的组件表；
- 健康如何：来自 `state/components/<name>.json`（状态、pid、重启次数、最近错误、启动时间）。

**长期：允许编辑，但边界写死。**

允许的动作只有三类：

1. **从预设集合里选择**：每个组件一个下拉框，只能选 `presets.toml` 里登记的 `id`；
2. **开关**：启用/停用某个组件；
3. **覆盖有限字段**：白名单字段（例如 `env` 里允许的 key、`cwd`、`health.url`、端口），逐项校验；不在白名单的字段一律拒绝。

明确**不允许**：

- 填写任意命令 / 任意脚本路径 / 上传脚本；
- 新增预设（新增预设只能改仓库、走 PR）；
- 修改 bridge 的 `[supervisor]`、profile、LKG、升级请求；
- 修改密钥与环境文件内容；
- 修改插件、wake / delivery 等业务策略；
- 修改 systemd unit、日志路径、acbl 启动方式。

长期编辑必须具备的前置：

- **鉴权**：控制台管理员 token + 角色校验（与现有 `POST /api/upgrade` 同级），未授权直接 403；
- **二次确认**：展示 diff（启用/停用、来源切换、字段变化），用户确认后才提交；
- **变更审计**：记录谁、何时、旧值/新值、结果（成功/回滚/失败），密钥类字段脱敏；
- **服务端校验**：多来源互斥、白名单字段、脚本存在性、变量可替换；不依赖前端；
- **失败回滚**：与 §4.6 的重载语义一致，校验失败不动运行中的组件，应用失败回滚到旧表；
- **并发控制**：同一时间只允许一个编辑/重载任务。

### 5.9 安全边界

- 预设脚本在 acbl 的权限下运行；不引入 setuid、不调用 `sudo`。
- 组件表只能由部署者写（`supervisor.toml` 在 state 目录）；控制台长期只允许“选预设 / 开关 / 白名单字段”，不允许任意命令（§5.8）。
- 用户自写组件时，`command` 必须是可执行文件路径或脚本路径，不接受 shell 字符串拼接；参数走 `args` 数组。

---

## 6. 与 nix、升级链的关系

### 6.1 nix 路线：先独立构建，成功再替换

- `nix build .#acb-bridge -o result` 在独立目录产出；成功后再 `profile add` → `profile switch`，失败不动当前 `current.json`。这与现有 `profile` 的 staging + 原子 rename 一致。
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

- 现状默认值是 `<bin_dir>/acb-launcher`，而 `bin_dir` 优先解析到 `profile/current/bin`（`crates/acb/src/unit.rs:539-549`、`crates/acb/src/profile.rs:209-217`）。新结构下这个默认值必须改，或在安装时显式覆盖 `ACB_UNIT_LAUNCHER_BIN`（G13）。
- 部署校验：`acbctl unit doctor` + `test -x <静态入口>`，并额外确认 `ExecStart` 路径不在 `profile/generations/` 下。
- payload 不再需要 `acbctl` / `acb-launcher` / `acb-supervisor`；`profile seed` 的必带清单要相应收窄（G14）。

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
- **旧 payload 没有 `acb-contract.json` 时如何处理：待定。** 实现里现有一段过渡放行逻辑
  （缺失按 `contract = 1` 处理并告警），但该策略是否保留尚未拍板，不作为已定契约；确定后
  回填本节。

### 6.5 两条升级路径对照

| 路径 | 触发 | 换什么 | 重启谁 | 频率 | 走升级链吗 |
|---|---|---|---|---|---|
| payload 升级 | `acbctl upgrade` / 控制台 / `profile switch` | generation 里的 bridge、harness、drivers 实现 | 只重启 bridge（acbl 原地） | 高频 | 是 |
| acbl 升级 | `nixos-rebuild switch` / 包升级 / 替换二进制 | acbl 自身、预设、unit 模板 | 重启 acbl；bridge 不重启（新 acbl adopt），预设组件随 acbl 重启 | 低频 | **否** |

这直接回答“守护者为什么需要重启”：**它不需要因为 payload 换代而重启；只有在它自己的版本/启动方式变了时才重启**。

---

## 7. 落地拆分（只列步骤，不改代码）

| 步骤 | 内容 | 依赖 | 验证 |
|---|---|---|---|
| P0 | **拆分分发**：acbl 独立包（NixOS 由 nix-config 提供）+ payload 收窄 + `acb-contract.json` 声明与校验（B-12 已实现） | 无 | acbl 不在 generation 里；unit 指向静态入口；不兼容 payload 被拒绝切换 |
| P1 | 组件表 schema + `supervisor.toml` 解析（bridge 之外的 `[[component]]`） | P0 | 配置文件能加载；非法字段报错；不启动任何新组件时行为与现在一致 |
| P2 | supervisor 多组件 Runner：每组件独立 `Option<Child>`、进程组、独立停止 | P1 | 起 2 个假组件，kill 一个不影响另一个；脚本后台孙进程随组停止 |
| P3 | 每组件健康探针 + 退避 + 独立状态文件 | P2 | 组件失败重试到上限后标 failed，acbl 不退出；`state/components/*.json` 可读 |
| P4 | 预设清单 + 多来源装载 + 互斥 + 覆盖规则 | P1 | 同一组件两个来源同时启用会被拒绝；启用/禁用/覆盖生效；用户自写组件不用 preset 也能跑 |
| P5 | 预设脚本去 nix 默认路径（dsh/drivers/opencode，含按安装方式拆分的脚本）；drivers 会话列表改从配置/库读取，脚本退化为纯扇出 | P4 | 非 NixOS 上按变量能找到二进制；缺变量时明确报错；改会话只改配置、不改脚本 |
| P6 | `acbctl status` 展示组件表；日志分流；控制台只读展示 | P3 | 状态里能看到每个组件的 pid/健康/重启次数/最近错误 |
| P7 | systemd 适配器与自定义 init 文档对齐（三种启动方式） | P2 | 78 仍由外部单元识别；升级不再用 75；组件失败不触发 75/78；安装器参数覆盖 A/B/C |
| P8 | 控制台“重新加载组件表”按钮 + 校验/回滚语义 | P1 P4 | 非法新表不动运行中组件；变更失败回滚；返回逐组件结果并写审计 |

---

## 8. 决策与仍待决

**已定：**

1. **acbl 独立安装**：不进 payload；NixOS 由 nix-config 提供，非 NixOS 用 nix profile / cargo / 预编译 tar.gz（gnu 或 musl，§2.4 / §2.5）。
2. **payload 升级不重启 acbl**：只切 payload、原地重启 bridge；acbl 自身的升级走系统层，只重启 acbl，bridge 继续运行并被新 acbl adopt（§4.2 / §4.7 / §6.5）。
3. **75 退役、78 保留**：75 只作为兼容保留值，不再由升级路径发出；78 表示“现场坏了、需要人”（§4.2）。
4. **`restart_owner` 取消**：不再作为升级门禁；`self` 档取消，改 `managed_by` 诊断（§4.3）。
5. **契约版本化（B-12 已实现）**：声明在 payload 根 `acb-contract.json`；acbl 只支持 `contract = 1`，三处校验点分别拒绝 add / 拒绝切换 / 启动退 78（§6.4）。
6. **谁启动 acbl**：三类并列，安装器参数选择——A 用户终端手工、B systemd 开机自启、C systemd 只守护不自启；无 systemd 由用户自管（D）。差异见 §4.3，接口用 `ACB_MANAGED_BY` / `ACB_UNIT_AUTOSTART` / `ACB_UNIT_SCOPE` / `ACB_LOG_TARGET` 预留。
7. **热加载**：不做自动热加载；只做控制台按钮，加载对象是 `[[component]]`，失败保留旧表（§4.6）。
8. **drivers**：v1 作为单个组件（`drivers_acb`），理由是代码事实（§5.5）；单驱动死亡要通过“脚本整体退出”暴露给 acbl。**会话列表必须从配置/库读取（与 `[[sessions]]` 同一数据源），不得写死在 preset 脚本里**。
9. **控制台**：短期只读；长期允许“选预设 / 开关 / 白名单字段”，不允许任意命令，前置条件见 §5.8。
10. **bridge 生命周期独立于 acbl**：acbl 是观察者 + 恢复者；acbl 停止 / 崩溃 / 升级期间 bridge 继续运行；重启后先探测身份再 adopt，禁止盲起；systemd unit 用 `KillMode=process`（§4.7）。

**仍待决：**

1. **旧 payload 没有 `acb-contract.json` 时的处理策略**：实现里现有一段“按 `contract = 1` 放行并告警”的过渡逻辑，但是否保留、何时删除尚未拍板（§6.4）。`N-1` 的并行窗口等 v2 出现时再定。
2. **默认推荐哪种启动方式**：三类都支持，但安装器默认给 A / B / C 哪一个？（建议 B，但需要用户确认是否接受 linger 与 journald）。
3. **系统级 vs 用户级**默认 scope，以及系统级 `User=` 是否由安装器强制生成。
4. **acbl 的非 nix 分发渠道**：预编译 `tar.gz`（gnu / musl）的构建流水线、校验和与签名由谁维护（C-4 / C-9）。
5. **每会话拆组件**：drivers 何时从单组件拆成“每会话一个组件”，取决于是否需要单独重启某个会话。
6. **npm / guix / scoop 的预设脚本谁补**：v1 可以先只交付 `dsh_nix` / `opencode_nix` / `opencode_npm` / `drivers_acb`，其余欢迎 PR。
7. **控制台长期编辑的落地时间**：鉴权、二次确认、审计都具备后再开写入。
8. **Windows 上的进程组与信号**：没有 `setsid`/进程组语义，需要 Job Object 或“包装器负责杀整棵树”；在 C-5 里一起处理。
9. **无守护窗口是否另设看门狗**：当前接受“acbl 停机期间 bridge 死亡无人拉起”（§4.7.5）；是否引入 systemd 直接托管 bridge 或独立看门狗，未决策。
10. **aarch64 制品是否与 x86_64 同期产出**：C-9 默认只出 `x86_64-unknown-linux-{gnu,musl}`（§2.5.1）。

---

## 9. 一句话结论

**acbl 是独立安装的静态监管者，payload 是它监管的迭代内容：升级只切 payload、只重启 bridge，acbl 全程不动。** 因此不需要 `restart_owner`，也不需要“退出 75 等外部重拉”；75 退役为兼容值，78 保留。要变成组件管理者，缺的仍是多组件表、进程组、每组件健康/重启策略和预设装载；预设按安装方式拆分并随 acbl 安装，同一组件只能启用一个来源。代价是 acbl↔payload 的接口必须版本化：payload 根 `acb-contract.json` 声明 `contract` / `min_acbl` / `layout` / `commands`，acbl 只支持 `1..=1`（B-12 已实现），否则两个独立分发单元会悄悄失配。

**acbl 还是 bridge 的观察者与恢复者，不是所有者：** bridge 的生命周期独立于 acbl，acbl 停止 / 崩溃 / 升级期间 bridge 继续服务，新 acbl 启动先按身份端点探测再 adopt，禁止盲起第二个实例；无守护窗口是被接受的代价（§4.7）。分发上只出 `tar.gz`：gnu 制品钉死最低 glibc 基线，musl 制品里的 acbl 全静态，但 **bridge 在 musl 上必须是动态链接**（`.so` 插件靠 `dlopen`，静态 musl 不支持动态加载），所以 acbl 与 payload 必须按同一 libc 家族配对交付（§2.5）。
