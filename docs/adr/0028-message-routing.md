# ADR-0028 消息路由：入库前把命令和链接交给代码

来源：用户裁定（2026-11-02）：`/stop` 不该继续写成 core 里的 if，应该变成插件；后续要让
AI 写 Lua 插件处理 `/send`、b23 链接等消息，这类触发不一定是命令前缀。

## 这条决定了什么行为

新增 `route` 扩展点，挂在**连接**上，可以挂多条，按配置顺序执行：

1. `pass`：继续下一个 route provider；全部 `pass` 才走正常 receive/入库/唤醒。
2. `drop`：吃掉，不写流水、不叫醒。
3. `handle`：交给代码处理，可以指定 `store`（命令写不写流水）和 `wake`（要不要让 AI
   也看到），并携带动作列表。

动作是**意图**，core 只负责在锁外执行：

| 动作 | 谁执行 | 用途 |
|------|--------|------|
| `interrupt` | core（无 I/O） | 打断 room / sessions / connection 的活跃轮次，清 pending |
| `send` | 后台 worker | 以桥的身份发消息，走正常发送钩子和文件中继 |
| `task` | 后台 worker | 跑慢活（如 yt-dlp 下载），完成后按结果发送 |

路由判定本身在 core 锁内、必须快（Lua 50ms）。`send` / `task` 的准备在锁内完成，
I/O 和执行在 `RouteWorker` 线程里，不挡入站队列，也不占全局锁。

`/stop` 因此从 core 移出：示例配置挂 `lua.stop_command`，它检查 `context.is_admin`
和命令文本，返回 `interrupt(scope="room")`。命令不写流水；被主动停掉的轮次不按
`on_turn_error` 发失败通知。非管理员发 `/stop` 只是普通消息。

## 为什么不是 receive

`receive` 回答“记不记流水”，route 回答“交给代码还是交给 AI”。把副作用塞进 receive
会让“丢噪音”和“执行命令”两件事纠缠：命令不该被噪声过滤器吃掉，而 `drop` 也不该
顺带发消息。两者分开后，route 可以独立决定 `store` / `wake`，receive 保持纯粹的存储门。

## 已知边界

- 路由 provider 目前只支持进程内载体（Lua / native）；外置进程会在入站路径里做 I/O。
- `task` 的 `exec` 直接跑参数数组（不经 shell），但仍是高权限动作：只挂管理员信任的脚本。
- 运行时挂载 AI 写的 Lua（不重启桥）还没做。预留的控制台 API 是 `plugin.mount` /
  `plugin.unmount`，要求 manifest 声明 `MaySend` / `MayInterrupt` / `MayCallPlatform` /
  `MayNetwork`，未声明的动作拒绝；这块不在本 ADR 的实现范围内。
