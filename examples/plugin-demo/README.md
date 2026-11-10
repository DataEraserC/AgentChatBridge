# 示例插件：demo.tools

用来验证插件平台的最小闭环，**不动 `crates/kernel` 任何业务代码**。

它演示三样东西：

1. `manifest.json` 里声明一个配置项、一条资源、一个动作；
2. 控制台按 schema 渲染表单，值写进 `plugin_config`，越界/类型不符会被拒；
3. 子进程按标准 JSON-RPC 通过 stdio 提供工具。

## 注册

```bash
acbctl plugin add \
  --id demo.tools \
  --command "$(command -v python3)" \
  --arg "$PWD/examples/plugin-demo/plugin.py" \
  --manifest examples/plugin-demo/manifest.json \
  --timeout 5000 \
  --config ~/.local/share/acb/config/config.toml
```

`--command-kind` 缺省按命令形状推断；`python3` 会走 `profile`，由 `PATH` 解析。

## 配置

```bash
acbctl plugin config demo.tools --config ~/.local/share/acb/config/config.toml
acbctl plugin config demo.tools --set limit=20 --config ~/.local/share/acb/config/config.toml
```

控制台的「插件」页也能改：注册表表格里点「配置」，表单按 manifest 渲染。
`apiToken` 这类 `secret: true` 的字段不会出现明文输入框，只能绑定具名密钥。

## 资源

```bash
acbctl plugin records demo.tools notes --config ~/.local/share/acb/config/config.toml
```

记录存在 `plugin_records` 表里，一行一条，不动态建表。

## 说明

- 动作 `ping` 已声明，子进程也已经实现 `run_action`；核心侧的动作执行器在下一阶段接上。
- 配置通过 `ACB_PLUGIN_<KEY>` 环境变量传给子进程，密钥不进环境变量。
