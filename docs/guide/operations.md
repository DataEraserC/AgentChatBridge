# 启停与体检

这一页只讲一件事：**这台机器上，怎么把服务起对、怎么判断它真的活着。**

## 先认清你用的是哪种部署形态

两种形态的「总开关」不一样，搞混了会一直在重启错的东西：

| | 形态 A：launcher 托管（推荐，本机当前） | 形态 B：四单元分段（`deploy/acb-up.sh` 装的） |
|---|---|---|
| 单元 | 只有 `acb-bridge.service`（描述是 *ACB launcher*） | `dsh-acb` / `acb-bridge` / `acb-drivers` / `acb-opencode-serve` |
| 谁拉起 dsh | launcher 自己，按 dsh → 桥 → 驱动 → opencode 的顺序 | 各自的单元 |
| 换代 | `acbctl profile switch` + 重启 launcher | 换 unit 里的二进制路径 |
| 查形态 | `systemctl --user cat acb-bridge \| head -3` 看见 `acbl` / `acb-launcher` 就是 A | 四个单元都在就是 B |

本机是 **形态 A**：`ps` 里能看到 dsh 和四个驱动的父进程都是 launcher 派出来的 shell，
所以**停一次 `acb-bridge` 就等于停全链路**，不存在「单独重启 dsh」。

## 起、停、重启

```sh
export XDG_RUNTIME_DIR=/run/user/$(id -u)   # SSH / 非桌面登录必须

systemctl --user restart acb-bridge         # 全链路重拉（dsh→桥→驱动→opencode）
systemctl --user stop    acb-bridge         # 全停（桥会先排空，约 30s）
systemctl --user start   acb-bridge         # 再起
systemctl --user status  acb-bridge         # 单元状态
```

开机自启（形态 A）：

```sh
systemctl --user enable acb-bridge
loginctl enable-linger $USER   # 不登录桌面也要跑时必须开
```

::: warning 别用 nohup / 别挂在 AI 会话里
常驻进程挂在终端或 AI 会话的 shell 里，会话一撤，进程还活着但**开不出新 socket**
（`ENOSYS` / `os error 38`），表现是「进程都在、群里没人理」。必须走 systemd。
:::

## 一条命令体检

```sh
deploy/acb-status.sh          # 人看
deploy/acb-status.sh --quiet  # 只要错（给定时任务/脚本用），有问题退出码 1
```

它只看四样东西，因为这四样能证伪「进程在但没活着」：

1. **单元**是 active（形态 A 只查 `acb-bridge`，形态 B 四个都查）；
2. **端口**在监听：7700 桥 / 3100 控制台 / 3080 dsh / 3131 中继（4097 opencode 缺了只提醒）；
3. **ESTAB 连接**：7700 上至少 1 条（驱动接入）、驱动到 3080 至少 1 条；
4. **接口**回话：控制台 `/healthz` 带令牌返回 200（**401 是常态，不带令牌当然 401**）。

最后它会各抓 3 条近期错误，省得再去翻日志。

## 看日志

| 组件 | 位置 |
|---|---|
| 桥 + launcher | `~/.local/state/acb/bridge.log`（形态 A 由 launcher 重定向；形态 B 在 `state/logs/bridge.log`） |
| 驱动 | `~/.local/share/acb/state/logs/drivers.log` |
| dsh | `~/.local/share/acb/state/logs/dsh.log` |
| opencode | `~/.local/share/acb/state/logs/opencode.log`、`opencode-plugin.log` |
| systemd 捕获 | `journalctl --user -u acb-bridge -n 100` |

::: tip 桥的日志为什么不在 journal 里
桥和 launcher 的 stdout/stderr 由 launcher 重定向到文件，`journalctl _PID=` 查不到。
这不是丢日志，是去向不同。
:::

## 换代（不重装）

```sh
cd <仓库>
nix build .#payload --print-out-paths --no-link          # 出 payload 目录
acbctl profile add <payload> --strategy store --label <备注> \
  --config ~/.local/share/acb/config/config.active.toml  # 必须带 --config
systemctl --user stop acb-bridge
acbctl profile switch <gen-id>
systemctl --user start acb-bridge
```

- `--config` 必须带：不带的话 `current.json` 里 args 为空，换代后桥会去找 cwd 的 `config.toml`。
- 换代前先备份库三件套（`acb.db` / `-wal` / `-shm`）。
- 新代起来后 LKG 稳定 60 秒自动 promote；回滚：`acbctl profile rollback` + 重启。
- **payload 完整性比回滚更重要**：新桥启动会把库迁到新 schema，随后若被判失败回滚，
  旧代只认旧 schema → 起不来。所以 `profile add` 的 `validate_payload` 不能绕过。

## 换了版本后「群里不回话」先看这三行

按顺序看日志，别猜：

```sh
grep -aE "content-blocked|dsh\.unreachable|ETIMEDOUT" ~/.local/state/acb/bridge.log | tail -5
```

- `400 content-blocked` → **模型侧内容审查拒了**，不是服务挂了。轮次会失败，
  而「本轮出错」的通知每房间 10 分钟最多 3 条，超了不再逐条发——但从 2026-11-09 起，
  被抑制期间每 `[notifications.in_band].remind_secs`（默认 600 秒）会补**一条**
  「[还在出错] 最近 N 分钟失败 M 次」。一条提醒都没有，说明不是这条路径。
- `dsh.unreachable / ETIMEDOUT`（`selectModel`）→ dsh 刚重启的冷启动窗口，
  30 秒～2.5 分钟内 `agentAvailable=false`，等一会儿或再发一次。
- 都没有 → 用 `deploy/acb-status.sh` 看连接和端口。

日志行**不带时间戳**，所以上面这条 grep 会把重启前的旧错也捞出来。判断「新的还是旧的」只有一个
锚点：`bridge.log` 里最近一条 `acb-config: generation=…`（这次启动的横幅），`drivers.log` 里最近一条
`起驱动：会话 …`。`acb-status.sh` 的「本次启动以来的错误」段就是按这个锚点切的，直接信它；
手工排查时先 `grep -an "acb-config: generation=" ~/.local/state/acb/bridge.log | tail -1` 拿到行号，
再从那一行往后看。

控制台「依赖健康」表怎么读（3100 → 概览）：

- **最近失败的轮次**（依赖表下面那块）是「到底有没有在失败」的权威计数，**含因去重/限流
  没发出通知的轮次**。有计数 = 每轮都在失败、只是通知被压住了；计数为 0 = 真的没轮到它。

- **harness 行**说的是「桥↔harness 的通道通不通」，看 `通道 N 条`。状态为「正常」+
  最后一句话是 `无法连接 ... selectModel ETIMEDOUT` 时，dsh 本身是活的，问题在模型侧。
- **model 行**说的才是「上游模型能不能用」。`内容被审核拦截` / `被限流` /
  `模型接口超时` 都只落在这行，不会把 harness 标红（2026-11-09 起）。
- harness 行转「不可用」只有三种来路：`未启动`（启动宽限期内一次连接都没建）、
  `通道断开`（最后一条连接关闭且达阈值）、`无法连接`（没有连接时的连不上）。
  看到这行红，才该去重启 dsh。
