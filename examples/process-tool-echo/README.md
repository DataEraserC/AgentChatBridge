# 外置进程工具插件：四步跑通

这个例子用 Node 写一个最小的 `echo` 工具，验证 bridge 的子进程工具载体。

## 1. 准备脚本

```bash
node examples/process-tool-echo/plugin.js
```

它从 stdin 逐行读 JSON-RPC 请求，向 stdout 写 `initialize` / `list_tools` /
`call_tool` / `shutdown` 的响应。不要往 stdout 打日志；日志写 stderr。

## 2. 写配置

在 `config.toml` 里加：

```toml
[[tool_plugins]]
id = "echo.tools"
command = "/usr/bin/node"
args = ["/绝对路径/examples/process-tool-echo/plugin.js"]
timeout_ms = 5000
```

- `id` 必须含 namespace，且不能用保留的 `acb`。
- `command` / `args` 只来自受信配置；bridge 不读环境变量里的命令。
- `timeout_ms` 省略默认 5000，允许 100..60000。

## 3. 启动并确认

启动 bridge 后：

```bash
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:3100/api/plugins
```

目录里应出现 `echo.tools`（extension = `tool`）。如果子进程启动失败，bridge
会在启动阶段报错，不会注册半套工具。

## 4. 调用

harness 侧先查 schema：

```json
{"tool":"plugin_tools","arguments":{"name":"echo"}}
```

再调用：

```json
{"tool":"plugin_tool_call","arguments":{"tool":"echo","arguments":{"hello":"world"}}}
```

应返回 `{"hello":"world"}`。

## 边界

- 每次调用超时 → 返回明确超时错误，子进程被杀，provider 标 Unhealthy。
- 子进程崩溃 → 按 1s/2s/4s/8s/16s/30s 退避重启；恢复前调用立即失败。
- 重启后工具集合必须与安装时一致，否则保持 Unhealthy。
- 被杀子进程留下的外部副作用（写文件、开连接）不由 core 清理。
