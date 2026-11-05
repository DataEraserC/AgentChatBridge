# WASM 与 `.so` 两种载体的参考实现

状态：**设计 + 最小实现（2026-11-05）**。两种载体都落地，同一份 access 决策
分别由 WASM 模块和 ABI v2 `.so` 实现，并用一致性测试锁定输出。

## 1. 两种载体怎么挂进 core

```text
core (acb-bridge)
  ├─ PluginLoader::Process  ← acb-plugin-process-provider（不认识 wasm）
  │    └─ 子进程 acb-wasm-host ── wasmi ── access.wasm
  └─ PluginLoader::Dynamic  ← acb-adapter-dynamic（dlopen）
       └─ libacb_access_so_fixture.so（ABI v2 + UNLOAD_SAFE）
```

- **WASM 载体**：`crates/wasm-host` 产出独立宿主进程 `acb-wasm-host`。core 通过
  `crates/plugins/process-provider` 的 `ProcessProviderLoader` 启动它，只做
  stdio 行 JSON；core 的依赖图里没有 `wasmi`。
  说明：仓库原本只有 `ProcessAdapterFactory` 和 `ProcessToolLoader`，没有通用的
  进程 provider loader；本次按同构方式补了 `ProcessProviderLoader`（同样是
  `PluginLoader`，只吃 stdio JSON，不认识 wasm），它不把任何运行时搬进 core。
- **`.so` 载体**：参考实现是 `crates/fixtures/access-so`（ABI v2，
  `PLUGIN_FLAG_UNLOAD_SAFE`），由现有 `DynamicLoader` 通过 `[[plugins]]` 装入，
  provider 走 `AccessProvider` 同一条 `resolve_access` 路径。
- 两种载体都注册同一个扩展点：**access provider**（`ExtensionPoint::Access`，
  `Capability::AccessDecision`），会话用 `[sessions.access] provider = ...` 引用。

WASM 宿主的协议（stdout，一行一条 JSON）：

```json
{"type":"provider_manifest","provider":"demo.wasm_access","extension":"access","api_version":1,"metadata":{...}}
```

之后 core 发送现有的 `CoreMessage::Decide`：

```json
{"type":"decide","id":7,"point":"access","options":{...},"request":{...}}
```

宿主回现有的 `AdapterMessage::Reply`：

```json
{"type":"reply","id":7,"result":{"outcome":"decision","decision":"allow","reason":"room allowlisted"}}
```

请求/回执复用 `acb-adapter-api` 的现成类型，不新增第二套 wire。

## 2. 参考能力（两种载体必须一致）

固定的 access 规则，便于断言：

| 输入目标房间 | 决策 | reason |
|---|---|---|
| `qq:group/1` | `allow` | `room allowlisted` |
| 其他房间 | `deny` | `room not allowlisted` |
| 无目标房间（connection 级） | `deny` | `no room` |

WASM 模块 ABI（`crates/wasm-host/fixtures/access_module.rs`）：

- `memory`：线性内存；
- `acb_alloc(len: i32) -> i32`：返回写请求 JSON 的缓冲区指针；
- `acb_decide(ptr: i32, len: i32) -> i32`：返回结果缓冲区指针；
- `acb_result_len() -> i32`：结果字节数。

`.so` 实现同一规则：解码 `CoreMessage::Decide`，读 `request.room`，回
`WireResult::Decision`。两边的原因字符串也保持一致，一致性测试比较
`AccessDecision`。

WASM 参考模块的 `.wasm` 已提交在 `crates/wasm-host/fixtures/access.wasm`，
源码在 `access_module.rs`；重编脚本是 `build-access-wasm.sh`（需要
`wasm32-unknown-unknown` 和 `wasm-ld`）。

## 3. 失败模式映射

| 失败 | 载体 | 表现 | core 侧 |
|---|---|---|---|
| `.wasm` 文件缺失 / 解析失败 | WASM | 宿主启动失败，退出 66 | `ProcessProviderLoader::install` 返回 `PluginError::Loader`；桥启动失败（退出 78），不装成半套 |
| 缺 `acb_decide` / `acb_alloc` / `memory` 导出 | WASM | 宿主启动失败，stderr 打印 `missing export: ...`，退出 66 | 同上 |
| 模块初始内存超过上限 | WASM | 宿主启动失败，stderr 打印 `memory limit exceeded`，退出 66 | 同上 |
| `trap` / fuel 耗尽 / 决策 JSON 非法 | WASM | 宿主退出 70 | 读线程把通道标 down；`ManagerPlugin::health()` 返回 `Unhealthy`；后续 `check` 一律 `deny`（fail-closed） |
| decide 超时 / 子进程不读 stdin | WASM | 通道标记 down 并 kill 子进程 | 本次及后续 `check` 返回 `deny`；catalog/manager health 可见 |
| ABI 版本不匹配 | `.so` | loader 在调用任何函数指针之前返回 `abi_incompatible` | `PluginError::Loader`；不会读错函数表，不会段错误 |
| 缺导出符号 / 函数表不完整 | `.so` | `UnloadSafeLibrary` / `DynamicLoader` 返回明确错误 | `PluginError::Loader` |
| 插件未声明 `UNLOAD_SAFE` | `.so` | loader 拒绝按可卸载模式加载 | `PluginError::Loader`；v2 入口存在但 flags 不满足 |

运行期健康沿用现有规则：`Health::Unhealthy` 会进入 `plugin_catalog`，由既有的
健康/阈值路径消费；进程 provider 没有 LKG 产物，因此不参与动态库回滚。
**硬失败不重启**：宿主 trap 后直接 down，避免「越崩越重启」的循环。

## 4. 配置与部署两层开关

**配置层（加载哪些）**：

```toml
# WASM：core 只负责启动命令，不认识 wasm
[[process_providers]]
id = "demo.wasm_access"
extension = "access"
command = ["/绝对路径/acb-wasm-host", "--module", "/绝对路径/access.wasm", "--provider", "demo.wasm_access"]
startup_timeout_secs = 10
timeout_ms = 5000

# 会话引用
[sessions.access]
provider = "demo.wasm_access"
```

```toml
# .so：现有动态库路径
[[plugins]]
path = "/绝对路径/libacb_access_so_fixture.so"

[sessions.access]
provider = "demo.access_allowlist"
```

列表为空 = 不加载任何东西；`.so` 不写进 `[[plugins]]` = 不 `dlopen`。
当前每个 provider namespace 只能有一个 `[[process_providers]]` 条目（一个进程宿主
对应一个 manager）；同一 namespace 的第二个条目会在启动解析期报错。

桥退出时 `Bridge::shutdown_plugins()` 会调用 manager 的 `shutdown()`：进程宿主
先收到 `shutdown` 行，超过 2s 才 kill。这样手动运行（没有 systemd
`KillMode=control-group` 兜底）也不会留下孤儿子进程。

**部署层（装不装载体）**：

- WASM：装 `acb-wasm-host` 二进制才可能加载；不装就是最彻底的禁用。
- `.so`：装/放 `.so` 文件才可能 `dlopen`；不装就是禁用。
- 不做「明确拒绝加载 wasm 插件」的构建变体。两种载体都不进用户默认环境；
  Nix 只在可选 module `deploy/nix/wasm-host.nix` / `wasm-host-home-manager.nix`
  里提供宿主包。
- `[[process_providers]]` 的命令既可以是 PATH 里的 `acb-wasm-host`，也可以是
  绝对路径；部署侧用 Nix 固化绝对路径更可控。

## 5. Feature flag

| feature | 所在的 crate | 默认 | 作用 |
|---|---|---|---|
| `wasm-host` | `acb-wasm-host` | 关 | 编译 `wasmi` 和 `acb-wasm-host` 二进制；关闭时 core/workspace 默认构建完全不碰 wasm 运行时 |
| `native-so` | `acb-wasm-host` | 关 | 构建两个参考 `.so` fixture 并启用 `.so` / 一致性测试；它隐含 `wasm-host` |

默认 `cargo build --workspace` 不编译 `wasmi`，也不产出 `acb-wasm-host` 二进制
（`required-features = ["wasm-host"]`）。验证命令：

```bash
cargo test -p acb-wasm-host --features wasm-host
cargo test -p acb-wasm-host --features native-so
```

`.so` fixture 不在 root workspace 成员里（`exclude`），只有 `native-so` 打开时
才作为可选依赖被构建。

## 6. Windows 与跨平台边界

- 宿主进程只用 `std::process` / stdio / 文件路径，不依赖 Unix 信号、`/proc`、
  `fork` 或 shebang；Windows 上应能以 `acb-wasm-host.exe` 运行。
- `.so` fixture 的定位按平台后缀选择（`.so` / `.dylib` / `.dll`），但本次只在
  Linux 实测；Windows 的 `.dll` 加载未实测。
- 宿主不假设路径分隔符；Nix module 里用绝对路径，Windows 部署由调用方给绝对路径。

## 7. 测试覆盖

`crates/wasm-host/tests/carriers.rs`：

- `wasm_access_provider_returns_expected_decisions`：WASM 正常调用，allow/deny/无房间
  三种结果；
- `wasm_trap_is_a_hard_failure`：trap 后 provider 返回 deny 且 manager health
  变为 `Unhealthy`；
- `missing_export_fails_before_manifest`：宿主直接报 `missing export: acb_decide`，
  且 `ProcessProviderLoader` 拒绝安装；
- `wasm_memory_limit_is_enforced`：内存超限在启动期硬失败；
- `so_v2_provider_loads_registers_calls_and_unloads`（`native-so`）：真实 `dlopen`、
  注册、调用、`unload_provider` 卸载；
- `so_abi_mismatch_is_a_clear_error`（`native-so`）：ABI 版本不匹配得到明确错误，
  不发生段错误；
- `wasm_and_so_agree_on_the_same_requests`（`native-so`）：同一组请求分别喂给
  两种载体，断言决策一致。
