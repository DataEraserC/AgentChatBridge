# 待评审：把 `plugin_references` 改成声明式依赖收集

状态：**设计稿，不改代码**。日期：2026-11-04。  
范围：B 项；只解决“被当前 config 引用的 provider 不能卸载”这一处手写扫描，不改 ADR，不改配置格式。

## 1. 现状

[engine.rs L484-L527](../../crates/kernel/src/engine.rs#L484-L527) 的 `plugin_references` 手工扫描：

- `connection.receive.provider`
- `connection.routes[].provider`
- `session.access.provider`
- `session.delivery.provider`
- `session.outbound.before/after`
- `session.turn_error.provider`
- `subscription.wake.provider`
- `subscription.clock.provider`

返回 `session:main wake qq:group/1` 这类位置串，供 `queue_unload` / `queue_reload` 判断
“被引用就不能热卸载”。

问题不是性能，而是**可扩展性**：

- 新增一个 provider-bearing 配置字段（T7 `task`、T8 反馈通知等）时，必须记得改这个函数；
- 忘了改的后果是“误判为无引用”，随后 `queue_unload` 允许卸载，下一轮解析配置失败；
- 位置串格式散落在 engine.rs，配置结构本身不知道“我引用了哪些 provider”。

## 2. 最小设计

### 2.1 新增类型

在 `crates/kernel/src/config.rs` 增加：

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderReference {
    pub id: ProviderId,
    pub location: String,
}
```

`location` 保持现在的稳定格式，例如：

- `connection:qq receive`
- `connection:qq route`
- `session:main access`
- `session:main delivery`
- `session:main outbound`
- `session:main turn_error`
- `session:main wake qq:group/123`
- `session:main clock qq:group/123`

### 2.2 配置侧收集

在 `config.rs` 加：

```rust
impl Config {
    pub fn provider_references(&self) -> Vec<ProviderReference> { ... }
}
impl Connection {
    fn provider_references(&self) -> Vec<ProviderReference> { ... }
}
impl Session {
    fn provider_references(&self) -> Vec<ProviderReference> { ... }
}
impl Subscription {
    fn provider_references(&self) -> Vec<ProviderReference> { ... }
}
```

只收集“当前 config 里真正引用 provider”的字段。`options` 里的插件私有 JSON 不扫描——
它没有类型约束，无法可靠判断哪个字符串是 provider id。

### 2.3 engine 侧

把 `Bridge::plugin_references` 改成：

```rust
fn plugin_references(&self, id: &ProviderId) -> Vec<String> {
    self.config
        .provider_references()
        .into_iter()
        .filter(|reference| &reference.id == id)
        .map(|reference| reference.location)
        .collect()
}
```

调用点、`queue_unload` / `queue_reload` 的语义、`refs` 输出格式保持不变。

### 2.4 为什么不是 derive 宏

可选强方案是给配置结构加 `#[derive(ProviderRefs)]`，让编译器在新增字段时强制更新。
代价是：

- 需要 proc-macro crate，增加构建/维护面；
- `ProviderId` 可能嵌套在 `Option`/`Vec`/policy struct 里，derive 要处理多种形状；
- 当前配置结构数量有限，手写集中方法更小、更容易 review。

因此**最小方案先不引入宏**；如果未来 provider-bearing 字段继续增长，再评估 derive。

## 3. 兼容性

- 配置文件格式不变；TOML 字段不变。
- `PluginInfo` JSON 的 `refs` 字段格式不变（仍返回位置串）。
- `queue_unload` / `queue_reload` 的拒绝条件不变。
- C ABI / LUA / 进程 wire 不变。
- 只移动代码位置，不改变行为。

## 4. 风险

1. **仍然不是编译期强制**：新增字段如果忘记加进 `Config::provider_references`，仍然会漏。
   缓解：把方法放在 `config.rs` 的配置结构旁边；加一个覆盖全部当前字段的测试；review checklist。
2. **`options` 里的隐式引用**：插件私有 JSON 可能包含 provider id（例如 `options.target`），
   当前和未来都不扫描。缓解：provider-bearing 字段应是一等配置字段，不放在自由 JSON。
3. **重复位置**：同一 connector 配多个 route 且 provider 相同，当前会产生重复 location。
   最小方案保持现状（去重只是展示优化，不影响拒绝逻辑）；如需要可在收集后 `dedup`。
4. **动态模块依赖**：本设计只回答“config 引用了谁”，不回答“模块 A 依赖模块 B”。
   后者属于 Cordis 评估里的 `requires`/`provides`，见
   [09-cordis-mechanism-evaluation.md](../09-cordis-mechanism-evaluation.md)。

## 5. 测试计划

1. 新增 `config_provider_references_cover_all_current_fields`：
   构造一份配置，让同一个 `test.provider` 同时出现在：
   `receive` / `route` / `access` / `delivery` / `outbound` / `turn_error` / `wake` / `clock`；
   断言 `Config::provider_references()` 返回 8 条位置，字符串与当前 `plugin_references` 一致。
2. 保留现有 `plugin_lifecycle::referenced_plugin_rejects_unload`，确保行为不变。
3. 加一条反向用例：没有引用的 provider 不出现在 `provider_references()`。
4. `cargo test --workspace`。

## 6. 落地顺序（若批准）

1. `config.rs` 加 `ProviderReference` + 三个收集方法 + 测试。
2. `engine.rs` 的 `plugin_references` 改为过滤 `config.provider_references()`。
3. 保留 `plugin_lifecycle` 测试；补覆盖用例。
4. 独立提交：`refactor(config): centralize provider reference collection`。

## 7. 建议

**建议采纳最小方案**：先集中到 `config.rs`，不引入 derive 宏；它不改变任何外部行为，
但把“新增字段忘记改 engine.rs”的遗漏点收敛到配置结构旁边。若未来 T7/T8 的
provider-bearing 字段超过 3 个，再评估 derive/宏或统一配置 IR。
