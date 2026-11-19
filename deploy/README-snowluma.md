# SnowLuma 部署（ACB 的 QQ 协议端依赖）

SnowLuma 是外部组件，安装与托管仍属用户职责（见 `docs/guide/deployment-matrix.md` §五）。
本文只说明**本机这套已经怎么接好**，以及改动时哪些东西不能碰。

## 文件

| 文件 | 入库 | 作用 |
| --- | --- | --- |
| `snowluma-compose.single.yml` | 是 | 单账号形态 |
| `snowluma-compose.multi.yml` | 是 | 多账号形态（开关默认空，行为等同单账号） |
| `snowluma.env.example` | 是 | env 模板，只有占位符 |
| `snowluma.env` | **否** | env 真值，含 `VNC_PASSWD`，权限 600 |
| `.env` | **否** | 软链接 → `snowluma.env`，见下方「为什么需要它」 |
| `snowluma.sh` | 是 | 手动操作唯一入口 |
| `verify-snowluma-compose.sh` | 是 | 守卫 |
| `snowluma-compose.yml` | 是 | 已废弃，仅历史参照 |

`~/Documents/SnowLuma/` 下的 `docker-compose.single.yml` / `docker-compose.multi.yml` / `.env`
都是指向本目录的软链接，两处同一份内容，不会漂移。

## 命令

```bash
deploy/snowluma.sh single up      # 单账号
deploy/snowluma.sh multi  up      # 多账号形态
deploy/snowluma.sh multi  down
deploy/snowluma.sh multi  status
deploy/verify-snowluma-compose.sh # 守卫
```

旧命令 `podman-compose -f ~/Documents/SnowLuma/docker-compose.yml …` 已废弃。

首次部署：

```bash
cp deploy/snowluma.env.example deploy/snowluma.env
chmod 600 deploy/snowluma.env     # 含远程桌面密码
# 填好端口与 VNC_PASSWD 后再 up
```

## 不能碰的四件事

现网实测（`ss -ltnp`）容器监听 **53000 / 53001 / 56081 / 55099**，值来自 `snowluma.env`；
compose 里的 `3000/3001/6081/5099` 只是容器内端口。

1. **宿主端口 53000(HTTP) / 53001(WS)** —— ACB 按这两个访问，改了桥连不上。
   `config.active.toml` 抬头亦写明「53001 = WS，53000 = HTTP」。
2. **三个具名卷** `qq-gateway-data` / `qq-client-config` / `qq-client-data` ——
   这就是登录态，改名等于全部重新扫码。注意顶层 `volumes.<key>.name` 才是实际挂哪个卷，
   只改它、不改 service 引用名，同样会挂到空卷（守卫已覆盖这种情形）。
3. **容器名 `snowluma`** —— `podman exec snowluma` 与运维习惯依赖它。
4. **两份形态不要同时 up** —— 同名容器 + 同宿主端口会冲突。切换＝先 down 再 up，登录态不丢。

## 为什么需要 `deploy/.env` 这个软链接

有两条启动路径，参数形状不同：

- **手动**：`snowluma.sh` 显式传 `--env-file`。
- **自启**：ACB supervisor 的 oneshot 组件由管理内核拼命令，实测
  `crates/supervisor/src/component_runtime.rs:1356-1372` 只拼
  `compose_bin[0] -f <file> up -d`，**不带 `--env-file`**；
  preset 兜底脚本 `presets/snowluma/snowluma_container.sh` 同样只取 `COMPOSE_BIN_0`。

而 podman-compose 默认只读「compose 同目录的 `.env`」这个**固定文件名**。
真值叫 `snowluma.env`，所以若没有 `deploy/.env`，自启会读不到端口、
静默退回 3000/3001，切断 ACB 且不报错。

`deploy/.env → snowluma.env` 让两条路径都能读到真值。已实测：不带 `--env-file`
直接 `podman-compose -f deploy/snowluma-compose.multi.yml config` 解析出 53000/53001。

## 自启接线（本机现状）

活配置 `~/.local/share/acb/state/supervisor/supervisor.toml`：

```toml
[[component]]
name = "snowluma"
preset = "snowluma_container"
type = "oneshot"
enabled = true
file = ".../deploy/snowluma-compose.multi.yml"
compose_bin = ["/etc/profiles/per-user/nixos/bin/podman-compose"]
restart = { policy = "never" }

[component.readiness]
kind = "tcp"
port = 53001
startup_grace_secs = 120
```

该文件首行是 `# acb-managed: acb maintains [supervisor]; [[component]] declarations are preserved.`
—— `[supervisor]` 由 ACB 重写，`[[component]]` 段原样保留
（实现：`crates/acb/src/supervisor_owner.rs:463` `component_suffix`，
测试 `component_suffix_is_preserved_verbatim`）。所以手改组件段不会被覆盖。

`stop_action` 未写＝默认 `leave_running`：停 acbl 不会带走容器，
`deploy/verify-shutdown.sh` 也据此把它的端口/进程记为 INFO 而非残留。

## 加一个 QQ 号

1. `snowluma.env` 里写 `SNOWLUMA_EXTRA_QQ_HOMES=/app/qq-acct2`（两个号用逗号分隔）。
   路径必须在 `/app/` 下、字符集限 `A-Za-z0-9_@%+=:,./-`，且各自独占卷
   （compose 已备好 `qq-acct2` / `qq-acct3`）。
2. `deploy/snowluma.sh multi down && deploy/snowluma.sh multi up`
3. **验证额外实例真的起来了**（唯一硬证据）：
   ```bash
   podman exec snowluma cat /etc/supervisor/conf.d/extra-qq.conf
   podman exec snowluma supervisorctl status   # 应有 qq / qq-extra-1 / snowluma
   ```
   若 `extra-qq.conf` 不存在，说明该镜像 tag 的入口脚本不支持此变量，需换版本；
   别只看容器 Running 就当成了。
4. noVNC（`http://<host>:56081/`）里逐个 QQ 窗口扫码，勾「自动登录」。
   额外实例有 10s/20s 错峰延迟，窗口不同时出现。
5. **WebUI 给新号分配端口**（不做必失败）：`http://<host>:55099/` 选中新账号 →
   OneBot 协议端点 → HTTP 改 3020、WS 改 3021（第三个号 3030/3031）。
   不改则沿用 3000/3001 撞主账号，日志报
   `账号启动失败 … conflicts with … on server binding`，且属终态失败**不自动重试**。
6. ACB 侧加连接：**走控制台**，schema 15 起连接在库里（`config_objects`），改磁盘文件无效。
   `kind=onebot11, transport=ws-client, url=ws://127.0.0.1:53021, account=<新号>`，
   token 与该账号 accessToken 一致。写法参考 `config.example.toml` 的 `qq` / `qq2`。

## 守卫查什么

`deploy/verify-snowluma-compose.sh`（退出 0 通过 / 1 违反 / 2 环境缺依赖）：

- 四个文件存在、`snowluma.sh` 可执行
- env 真值未被 git 跟踪、已被忽略、权限 600/400；模板存在且密码是占位符
- `snowluma.sh` 固化 `--env-file`，env 缺失时 fail-closed
- 两份 compose 解析后：53000/53001 在、三卷名（含顶层 `name`）、容器名、
  多账号 HOME 三约束
- supervisor 的 `file` 指向仓库内权威 compose、`readiness.port` 与 53001 一致、
  `deploy/.env` 存在且指向真值并被忽略

均经负向验证：改坏副本 / `git add -f` 真值 / `chmod 644` / `file` 指回废弃路径 /
readiness 端口错配，都能报红并退出 1。

## 未验证

本机 podman 报 `invalid internal status … pause process`（需 `podman system migrate`
或重启恢复），因此**未做实机 down/up 与自启实测**。以上端口结论来自
`podman-compose config` 解析与 `ss -ltnp` 观测，不是容器启动后的端到端验证。