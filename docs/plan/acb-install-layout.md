# 可移植安装布局：Windows 指针、非 nix 自建安装与 gcroot

状态：设计/评估，未改代码。范围只覆盖指针形态与安装来源，不改现有
`current.json` 单写者、LKG、75/78 契约。

## 0. 结论

| 环境 | 执行真值 | 投影 | 现状 |
|---|---|---|---|
| Unix | `state/supervisor/current.json` | `profile/current` 软链（相对目标，原子 rename） | 已实现 |
| Windows | `state/supervisor/current.json`（不变） | 优先 NTFS junction 到绝对路径；不可用时写 `profile/current.pointer` 文本；两者都失败则只保留真值 | junction / pointer 均未实现 |
| 无 nix | `state/supervisor/current.json`（不变） | 与 Unix 相同的软链，或用 `copy` 策略的真实目录 | 已实现（Unix）；Windows 见上 |

三句话结论：

1. **不要反过来让 junction 当指针**。junction 不能原子替换、不跨卷、非 NTFS / 组策略禁用时直接不可用；把它降为“可重建的投影”，真值继续留在 `current.json`，才和现有 journal / 启动对账 / 75/78 契约兼容。
2. **非 nix 自建安装不是第三种策略**，它就是现有的 `copy` 策略（暂存目录 → 快照 + 逐文件 sha256 manifest → `generations/<id>/root` → 翻指针）。缺的是**构建来源元数据**和**平台相关的 payload 校验清单**，不是新的 payload 链接方式。
3. **`nix build` 的 `result` 确实是 GC 根**，但它是“间接根”：根指向 `result` 这个软链，软链被删或被下一次 `nix build` 改指向，旧输出就失去保护。`pins.rs` 当前的第一个候选 `~/.local/state/nix/gcroots` 在本机这种**多用户 Nix** 下不会被扫描，却返回 `PinOutcome::Gcroot`——这是一个“假成功”，需要修。

---

## 1. 现状核查（代码层）

### 1.1 Unix：真值 + 投影已经是单写者

- `ActivationWriter::commit` 的提交顺序：写 journal → 原子写 `current.json`（真值）→ 写投影 → 删除 journal。投影失败会把 `current.json` 和投影一起回滚，见
  `crates/supervisor/src/pointer.rs:256-294`。
- 投影写入是“同目录临时软链 + `fs::rename` + `fsync_dir`”，目标为相对路径
  `generations/<id>/root`，见 `crates/supervisor/src/pointer.rs:390-421`。
- 启动对账 `repair_projection` 比较 `fs::read_link(link)` 与 journal 里记录的目标，缺失或不等就重写，见
  `crates/supervisor/src/pointer.rs:308-338`。
- 运行期 launcher 只读 `current.json`，不读投影，见
  `crates/launcher/src/main.rs:82-113`。这意味着投影在运行期是“诊断/便利”角色，不是执行依赖。

### 1.2 Windows：投影是空实现，读取路径却依赖它

- `write_projection` 在 `#[cfg(not(unix))]` 分支直接 `Ok(())`，什么都不写：
  `crates/supervisor/src/pointer.rs:416-420`。
- 生成 generation 的 `symlink` 在非 Unix 直接报
  `profile generation requires Unix symlinks`：
  `crates/acb/src/profile.rs:1776-1779`；pin 的软链同样，见 `crates/acb/src/pins.rs:125-139`。
- 读者依赖投影：
  - `status_at` 用 `fs::symlink_metadata(profile/current)` + `fs::read_link`，见 `crates/acb/src/profile.rs:152-199`；
  - `current_id` 同样读 `profile/current`，见 `crates/acb/src/profile.rs:1734-1748`；
  - `resolve_bin_dir` / `resolve_repo_dir` 返回 `profile/current/bin` 等路径，见
    `crates/acb/src/profile.rs:208-238`；
  - unit 渲染会把 `BIN_DIR` / `REPO_DIR` 换成 `profile/current` 下的路径，见
    `crates/acb/src/unit.rs:492`。
- 结论：今天在 Windows 上 `current.json` 能写，但 `profile current` / `status` 会报 `Missing`，
  `run` / `render` 走不到 profile 二进制。文档里的
  `current.pointer` / `resolve_current()` 只存在于
  `docs/plan/acb-upgrade-chain.md:421-432`，代码里没有（全仓库 grep 只在 docs 命中）。

### 1.3 非 nix 的 copy 路径已经存在

`profile add --strategy copy` 的流程：`validate_payload` → `copy_payload`（逐文件复制 + sha256）
→ 写 `manifest.json` → 写 `meta.json`，见
`crates/acb/src/profile.rs:952-1058`。manifest 校验是显式命令 `profile verify`，启动前不做，
见 `crates/acb/src/profile.rs:1261` 与 `:1599-1625`。

`validate_payload` 目前要求 `deploy/units/systemd`、`deploy/*.sh` 等 Unix 布局
（`crates/acb/src/profile.rs:1599-1621`），Windows 自建 payload 不会满足。

---

## 2. Windows 指针设计（参照 Scoop）

### 2.1 Scoop 的可借鉴点与不可照搬点

Scoop 的形态：

```
scoop/apps/<app>/<version>/      # 每个版本一个真实目录
scoop/apps/<app>/current         # 指向某个 <version> 的 NTFS junction
scoop/shims/<cmd>.cmd|.ps1|.exe  # PATH 里的入口，运行时经 current 找目标
```

可借鉴：版本目录不可变、`current` 只是指向某个版本的“便利指针”、入口与版本解耦。

不可照搬：

- Scoop 的 `current` 就是它的事实指针，删掉重来一次即可；ACB 还有 `current.json` 里的
  `CommandSpec`（程序、argv、generation id、config sha256），这是 launcher 重启后执行所需的真值，
  不能让 junction 承担。
- ACB 的 `current` 需要被 `status` / `run` / `render` / 启动对账读取；junction 的创建、替换、
  失效诊断都比软链更多坑，不适合作为唯一真值。
- ACB 没有“shim”需求：`acb-launcher` 本身是稳定入口，它读 `current.json`；`acbctl` 由用户安装并
  自行解析 profile。不要为每个 generation 生成 shim，那会变成第三份需要同步的指针。

### 2.2 推荐的 Windows 落法

**真值**：`state/supervisor/current.json`，跨平台完全不变。launcher / `acbctl run` 的
`load_current` 逻辑不变（`crates/supervisor/src/pointer.rs`）。

**投影（两层，按可用性降级）**：

1. `profile/current`：NTFS junction → `<profile>/generations/<id>/root` 的**绝对路径**。
   - junction 只能指向目录、只能同卷、只能 NTFS，目标必须是绝对路径；`mklink /J` 普通用户可建，
     不需要管理员或开发者模式。
   - Rust std 没有创建 junction 的 API，需要 Windows-only 的 Win32 调用
     （`DeviceIoControl(FSCTL_SET_REPARSE_POINT, IO_REPARSE_TAG_MOUNT_POINT)`）或引入
     `junction` 之类的小 crate。
   - 读取可继续用 `fs::read_link`（Windows 实现会解析 reparse point）；这一点要在目标 Windows
     版本做一次冒烟测试，不能只靠推断。
2. `profile/current.pointer`：JSON 文本，作为 junction 不可用时的投影，也是人可读的诊断文件：

   ```json
   {"schema":1,"generation":"gen-...","payload":"generations/gen-.../root",
    "updated_at_ms":1760000000000,"updated_by":"acb-launcher"}
   ```

   写法和 `write_atomic` 一样（同目录临时文件 + `fs::rename` + fsync）。Windows 上
   `fs::rename` 对已存在的**文件**使用 `MoveFileEx(MOVEFILE_REPLACE_EXISTING)`，可以替换；
   对**目录/junction** 不能替换，所以 junction 的更新是“删旧 + 建新”，不保证原子。

**为什么 junction 失败不能回滚真值**：现有 `commit` 在投影失败时会回滚 `current.json`
（`crates/supervisor/src/pointer.rs:271-290`）。这个语义对 Unix 软链成立，但不适用于
“junction 只是 best-effort 的 Windows”：

- junction 可能因为磁盘不是 NTFS、profile 放在网络盘、组策略禁用 reparse point 而失败；
- 这些都不应阻止一次升级提交，否则 Windows 上会出现“指针能写但永远升级失败”。

因此 Windows 的 `write_projection` 应返回 `Ok`（写入 pointer 文件成功即视为投影成功；junction
失败只是降级），只有 **pointer 文件也写不进去**（磁盘/权限错误）才返回 `Err`，走现有回滚。
建议把 junction 的结果记进日志，并在 `ProjectionRecord` 增加可选 `kind: "junction"|"pointer"`
（serde 可选字段，不 bump schema，旧读者忽略即可）。

**读取路径**：新增统一的 `resolve_current(root, state_dir)`，优先级：

1. `load_current(state_dir)`（真值）→ 目标 generation id / payload；
2. `profile/current` 存在且解析出的 id 与真值一致 → `Active`；
3. 否则若 `current.pointer` 存在且与真值一致 → `Active`（并在下次 `reconcile` 时尝试修复 junction）；
4. 否则 → `Missing`/`Broken`，错误信息里同时给出真值、junction 状态、pointer 状态。

`status_at` / `current_id` / `resolve_bin_dir` / `resolve_repo_dir` 都应改为基于这个解析结果，
而不是直接 `root.join("current")`。改动面小，但**是 Windows 能用的前提**。

**junction 替换的失败恢复**：因为真值先提交，投影后写，junction 的“删旧 + 建新”中途崩溃只会得到
“真值指向新代、junction 缺失或指向旧代”。启动时 `ActivationWriter::reconcile` 读真值并修复投影，
语义与今天的 `ProjectionRepaired` 相同；不需要新的恢复机制。

### 2.3 现在就能做 / 需要新机制

| 工作 | 类型 | 说明 |
|---|---|---|
| `current.pointer` 写入 + `resolve_current` 读取 | 现在能做 | 纯 Rust，无新依赖；同时把 `status_at`/`current_id` 改成真值优先 |
| junction 创建/替换/删除 | 新机制 | Windows-only Win32 FFI 或小依赖；失败降级为 pointer |
| 启动对账比较 junction 绝对目标 | 新机制（小改） | 现有 `repair_projection` 只比较相对目标 |
| Windows payload 必填清单 | 新机制（小改） | `validate_payload` 增加 `cfg(windows)` 分支 |
| Windows 进程树（Job Object） | 新机制 | 已在 `docs/plan/acb-upgrade-chain.md:418-432` 记为 C-5，和指针无关 |

---

## 3. 非 nix 自建安装

### 3.1 与 `copy` 策略的关系

用户描述的流程：

```
工具链编译 → staging 目录 → 校验 → 放进 generations/<id>/root → 翻指针
```

这与 `copy` 策略逐项对应：

| 流程步骤 | 现有实现 |
|---|---|
| staging 目录 | `profile add <staging> --strategy copy` 的 source |
| 校验 | `validate_payload`（`crates/acb/src/profile.rs:1599`） |
| 放进 generations | `copy_payload` 复制到 `generations/<id>/root`（`:1016-1040`） |
| 逐文件清单 | `manifest.json`（`:1025-1032`） |
| 翻指针 | `switch` / `rollback --to`（共用 `switch_to` → `ActivationWriter`） |

所以：**不需要第三种策略**。`store` / `copy` 描述的是“payload 如何链接”，构建来源是另一个维度。
新增“自建安装”只是在 `copy` 之上记录“这一代是谁、用什么构建的”。

补充一点：`copy` 是**复制**而不是移动 staging。构建流水线通常希望 staging 可丢弃，复制是合理的；
如果磁盘敏感，可以后续加 `--move` 选项，但不要改变默认语义。

### 3.2 “每次单独维护”意味着什么

它意味着不存在内容寻址的不可变来源（不像 `/nix/store`），所以：

- **必须保留逐文件 manifest 并在升级前可校验**；今天的 `verify` 是显式命令，建议升级链在
  `activate_generation` 前至少校验目标代的二进制存在（现在已有，见
  `crates/acb/src/profile.rs:609-660`），全量 manifest 校验可选；
- **来源信息必须进 generation 元数据**，否则回滚后无法回答“这一代是什么、能否重建”。

建议在 `Meta` 增加可选字段（保持 `schema: 1`，serde 默认缺失即可，旧数据不受影响）：

```json
"build": {
  "builder": "cargo|cmake|msbuild|...",
  "revision": "<git sha 或构建号>",
  "toolchain": "<rustc/msvc 版本>",
  "command": "<构建命令>",
  "built_at_ms": 1760000000000,
  "staging_sha256": "<staging 目录或归档的摘要>"
}
```

录入方式：`profile add` 增加 `--build-revision` / `--build-toolchain` / `--build-command` 之类
可选参数，或从 staging 里的 `build.json` 读取。ACB 只记录，不解释，也不据此做鉴权。

### 3.3 Windows 上的 payload 校验

`validate_payload` 目前硬编码 systemd unit 与 `.sh`（`crates/acb/src/profile.rs:1599-1621`），
Windows 自建包必然失败。需要按平台分清单，例如 Windows 至少要求：

- `bin/acb-bridge.exe`
- `deploy/run/supervisor.toml.tmpl`
- `harnesses/...`（JS 部分跨平台）
- 不含 `deploy/units/systemd` 与 `.sh`

这是“非 nix 自建安装”在 Windows 上真正缺的一块，属于小改但必须显式设计，不能靠“payload 里碰巧有什么”。

---

## 4. nix 环境下的 gcroot：事实核查

### 4.1 `nix build` 的产物是不是根

是，但要区分“根存在”和“根一直有效”。

在本机（Nix 2.35.2，多用户安装）实测：

```console
$ nix-store --query --roots /nix/store/xldd8...-acb-bridge-0.1.0
/home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/result -> /nix/store/xldd8...-acb-bridge-0.1.0

$ for f in /nix/var/nix/gcroots/auto/*; do t=$(readlink "$f"); case "$t" in *result*) echo "$f -> $t";; esac; done
/nix/var/nix/gcroots/auto/hd86... -> /home/nixos/.../result
```

即：`nix build` 默认创建的 `result` 软链被注册成**间接根**（auto link 指向 `result`，`result`
再指向 store path）。删除 `result`、或下一次 `nix build` 把 `result` 改指到新输出，旧输出就失去
这个根，变成可回收。`nix build --no-link` 不创建链接，也就没有根；`nix build -o PATH` /
`--out-link PATH` 则把根登记到 PATH。

### 4.2 `nix-store --add-root` 才是普通用户可用的“pin API”

实测（普通用户，无需 sudo）：

```console
$ nix-store --add-root ~/acb-probe-root --indirect -r /nix/store/xldd8...-acb-bridge-0.1.0
$ nix-store --query --roots /nix/store/xldd8...-acb-bridge-0.1.0 | grep acb-probe-root
/home/nixos/acb-probe-root -> /nix/store/xldd8...-acb-bridge-0.1.0
$ rm ~/acb-probe-root
$ nix-store --query --roots /nix/store/xldd8...-acb-bridge-0.1.0 | grep -c acb-probe-root
0
```

daemon 会在 `/nix/var/nix/gcroots/auto/` 下写入间接根；根软链被删，根立即失效。这正是
ACB 需要的语义：pin 的生命周期由 ACB 控制，删除 pin 即释放。

### 4.3 `pins.rs` 当前做法的问题

`gcroot_candidates()` 的顺序是：

1. `ACB_GCROOT_DIR`（显式覆盖）
2. `$XDG_STATE_HOME/nix/gcroots`（默认 `~/.local/state/nix/gcroots`）
3. `/nix/var/nix/gcroots/per-user/$USER`
4. `/nix/var/nix/gcroots`

见 `crates/acb/src/pins.rs:51-77`。`pin_with_dirs` 对每个候选 `create_dir_all` 后建软链，
第一个成功就返回 `PinOutcome::Gcroot`（`:85-99`）。

实测反例：在 `~/.local/state/nix/gcroots` 放一个指向已构建输出的软链后：

```console
$ nix-store --gc --print-roots | grep -c acb-probe-root
0
$ nix-store --query --roots /nix/store/xldd8...-acb-bridge-0.1.0 | grep acb-probe-root
（无输出）
```

也就是说，在本机这种多用户 Nix（`state-dir=/nix/var/nix`）下，用户目录里的软链**不是 GC 根**，
但 `pins.rs` 会认为成功并返回 `Gcroot`。`/nix/var/nix/gcroots` 与 `per-user/$USER` 实测不可写
（`test -w` 为 no）。最终结果：

- 前半段“假成功”，实际无保护；
- 本地降级 `<profile>/../state/pins` 也一定不会被 nix GC 扫描；
- 真正保护当前输出的是 `nix build` 留下的 `result` / `-o` 根，而不是 ACB 的 pin。

### 4.4 建议

按侵入性从低到高：

1. **短期（不改机制，只改报告）**：把 `~/.local/state/nix/gcroots` 成功但未被扫描的情况标成
   `Local`/`Unprotected`，不要返回 `Gcroot`。这样至少不会误导。
2. **推荐**：`store` 策略本来就只在 nix 环境成立，pin 改为调用
   `nix-store --add-root <pin> --indirect -r <store-path>`（或直接用 `nix build -o <pin>` 产出）。
   失败则报 `Unprotected` 并给出可操作提示；不要静默降级。
3. **如果坚持运行期完全不调用 nix**：只能把“保根”责任交给部署方：部署必须保留构建输出的
   `result` / out-link，且不能用 `git clean -xdf`、不能把同一个 out-link 指向新版本后丢弃旧代；
   ACB 的 pin 只能作为记录，无法提供保护。这需要写进部署契约，不能靠代码兜底。
4. **`copy` 策略不需要 pin**：代目录是本地真实快照，和 nix GC 无关。

风险边界要写清楚：`result -> store path` 这个根的有效期等于
“软链存在且未被改指”；`nix-collect-garbage -d` 清理的是 profile 旧代，不会删除仍被根引用的路径，
但会把失效的 auto root 清掉。

---

## 5. 落地顺序建议

| 顺序 | 内容 | 是否新机制 |
|---|---|---|
| 1 | `current.pointer` 写入 + `resolve_current`（真值优先） | 否 |
| 2 | `status`/`current`/`resolve_bin_dir`/`resolve_repo_dir` 改为走 `resolve_current` | 否 |
| 3 | `pin` 结果如实报告（`Gcroot`/`Local`/`Unprotected`），store pin 走 `nix-store --add-root` | 小 |
| 4 | `Meta.build` 可选字段 + `profile add` 录入参数 | 否 |
| 5 | Windows payload 校验清单 | 小 |
| 6 | Windows junction 投影（失败降级 pointer） | 是 |
| 7 | Windows 进程树 / Job Object（与指针无关） | 是 |

不建议做的事：

- 用 `nix profile` 的 profile 链接替代 `current.json`（运行期依赖 nix、copy/Windows/无 nix 不可用、
  多写者、LKG 健康语义无处安放；已在上一轮评估中逐条核对）。
- 用 junction 作为唯一指针。
- 为每个 generation 生成 shim 作为第三份入口。

---

## 6. 代码位置索引

| 主题 | 位置 |
|---|---|
| 单写者提交 / journal / 回滚 | `crates/supervisor/src/pointer.rs:228-294` |
| 投影写入（Unix） | `crates/supervisor/src/pointer.rs:390-421` |
| 启动对账修复投影 | `crates/supervisor/src/pointer.rs:308-338` |
| 非 Unix 投影空实现 | `crates/supervisor/src/pointer.rs:416-420` |
| 非 Unix 软链报错 | `crates/acb/src/profile.rs:1776-1779`、`crates/acb/src/pins.rs:125-139` |
| 读 `profile/current` | `crates/acb/src/profile.rs:152-199`、`:1734-1748`、`:208-238` |
| copy 快照 + manifest | `crates/acb/src/profile.rs:952-1058`、`:1065-1121` |
| payload 必填清单 | `crates/acb/src/profile.rs:1599-1625` |
| 目标代校验（二进制） | `crates/acb/src/profile.rs:609-660` |
| gcroot 候选与 pin 结果 | `crates/acb/src/pins.rs:19-108` |
| launcher 只读 `current.json` | `crates/launcher/src/main.rs:82-113` |
| Windows 设计草案 | `docs/plan/acb-upgrade-chain.md:421-432` |
