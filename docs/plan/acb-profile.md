# ACB profile：generation 布局与命令契约

状态：2026-11-06。范围是 profile 入口与现有 `unit` / `run` / `doctor` / supervisor LKG 的衔接；
不做完整 nix profile 集成，nix-config 只通过 `profile seed` 做一次 bootstrap。

## 1. 布局

```text
$XDG_DATA_HOME/acb/profile/          # 默认 ~/.local/share/acb/profile
  generations/
    gen-<unix_ms>-<8hex>/
      meta.json                      # 元数据
      root -> /nix/store/...         # 绝对软链，指向产物根目录
  current -> generations/<id>/root   # 相对软链，原子 switch 只改这一条
```

- 一个 generation = 一个产物目录；`root` 可以指向 nix store path，也可以指向本地构建产物目录。
- 不复制产物本体；`root` 是软链。本地路径必须由使用者保证在 generation 生命周期内稳定。
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
  }
}
```

- `schema`：当前固定 1；读取时不支持的值直接报错。
- `id`：`gen-<unix_ms>-<8hex>`；同一毫秒冲突时追加 `-1`、`-2`。
- `label`：可选，generation 内唯一；`switch` 可用 id 或唯一 label。
- `created_at_ms`：Unix 毫秒；`list` / `gc` 排序依据。
- `source.kind`：路径以 `/nix/store/` 开头为 `store`，否则 `path`；`source.version` 来自
  `--version`。
- `config`：配置文件路径 + 内容 sha256。未提供 `--config` 时回退到现有
  `ACB_UNIT_CONFIG_FILE`；两者都没有时为 `null`。这是 LKG 记录配置身份的数据源。

## 3. 命令

```text
acbctl profile list    [--profile-dir DIR]
acbctl profile current [--profile-dir DIR]
acbctl profile add <path> [--label LABEL] [--config PATH] [--version VERSION] [--profile-dir DIR]
acbctl profile switch <gen-or-label> [--profile-dir DIR]
acbctl profile gc      [--keep N] [--profile-dir DIR]
acbctl profile seed <path> [--profile-dir DIR]
```

- `list`：退出 0；按 `created_at_ms` 倒序列出，`*` 标记 current。
- `current`：有 current 退出 0，无 current/损坏退出 1。
- `add`：校验产物布局，创建 generation，写 `meta.json`，不自动 switch。
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
  `../state/supervisor/lkg.json` 记录的 generation。其余删除 generation 目录（只删
  `root` 软链，不动产物）。`lkg.json` 存在但解析失败时直接报错，不盲删。
- `seed`：仅在 generations 为空时 `add` + `switch`；已有 generation 时直接跳过，
  重复调用无副作用。nix-config 只需调用一次。

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
path = "/nix/store/..."
config_sha256 = "..."
```

- supervisor 的 `CommandSpec` 增加可选 `generation` 字段，`current.json` / `lkg.json`
  原样携带 id、path、配置摘要。旧 JSON 无该字段仍可读。
- `acbctl unit doctor`：profile 缺失打印
  `warn: profile not used; using fallback ACB_UNIT_REPO_DIR/ACB_UNIT_BIN_DIR paths`，
  不改变退出码；`current` 存在但目标损坏算 drift，退出 1。doctor 仍不读取 env 文件内容。

## 5. 边界

- 不做自动 rollback 命令；LKG 记录 generation + 配置摘要，回滚决策仍由 launcher/supervisor
  与后续工具使用。
- `gc` 不保护带 label 的 generation；需要保留时提高 `--keep` 或不要 gc。
- `profile switch` 不启停服务，也不自动同步 `current.json`；launcher 下一次重拉时按自己的
  `current.json` 决定版本。
- 产物目录内容是否完整由 `add` 的固定校验项覆盖；不校验每个二进制版本号或 ABI。
