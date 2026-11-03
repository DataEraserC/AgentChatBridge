# native 重载的保底：排空、退出与外部 supervisor

状态：设计提案（2026-11-04）。**本轮只出设计，不改代码、不改 systemd 单元、不重启服务。**
等待用户拍板后再落地。

## 0. 结论先行

- **core 能做**：识别「该重启了」、进入排空状态、停止开新轮、等在途轮次/任务收尾、刷库、
  按约定退出码退出、暴露健康/排空探针。
- **core 不能做**：可靠地把自己重新拉起来。进程内 `exec` 自我重启在「进程已不可信」的
  场景下不成立；真正的拉起必须由外部 supervisor（这里就是 systemd 用户服务）完成。
- **推荐形态**：core 负责「排空 + 退出码 + 探针」，systemd 负责「拉起 + 退避 + 启动上限」，
  再由一个很薄的启动包装器负责「选择 store path + 健康门禁 + 版本/插件回滚」。
- **不建议**：在 core 里内置第二进程、进程内 `fork/exec` 自重启、或把回滚逻辑塞进
  插件卸载路径。它们都会在真正需要保底时依赖已经不可信的进程状态。

## 1. 边界判断：core 能做什么、不能做什么

### 1.1 具体失败场景（不是「语言/FFI 差异」）

1. **FFI 插件把堆写坏**：native `.so` 越界写、double free、ABI 不匹配导致 allocator
   metadata 损坏。`dlclose` 之后代码段没了，但损坏的堆还在。此后任何一次 `Vec` 扩容、
   `serde` 解析、甚至 `eprintln!` 都可能崩在无辜位置。
2. **插件留下不可回收的线程/锁**：插件起线程并持有锁，或注册了 TLS destructor；
   `dlclose` 无法 join 线程、无法撤销已注册的全局状态。进程内重启后，新实例可能看到
   旧线程、旧信号处理器、旧 `atexit` 项。
3. **信号/文件描述符状态被污染**：插件改过 signal handler、关闭了不该关的 fd、
   占着 socket/端口。进程内 `exec` 保留 PID、fd 表与部分进程属性；端口仍被占，
   重启等于原地失败。
4. **self-exec 本身走不过去**：多线程进程里 `fork()` 后的子进程只应调用
   async-signal-safe 函数；Rust 的 `std::process::Command` 会分配、会碰锁。
   当进程已经不安全时，「执行重启代码」正是最不可靠的一步。
5. **supervisor 看不到重启**：进程内 `exec` 后 MainPID 不变，systemd 的启动计数、
   失败计数、cgroup 内存峰值、`RestartSec` 退避都不会重置；崩溃循环会被藏起来。
6. **启动期问题无法自愈**：如果坏的是配置/插件清单，self-exec 会变成无限循环；
   只有外部能看到「连续失败」并决定回滚。

### 1.2 core 的职责边界

| 能做 | 不能做 |
|---|---|
| 标记 `Draining`，拒绝新轮次、继续入库 | 保证损坏的 allocator/线程/信号状态可恢复 |
| 等在途轮次、route worker 任务收尾 | 可靠地 reinvoke 自身（尤其进程已不可信时） |
| 刷 SQLite WAL、逆序 shutdown managers | 看到自己的 crash-loop 次数并决定回滚 |
| 暴露 `/healthz` `/readyz` 与排空状态 | 重置 cgroup、启动计数、环境变量、fd 表 |
| 按约定退出码退出（0 / 75 / 78） | 在没有外部 supervisor 时保证「退出后一定再起来」 |

**判断规则**：凡是需要「重新获得一个干净进程」的动作，都不属于 core；core 只负责
把自己安全地交出去。

## 2. supervisor 方案（systemd 用户服务）

### 2.1 现有单元的事实

- `deploy/systemd/acb-bridge.service`：`Type=simple`、`Restart=always`、`RestartSec=5`，
  `ExecStart` 指向 store path（当前 worktree debug 用 drop-in 覆盖）。
- 没有 `StartLimitIntervalSec` / `StartLimitBurst` 显式配置，走 systemd 默认
  （10 秒内 5 次启动），坏配置会很快进入 failed。
- core 当前没有 SIGTERM/SIGHUP 处理；`shutdown` 原子量没有被主进程外部触发路径设置。
  也就是说**排空目前不存在**，systemd 发 SIGTERM 会直接杀进程。

### 2.2 需要的 core 侧信号

1. **退出码约定**
   - `0`：正常停止（`systemctl stop`、管理员显式停机）。
   - `75`（`EX_TEMPFAIL`）：core 请求重启（排空完成，或 native 卸载后被判定为不可信）。
   - `78`（`EX_CONFIG`）：配置/启动校验失败，**不应自动重启**，等人修配置。
   - 其它非零：运行时错误，按 `on-failure` 重启。
2. **健康探针**
   - `GET /healthz`（仅 loopback）：`200` 表示进程活着，body 至少包含
     `state`（`starting` / `ready` / `draining` / `degraded`）、`active_turns`、
     `queued_turns`、`running_tasks`、`plugin_errors`、`schema_version`、`build`。
   - `GET /readyz`：只有 `state == ready` 才 `200`；`starting/draining/degraded` 返回 `503`。
   - 探针必须能在不拿控制台 token 的情况下被本机脚本调用；绑定 `127.0.0.1`，
     不复用对外监听地址。
3. **排空状态**
   - `Draining` 期间：**继续入库**（不丢消息）、**拒绝开新轮**、不应用 pending 插件动作、
     不再接受新 harness 连接（已有连接可收尾）。
   - `drain_timeout_ms` 到点仍有在途轮次：记录未完成会话/任务，仍退出 `75`；
     未读由 SQLite 水位恢复，内存任务记录会丢（明确写在日志里）。
4. **触发方式**
   - `SIGTERM`：进入排空，成功后退出 `0`（operator stop）。
   - `SIGHUP`：进入排空，成功后退出 `75`（reload/restart）。
   - 管理员控制台 `POST /api/restart`：同 SIGHUP。
   - native 卸载/重载后若 loader 自报「不可信」，core 可主动请求退出 `75`（最终由
     loader/manifest 能力决定，不是默认行为）。

### 2.3 需要的 systemd 侧改动

```ini
[Unit]
StartLimitIntervalSec=60
StartLimitBurst=3

[Service]
Type=simple
Restart=on-failure
RestartForceExitStatus=75
RestartPreventExitStatus=78
RestartSec=5
TimeoutStopSec=45
KillMode=mixed
ExecStart=/path/to/acb-bridge-launch --config /path/to/config.toml
ExecReload=/bin/kill -HUP $MAINPID
ExecStartPost=/bin/sh -c 'sleep 5; curl -fsS --max-time 5 -H "Authorization: Bearer $(cat ${XDG_RUNTIME_DIR}/acb-health.token)" http://127.0.0.1:3100/healthz >/dev/null'
```

- `ExecStart` 不再直接写死 store path，而是指向**启动包装器**；包装器读
  `state/deploy/current`（符号链接或文本文件），`exec` 对应 store 的二进制。
- `ExecStartPost` 只做本机 curl 健康探针（token 由 core 写到运行时目录，0600）；
  **MVP 不做自动回滚**：探针失败让本轮启动失败，连续失败撞 StartLimit 后停在 failed，
  由人切换回上一个可用 store path。完整版再让门禁自动改 `current`。
- `TimeoutStopSec` 必须大于 core 的 `drain_timeout_ms`，否则 systemd 会在排空完成前
  `SIGKILL`。
- `KillMode=mixed`：SIGTERM 先给主进程，超时后再杀 cgroup；避免插件子进程先死、
  主进程还在等。

### 2.4 配置项命名（core 侧）

建议新增 `[runtime.drain]`（或 `[supervisor]`，二选一，落地前定）：

| 配置 | 默认 | 含义 |
|---|---|---|
| `drain_timeout_ms` | `30000` | 等轮次/任务收尾的上限（MVP 先写死为 30s，配置化留后） |
| `drain_poll_ms` | `250` | 排空状态轮询间隔（MVP 常量） |
| `health_bind` | `127.0.0.1:3101` | 健康探针监听地址；MVP 直接复用控制台端口 + loopback 限制 |
| `ready_grace_ms` | `10000` | 启动后多久才算 ready（空日后插件加载用） |
| `restart_on_untrusted_unload` | `false` | native 卸载后是否主动请求重启 |
| `last_good_root` | `state/deploy` | 版本 last-good 指针所在目录（MVP 由人维护） |
| `plugin_lkg_root` | `state/plugins` | 插件 last-known-good 快照所在目录（MVP 只记录） |

退出码本身是合同，不进配置。MVP 实现说明：drain 预算/轮询是常量；`/healthz` 挂在
控制台 HTTP 上，但强制 loopback；token 复用控制台 token，core 另写一份 0600 的运行时
token 文件供 systemd 侧 curl。

### 2.5 需要改 core 的具体位置

- `crates/bridge/src/main.rs`：装 SIGTERM/SIGHUP handler；把 `shutdown` 升级为
  `RuntimeState`（`Running` / `Draining` / `Stopping`）；主循环等待状态而不是
  `sleep(3600)`；退出码映射。
- `crates/kernel/src/engine.rs`：`poll_turn` / `open_turn` 在 `Draining` 时拒绝新轮；
  `ingest` 仍写流水；`apply_pending_plugin_actions` 在 `Draining` 时跳过；
  暴露 `drain_snapshot()`（active/pending/tasks）。
- `crates/kernel/src/endpoint.rs`：`serve` 在 `Draining` 时停止接受新连接。
- `crates/bridge/src/console.rs`：`/healthz` `/readyz` / `POST /api/restart`（控制台
  token 保护；健康探针不需要 token）。
- `crates/bridge/src/main.rs` / `acb-adapter-dynamic`：退出前按逆序
  `shutdown_managers()`，SQLite checkpoint。

## 3. 回滚

### 3.1 版本回滚（store path 级）

**要持久化的东西**

- `state/deploy/current`：指向当前应运行的 store path（符号链接或单行文本）。
- `state/deploy/last-good`：最近一次通过健康门禁的 store path + build 标识。
- `state/deploy/attempt`：本轮启动时间、目标 path、结果（`pending` / `healthy` / `failed`）。

**谁写（MVP）**

- 部署脚本/人写 `current` 与 `last-good`；**人是回滚的执行者**。
- `ExecStartPost` 只做健康检查：失败让本轮启动失败，连续失败撞 StartLimit 后停在
  failed，不自动改 `current`。
- core 只读自己的 build 信息，不写部署指针。

**流程（MVP）**

```text
部署新 store path → current 指向新版本 → systemd 启动
  → 5s 后 curl /healthz：通过 → 正常运行
  → 失败：本轮启动失败；连续失败后 unit failed
  → 人把 current/last-good 切回上一个 store path，再 systemd restart
```

自动回滚放在完整版：健康门禁在稳定窗口通过后写 `last-good`，失败时改 `current`
并退出非零；前提是健康判定足够可信。

### 3.2 插件回滚（last-known-good 清单）

**要持久化的东西**

- `state/plugins/last-good.json`：模块 id、相对路径、manifest 哈希、内容哈希、
  校验时间、动态 namespace、声明式/扩展点/能力快照。
- 插件源码/构建产物：仍放扩展目录，LKG 只记「哪些文件内容哈希是好的」，
  不复制整棵目录（避免双份真源）。
- 可选：`state/plugins/current.json`：本次启动实际加载成功的快照。

**谁写**

- core 在动态 namespace 完成一次「全部模块校验通过 + 快照替换成功」后，写
  `current.json`。
- 外部健康门禁在稳定窗口通过后，把 `current.json` 提升为 `last-good.json`。
  这样「加载成功但启动后立刻不可用」的插件不会被误标为好。

**回滚流程**

1. 启动时 core 先按 `ACB_DYNAMIC_DIR` 加载；任一模块硬失败（manifest 不可读、
   校验异常、ABI 不匹配）时，如果 `plugin_lkg_root` 存在且启用回滚，则加载
   LKG 清单并标记 `degraded`，`/healthz` 明确列出被跳过的模块。
2. 外部健康门禁看到 `degraded` 且 `rollback_plugins=true`：把扩展目录切到 LKG
   （或写一个 `ACB_DYNAMIC_DIR` 指向 LKG 的 drop-in），再重启一次。
3. 若 LKG 也失败：不再自动回滚，退出 `78`，等人处理。

**边界**：插件回滚不负责数据回滚。插件写过的外部文件/任务产物不在 LKG 范围内；
需要的话由插件自己按 manifest 声明快照策略。

## 4. 与 ADR-0029 / 0030 的关系

- **轮次边界**（ADR-0029 D4）：排空不改变「轮次边界替换快照」的语义；`Draining`
  期间不应用 pending unload/reload，正在跑的轮次继续用它的旧快照。
- **新不变量**：`Draining` 期间**拒绝新轮次但绝不丢队列**。入站记录照常写 SQLite，
  未读由水位/水位之上的记录推导；内存 `pending` 丢失后，下一进程仍能凭未读重新唤醒。
  需要在 `Bridge` 层加一条断言/测试：drain 后 `unread` 不减少。
- **任务**（ADR-0029 D5）：drain 等 route worker 在途任务结束；任务记录是内存态，
  重启后丢失，这一点必须在日志和文档里说清，不能假装持久化。
- **native 卸载**（ADR-0030 D3）：ADR 写的「回滚到上一份已加载实现」在同路径 reload
  下仍不成立（见 `crates/adapters/dynamic/src/lib.rs` 注释）。supervisor 重启是
  这个缺口的**现实保底**，不改变 ADR 原文，也不假装 core 能回滚 native 实现。
- 需要新增的不变量建议写进 ADR-0029 的后续修订：
  1. 进程重启是 native 生命周期的唯一可信边界；
  2. drain 状态必须可观察，且 drain 不丢队列；
  3. 版本/插件回滚由外部 supervisor 负责，core 只提供证据。

## 5. 代价与取舍

按四项统一口径评估（开发难度不计入）：

| 口径 | MVP | 完整版 |
|---|---|---|
| 稳态运行成本 | 不增加常驻进程；每次重启 = 启动 + 重放未读的一轮成本 | 额外常驻 10–30 MB；多一层代理/健康窗口开销 |
| 可观测性 / 可审计性 | 退出码 + `/healthz` + journal + 回滚脚本退出码，足以对账 | supervisor 自带状态机/灰度指标/回滚审计，更完整 |
| 接口面增长速度 | core 只加信号、排空状态、探针、退出码；不按功能加入口 | 新增 supervisor 配置/协议，接口面变宽，需要版本化 |
| 可变更性 | 退出码与探针 schema 一旦被外部脚本依赖就难改，先定窄接口 | supervisor API 一旦被部署脚本依赖更难改，宜先内部 |

形态：MVP = systemd `Restart=on-failure` + core drain + `/healthz` + 启动包装器；
完整版 = 独立 `acb-supervisor` + 健康窗口 + 多实例/灰度。内存数字来自常驻进程估算，
不是承诺。

**MVP 明确不做**：进程内自重启、第二常驻进程、跨版本数据迁移回滚、插件产物回滚。

## 6. 落地顺序（建议，未执行）

1. core：`RuntimeState` + SIGTERM/SIGHUP + `/healthz` `/readyz` + 退出码（不改 systemd）。
2. systemd：`Restart=on-failure` / `RestartForceExitStatus=75` / `RestartPreventExitStatus=78` /
   `TimeoutStopSec` / StartLimit。
3. 启动包装器 + `current`/`last-good` + 本机健康探针（MVP 不做自动回滚）。
4. 插件 LKG：先只记录清单（`current.json` / `last-good.json`），自动回退留到有真实需求。
5. 完整版 supervisor 只在出现多实例/灰度需求后再做。

## 7. 已代行决策（2026-11-04）与剩余待拍板

已按用户授权确定：

1. 进程重启是 native 生命周期的唯一可信边界；core 只排空后退出，拉起交 systemd。
2. drain 默认 30s，超时强制退出并记录剩余内存任务清单；任务记录会丢，消息不丢。
3. `/healthz` 只允许 loopback，且要求 token；token 复用控制台 token，core 写运行时
   0600 文件供 systemd curl，单元文件不出现 token 明文。
4. 回滚 MVP 为手动：保留上一个可用 store path（Nix generation）+ 插件 LKG 清单；
   自动回滚留到完整版。

仍需用户确认：ADR-0030 D3 的措辞是否按「supervisor 重启保底」修订（本轮不改 ADR）。
