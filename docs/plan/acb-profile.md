# ACB profile：generation 布局与命令契约

状态：2026-11-07。范围是 profile 入口与现有 `unit` / `run` / `doctor` / supervisor LKG 的衔接；
nix-config 只通过 `profile seed` 做一次 bootstrap，运行期指针归 launcher + supervisor。

## 1. 布局

```text
$XDG_DATA_HOME/acb/profile/          # 默认 ~/.local/share/acb/profile
  generations/
    gen-<unix_ms>-<8hex>/
      meta.json                      # 元数据
      manifest.json                  # 仅 copy 策略：逐文件 path/size/sha256
      root/                          # copy 策略：本地复制快照
      root -> /nix/store/...         # store 策略：软链，靠 store 不可变
  current -> generations/<id>/root   # 相对软链，原子 switch 只改这一条
```

- 一个 generation = 一个产物目录；payload 策略由 `--strategy store|copy` 显式决定，不做自动判断。
- `store`：`root` 是指向 `/nix/store/...` 的软链，回退依赖 store 不可变；要求源路径在 store 下。
- `copy`：`root` 是本地目录，payload 在 `add` 时复制进来并生成 `manifest.json`，
  之后源目录被改动不影响回退；适用于没有 nix 的机器。
- 旧 generation（`meta.json` 无 `payload` 字段）仍是软链，行为完全保留。
- `current/bin`、`current/harnesses`、`current/deploy` 直接可用；切换 `current` 后，
  `unit render` 生成的 `ExecStart` 跟随新 generation。
- `ACB_PROFILE_DIR` 可覆盖 profile 根目录（测试/高级用法），默认按 XDG 规则。

## 2. `meta.json`

```json
{
  "schema": 1,
  "id": "gen-178...-a1b2c3d4",
  "label": "main",
  "created_at_ms": 178...,
  "source": {
    "kind": "store",
    "path": "/nix/store/...",
    "version": null
  },
  "config": {
    "path": "/home/nixos/.local/share/acb/config/config.toml",
    "sha256": "...."
  },
  "payload": {
    "strategy": "copy",
    "snapshot": { "files": 98, "bytes": 35651584 }
  }
}
```

- `schema`：当前固定 1；读取时不支持的值直接报错。`payload` 是可选新增字段，
  旧数据没有它仍可读，旧二进制忽略未知字段。
- `id`：`gen-<unix_ms>-<8hex>`；同一毫秒冲突时追加 `-1`、`-2`。
- `label`：可选，generation 内唯一；`switch` 可用 id 或唯一 label。
- `created_at_ms`：Unix 毫秒；`list` / `gc` 排序依据。
- `source.kind`：路径以 `/nix/store/` 开头为 `store`，否则 `path`；`source.version` 来自
  `--version`。`source` 记录来源，`payload` 记录该代如何落地。
- `payload.strategy`：`store` 或 `copy`；缺失表示旧版软链 generation。
  `payload.snapshot` 仅 copy 有，记录文件数与总字节数。
- `config`：配置文件路径 + 内容 sha256。未提供 `--config` 时回退到现有
  `ACB_UNIT_CONFIG_FILE`；两者都没有时为 `null`。这是 LKG 记录配置身份的数据源。

## 3. 命令

```text
acbctl profile list    [--profile-dir DIR]
acbctl profile current [--profile-dir DIR]
acbctl profile add <path> --strategy store|copy [--label LABEL] [--config PATH] [--version VERSION] [--profile-dir DIR]
acbctl profile switch <gen-or-label> [--profile-dir DIR]
acbctl profile gc      [--keep N] [--profile-dir DIR]
acbctl profile seed [<path>] --strategy store|copy [--state-dir DIR] [--profile-dir DIR]
acbctl profile verify  [<gen-or-label>] [--profile-dir DIR]
acbctl profile migrate --to copy [<gen-or-label>] [--profile-dir DIR]
```

- `list`：退出 0；按 `created_at_ms` 倒序列出，`*` 标记 current。
- `current`：有 current 退出 0，无 current/损坏退出 1；额外打印 payload 策略。
- `add`：校验产物布局，按显式策略创建 generation，写 `meta.json`，不自动 switch。
  `--strategy` 缺失或不支持时直接报错退出（也接受 `ACB_PROFILE_STRATEGY`），不静默降级。
  校验项：`bin/`、`harnesses/`、`deploy/start-dsh.sh`、`deploy/start-drivers.sh`、
  `deploy/opencode-serve.sh`、`deploy/run/supervisor.toml.tmpl`、`deploy/units/systemd/`、
  `harnesses/dsh/src/driver.mjs`、`harnesses/dsh/dsh-mcp.patch.yml`、
  `harnesses/opencode/plugin/index.js`、`harnesses/opencode/plugin/acb-wire.mjs`、
  `harnesses/shared/tools.mjs`。不强制每个 bin 文件名都存在
  （`acb-drivers` / `acb-issue-service` 尚未全部产出）。
  插件必须保持原仓库的相对结构：`harnesses/opencode/plugin/index.js` 通过
  `../../shared/tools.mjs` 引用 `harnesses/shared/tools.mjs`，不能只单独拷插件目录。
- `switch`：只改 `current` 软链；先校验 generation 的 `root` 存在，再在 profile 根目录创建
  临时软链并 `rename` 到 `current`。失败只删临时软链，旧 `current` 保持有效。
- `gc`：默认 `--keep 5`；保留最新 N 个 generation + current + 兄弟目录
  `../state/supervisor/lkg.json` 记录的 generation。其余删除 generation 目录
  （store 策略只删 `root` 软链不动 store；copy 策略删本地快照）。`lkg.json` 存在但解析失败时直接报错，不盲删。
- `seed`：安装器入口。generations 为空时按显式策略 `add` + `switch`，并在首次安装时
  写入一次 `lkg.json`（默认 `<profile>/../state/supervisor/lkg.json`，可用 `--state-dir` 指定）；
  已有 `lkg.json` 绝不覆盖。generations 非空时先校验 `current`：软链缺失、目标 root 不存在、
  或目标缺 `bin/acb-bridge` 都视为损坏。损坏且提供了合法 payload 时重建一代并 switch，
  打印 `seed repaired current -> <id> (was: <原因>)`；损坏但未提供/无法使用 payload 时
  非零退出并提示手工 `profile add --strategy ...` + `profile switch`。`current` 正常时
  行为不变：打印 `seed skipped: profile already has N generation(s)`，退出 0。
- `verify`：store 策略检查软链目标存在（store 不可变，无需逐文件摘要）；copy 策略按
  `manifest.json` 逐文件校验快照，并额外对比 `source.path` 报告源漂移（退出码只由快照
  损坏决定）；旧 generation 明确打印不可校验并退出 0。
- `migrate --to copy`：把已有 legacy/store generation 原地转成 copy 快照（复制当前 `root`
  指向的 payload，保留 id/label/created_at），`current` 与 `lkg.json` 指针一律不动；
  只支持 `copy`，`--to store` 报错并提示用 `profile add --strategy store`。
- 尺寸：copy 策略直接复制，不去重。原因：store 与 home 常在不同文件系统（硬链接 EXDEV），
  path 源硬链接会让快照跟着源变，内容寻址需要额外引用计数；payload 约 35MB/代，
  `--keep 5` 约 175MB，直接复制可接受。

## 4. 与现有契约衔接

- `unit render` 与 `acbctl run` 解析路径时，优先用 `profile/current/`：
  - `profile/current/bin` 提供 bridge/launcher/acbctl 等二进制；
  - `profile/current/deploy/*.sh` 提供启动脚本；
  - `profile/current/harnesses/` 提供 dsh/opencode 插件与共享代码，目录结构必须与原仓库一致。
- `ACB_UNIT_REPO_DIR` 从必填改为可选；profile 不存在时才回退到它（开发覆盖/回退路径）。
  `ACB_UNIT_BIN_DIR` 同理。
- `unit render` 生成的 `WorkingDirectory`、`ExecStart` 脚本路径全部指向
  `<profile>/current/...`；切换 generation 后下次启动自然跟随。
- `acbctl run` 也使用同一解析结果，并把 `ACB_UNIT_REPO_DIR=<profile>/current` 传给子进程；
  launcher 模式渲染 `supervisor.toml` 时，profile 活跃则追加：

```toml
[supervisor.generation]
id = "gen-..."
path = "/home/nixos/.local/share/acb/profile/generations/gen-.../root"
config_sha256 = "..."
```

- `path` 是新格式 generation 的 payload 路径（store 策略下该 `root` 仍是 store 软链；
  legacy 保留原 `source.path`，避免 LKG 身份抖动）。launcher/supervisor 只消费这个路径，
  不感知策略。
- 渲染 `supervisor.toml` 时 `program` 钉在 payload 的 `bin/acb-bridge`，LKG 提升/回退
  才会真正回到那一代的字节，而不是跟随 `profile/current`。
- supervisor 的 `CommandSpec` 增加可选 `generation` 字段，`current.json` / `lkg.json`
  原样携带 id、path、配置摘要。旧 JSON 无该字段仍可读。
- `acbctl unit doctor`：profile 缺失打印
  `warn: profile not used; using fallback ACB_UNIT_REPO_DIR/ACB_UNIT_BIN_DIR paths`，
  不改变退出码；`current` 存在但目标损坏算 drift，退出 1。doctor 仍不读取 env 文件内容。

## 5. 边界

- 不做自动 rollback 命令；LKG 记录 generation + 配置摘要，回滚决策仍由 launcher/supervisor
  与后续工具使用。安装器只在首次安装时写一次 LKG，之后运行期提升/回退完全归 launcher/supervisor。
- `gc` 不保护带 label 的 generation；需要保留时提高 `--keep` 或不要 gc。
- `profile switch` 不启停服务，也不自动同步 `current.json`；launcher 下一次重拉时按自己的
  `current.json` 决定版本。
- copy 快照损坏/不完整时：`verify` 退出 1 并列出 changed/missing/extra；恢复方式是重新
  `profile add` 生成新代（或 `migrate` 重建）并 switch，不会自动修复或删除现有快照。
- 产物目录内容是否完整由 `add` 的固定校验项覆盖；不校验每个二进制版本号或 ABI。

