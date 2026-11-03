# ADR-0030 unload-safe native plugin：ABI v2 与可卸载契约

状态：**已决定（2026-11-03 用户裁定：做一个 unload-safe native plugin）**；实现待做。
本文定义 ABI 与生命周期，不把“C ABI”误当成“可卸载”的充分条件。

来源：用户裁定（2026-11-03：在 ADR-0029 讨论中确认 native 插件需要一条可卸载路径）；
技术细节为实现默认。

## 背景

现有动态库 ABI（ADR-0026 / ADR-0027）已经是 C ABI：

- 唯一导出符号 `acb_adapter_entry_v1`；
- 返回 `AdapterV1` 函数表；
- 跨边界只有 `char*` / `void*` / 函数指针，文本是 JSON。

问题不在 ABI 形状，而在**生命周期**：

- `acb-adapter-dynamic` 用 `libloading::Library` 装载后，把 `Library` 保存在加载结果里；
- `PluginLoader::unload` 默认不支持；
- provider / adapter 实例、在途轮次、后台线程都可能持有插件函数指针；
- Rust cdylib 常带 TLS、全局状态、panic 运行时和析构器，`dlclose` 后继续使用是未定义行为；
- glibc 还可能因 `STB_GNU_UNIQUE` / TLS / `NODELETE` 拒绝真正卸载。

因此必须新增一个**显式声明可卸载**的 ABI 版本，而不是给 v1 加一个 `unload` 开关。

## 决定

### D1. 新增 ABI v2，v1 保持不变

- 新导出符号：`acb_plugin_entry_v2`。
- v1 的 `.so` 继续按“加载后常驻”处理；升级需要重启桥。
- v2 是 opt-in：只有声明 `UNLOAD_SAFE` 的插件才允许走卸载路径。
- loader 先找 `acb_plugin_entry_v2`，找不到再回退 v1。

### D2. v2 函数表包含版本、大小和标志

```c
typedef struct {
    uint32_t abi_version;   // 2
    uint32_t struct_size;   // sizeof(AcbPluginV2)
    uint32_t flags;         // ACB_PLUGIN_FLAG_UNLOAD_SAFE | ...
    const char* (*manifest)(void);
    void* (*create)(const char* config_json, const HostV1* host);
    const char* (*invoke)(void* handle, const char* request_json);
    void (*destroy)(void* handle);
    void (*free_string)(const char* text);
} AcbPluginV2;

const AcbPluginV2* acb_plugin_entry_v2(void);
```

- `struct_size` 允许后续加字段而不破坏旧 loader。
- `flags` 至少定义 `ACB_PLUGIN_FLAG_UNLOAD_SAFE`；未声明时按 v1 常驻处理。
- wire 协议沿用 `acb-adapter-api` 的 JSON，不另造一套消息格式。
- 所有跨边界内存由插件自己的 `free_string` / `destroy` 释放；core 不保存插件分配的指针。

### D3. 卸载只在安全点发生

卸载顺序固定：

```text
停止新调用
  → 等当前轮次结束（策略类在轮次边界，适配器先断连接并 drain 入站队列）
  → 对每个 handle 调 destroy
  → 丢弃 provider / adapter 实例
  → 丢弃 Library（dlclose）
  → 记录审计：插件 id、版本、原因、结果
```

- 策略类扩展点（wake / access / delivery / send / turn_error / clock / receive / route）
  先支持；平台适配器最后做，因为它还持有连接线程和事件队列。
- 卸载失败或超时：插件标记为 quarantined，不再接受新调用，桥继续运行；版本回滚到上一份已加载实现（如果仍在内存中）或恢复配置的上一版本。
- 任务/后台线程必须在 `destroy` 前停止；core 不为插件线程做生命周期托管。

### D4. unload-safe 契约

插件声明 `UNLOAD_SAFE` 即承诺：

1. 不创建线程，或创建的线程在 `destroy` 前全部 join；
2. 不使用线程局部存储（TLS）；
3. 不依赖卸载后仍会被执行的全局析构器；
4. 不把回调函数指针保存到 `destroy` 之后；
5. 所有跨边界分配由插件自己释放；
6. 不依赖 `STB_GNU_UNIQUE` 符号或 glibc `NODELETE` 行为；
7. `destroy` 之后不再调用 core 的 `HostV1` 回调。

loader 无法在运行时验证 1–7；只能通过：

- `UNLOAD_SAFE` 声明；
- conformance fixture（加载 → 调用 → destroy → dlclose → 重新 dlopen）；
- 代码审计与文档；
- 需要强保证时改用外置进程载体。

违反契约导致崩溃属于插件责任，桥不把 native 插件当作隔离边界。

### D5. 第一版范围

- 新增 `acb-plugin-entry-v2` fixture（C 或纯 C ABI 的最小实现）；
- `DynamicLoader` 支持 v2，并能卸载声明 `UNLOAD_SAFE` 的策略插件；
- `PluginLoader::unload` 在 Root 层提供安全点调度；
- 平台适配器的 v2 支持留到后续阶段；
- Rust cdylib 默认不声明 `UNLOAD_SAFE`；需要时由作者自行满足契约并加 conformance 测试。

## 不这样会怎样

- 直接给 v1 加 `dlclose`：Rust 插件、仍在途的 provider、TLS 和全局析构会让桥随机崩溃。
- 只做“重启桥”不做 v2：AI 的 native 扩展无法形成开发闭环，扩展工厂每次迭代都要重启整座桥。
- 把 C ABI 当成卸载保证：C 插件同样可以起线程、用 TLS、注册回调；ABI 语言不等于生命周期安全。

## 参考

- 扩展宿主：[ADR-0026](0026-plugin-host.md)
- 适配器注册：[ADR-0027](0027-platform-adapter-registration.md)
- 动态扩展与扩展工厂：[ADR-0029](0029-dynamic-extensions.md)
- ABI 类型：[crates/adapter-api](../../crates/adapter-api/src/lib.rs)
- 现有 loader：[crates/adapters/dynamic](../../crates/adapters/dynamic/src/lib.rs)
