# 嵌套 manager 示例

`ManagerPlugin::children()` 可以返回子 manager；Root 会先安装父 manager，再递归安装
子 manager。这个示例演示：

- 父 manager `parent` 只做容器，自己不提供 provider；
- 子 manager `child` 提供 `child.always`；
- 配置直接引用 `child.always`，由 Root 委托给子 manager实例化；
- `manager_health()` 的顺序是父在前、子在后（安装顺序）。

## 构建与测试

```bash
cargo test --manifest-path examples/dynamic/nested-manager/Cargo.toml
```

这个 crate 故意不加入根 workspace，单独构建不会拖慢主仓库。

## 装配

嵌套 manager 是进程内 native 扩展，需要编译进桥（或由自定义 `PluginLoader` 安装），
不能像 `.so` 插件那样只在配置里写路径。启动时：

```rust
let mut plugins = PluginManager::with_native_builtins();
acb_example_nested_manager::install(&mut plugins)?;
let bridge = Bridge::with_plugins(config, store, outbound, plugins);
```

配置：

```toml
[sessions.subscriptions.wake]
provider = "child.always"
```

## 命名规则

manager 树是安装/生命周期结构，不编码进 provider id。`ProviderId` 的 namespace 只有
第一段：`child.always` 的 namespace 是 `child`，所以配置写 `child.always`，不是
`parent.child.always`。当前的 `install_manager` 也不接受含点的 namespace。

## 深度与环路

Root 对 manager 子树做：

- 深度上限 `MAX_MANAGER_DEPTH`（4 层，Root 算第 0 层）；
- 环路检测（同一 namespace 重复出现在祖先链里）；
- namespace / descriptor / capability / source 审计。

这些约束在 `crates/kernel/src/plugin.rs` 的 `install_manager` / `collect_manager` 里，
本示例的父→子结构在限制内。
