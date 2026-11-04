# 外置进程工具载体（MVP）

状态：已实现（2026-11-04）。只承载第三方工具；第一方与热路径继续走进程内
`ToolLoader`。

## 协议

子进程与 bridge 在 stdio 上按行交换 JSON-RPC 2.0：

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
{"jsonrpc":"2.0","id":2,"method":"list_tools","params":{}}
{"jsonrpc":"2.0","id":3,"method":"call_tool","params":{"name":"echo","arguments":{}}}
{"jsonrpc":"2.0","id":4,"method":"shutdown","params":{}}
```

- `initialize` → `{"id":"echo.tools","version":"0.1.0"}`
- `list_tools` → `{"tools":[{"name","summary","inputSchema"}]}`
- `call_tool` → `{"result": <JSON>}`
- `shutdown` → `{}`，随后进程应退出；1 秒内不退出由 bridge 强杀。

stdout 只能输出协议行；日志写 stderr。

## 配置

```toml
[[tool_plugins]]
id = "echo.tools"          # ProviderId，必须含 namespace，不能用 acb
command = "/usr/bin/node"
args = ["/opt/acb/echo.js"]
timeout_ms = 5000          # 可选，默认 5000，范围 100..60000
```

命令与参数只来自 `config.toml`；不从 env、网络或插件自述读取。

## 生命周期与健康

- 安装时 spawn 子进程并完成 `initialize` + `list_tools`；任一步超时/失败/工具集为空，
  整个 loader 安装失败，bridge 启动报明确错误。
- 每次 `call_tool` 有独立超时；超时后 kill 子进程，provider 标 Unhealthy，调用返回
  `Timeout` 错误。
- 子进程退出/读失败 → Unhealthy，后台 supervisor 按
  `1s, 2s, 4s, 8s, 16s, 30s...` 退避重启；重启需重新通过 initialize/list_tools，
  且工具集合必须与安装时一致。
- Unhealthy 期间调用立即返回 `Internal`（不可用）错误，不排队、不阻塞。
- 调用队列容量 64；满时立即返回超时/不可用错误。
- 健康状态通过 `PluginManager::catalog()` 的 `health` 字段暴露。

## 工具名冲突

由 `PluginManager::register_tools()` 统一拒绝：与内置工具或其它插件工具重名时，
安装失败并回滚已注册项，不留半套。

## 残留与边界

- 被杀子进程留下的外部副作用（文件、连接、外部任务）不由 core 清理。
- 不做权限模型、签名、市场、工具热更新。
- 进程载体的 provider 暂不支持热卸载/重载；配置变更需要重启 bridge。
