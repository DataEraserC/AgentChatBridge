# 待决策：ADR-0030 D3 的 native reload 回滚缝

状态：**决策选项，不改 ADR 原文**。日期：2026-11-04。  
范围：C 项；只针对 native `.so` 同路径重载，不涉及 T6 声明式模块。

## 1. 冲突原文与现状

ADR-0030 D3 要求：

> 卸载失败或超时：插件标记为 quarantined，不再接受新调用，桥继续运行；
> 版本回滚到上一份已加载实现（如果仍在内存中）或恢复配置的上一版本。

[ADR-0030](../adr/0030-unload-safe-native-plugin.md) D4 的 unload-safe 契约不保证
“旧库还在内存里”；T4c 的实现是：

1. `DynamicLoader::unload` 取快照 → `manager.uninstall_manager` → 移除 `unloadable`
   → 写 `unloaded` 快照 → `drop(Arc<Library>)` 触发 `dlclose`；
2. `DynamicLoader::reload` 重新 `dlopen` 路径 → 校验 id/UNLOAD_SAFE → 安装。

因此**同路径 native reload 是“先卸载、再加载”**；加载失败时旧版本已经 `dlclose`，
不存在“回滚到上一份已加载实现”。T4c 的 `lastError` 会把失败暴露出来，但插件保持
`unloaded`，不是“旧版本继续工作”。

这与 ADR-0030 D3 的严格表述存在缝，但**不是 Cordis effect/HMR 能解决的**：
effect 栈能保证清理顺序，不能让已经 `dlclose` 的代码回到内存。

## 2. 方案 A：候选 `.so` 预加载探针（严格方案）

### 2.1 流程

```text
候选 .so 复制到唯一临时路径（或 dlmopen 新 namespace）
  → dlopen 候选
  → 校验 ABI v2 / struct_size / flags / manifest / id
  → create 一个探针 handle（不进入正式 provider 表）
  → destroy 探针 handle
  → 成功：在同一安全点 destroy 旧实例、dlclose 旧库、换上新库
  → 失败：dlclose 候选，旧库/provider 继续使用
```

关键点：

- 必须使用**唯一临时路径/独立 namespace**，不能直接 `dlopen` 同路径——同路径会复用
  旧 handle，拿不到新代码。
- 旧库必须在新库验证通过前保持加载；这要求新的 `PluginManager`/`DynamicLoader`
  能同时持有“旧 live 库”和“候选库”，并在安全点原子切换。
- `UNLOAD_SAFE` 契约需要扩展一条：候选探针的 `create`/`destroy` 不得产生不可撤销副作用；
  否则“探针”本身会污染线上状态。

### 2.2 代价

- **实现复杂度高**：临时复制、唯一路径、候选生命周期、旧/新双持有、探针清理。
- **平台相关**：Linux 上同路径 `dlopen` 会复用 handle；要真正拿新代码必须复制/`dlmopen`。
  macOS 行为又不同。
- **符号/全局状态风险**：同一进程加载两个版本的库，可能发生全局符号冲突；
  `UNLOAD_SAFE` 禁止 STB_GNU_UNIQUE/全局状态只能降低概率，不能证明安全。
- **内存**：切换窗口内双份库常驻，RSS 峰值上升；对大型插件明显。
- **测试成本**：需要 good/bad 两个 fixture、探针失败、旧库仍可解析、切换成功、
  RSS 峰值基线。

### 2.3 可验证方式

- fixture `bad_manifest.so`：ABI/manifest 校验失败，旧 provider 仍可解析并正常 decide。
- fixture `create_fail.so`：`create` 返回 null，旧 provider 仍可用。
- fixture `good_v2.so`：候选通过后，旧 provider 不可解析、新 provider 可解析。
- `cargo test -p acb-adapter-dynamic --test reload_candidate`，并记录切换窗口 RSS 峰值。

### 2.4 对现有测试的影响

- T4c 的 `unload_v2` 测试仍保留（单实例卸载/重载语义不变）。
- 新增候选测试不改变 `unload` 的公开语义；`reload_provider` 可能新增“失败时保留旧实例”
  分支，现有“失败保持 unloaded”测试需要按新语义改写。
- 控制台 `lastError` 文案需要区分“候选失败、旧版本继续”和“卸载后加载失败、已不可用”。

## 3. 方案 B：明确同路径 native reload 不可逆（务实方案）

### 3.1 新表述（建议）

在 ADR-0030 D3 追加/明确：

> 同路径 native reload 是“同一安全点的 unload → load”；`dlclose` 不可逆。
> 如果新库加载/校验失败，插件保持 `unloaded`，通过 `lastError` 暴露；
> 旧版本不会自动恢复。需要零停机回滚时，使用 process carrier，或等待候选预加载机制。

### 3.2 代价

- **可用性弱化**：一次坏的重载会让插件暂时不可用，直到人工恢复旧文件/重启。
- **运维要求**：需要在部署流程里保留旧 `.so` 和配置，失败时可手工回退。
- **没有代码改动**：T4c 现在的行为就是方案 B；只需补测试断言 `lastError` 和
  `state=unloaded`，以及控制台文案明确“已卸载，不是回滚”。
- **对 AI 扩展工厂**：AI 写 native 插件后热重载失败会短暂停服；这可以接受，因为
  native 本来要求人工审批。

### 3.3 可验证方式

- 现有 `unload_v2` 测试：卸载后 `lifecycle=unloaded`、`reload` 成功恢复。
- 新增测试：故意把 `.so` 换成坏文件后 `queue_reload`，断言：
  - `reload_provider` 返回错误；
  - `PluginInfo.lifecycle == Unloaded`；
  - `plugin_catalog.lastError` 非空；
  - 旧 provider 不可解析（明确不是“回滚继续工作”）。
- 控制台：`lastError` 可见；`state=unloaded` 而不是 `unloadable`。

### 3.4 对现有测试的影响

- 不需要改 `DynamicLoader`。
- 需要把“失败保持 unloaded”从隐含行为变成显式断言。
- 如果现有测试只断言 `reload_provider().is_err()`，需要补 `lastError`/lifecycle 断言。

## 4. 方案 C：分阶段（推荐）

1. **现在采用方案 B**：把 ADR-0030 D3 表述改准确，T4c 行为与文档一致；
   在控制台/文档明确“同路径 reload 失败 = 已卸载，不是回滚”。
2. **未来按需采用方案 A**：当 AI 扩展工厂需要 native 零停机升级，或 native 插件
   成为高频迭代目标时，再做候选预加载；届时需要新的 ABI/loader 契约与 RSS 预算。
3. **不把 Cordis effect/HMR 当成方案 A 的替代**：它们不能跨 `dlclose` 恢复代码。

## 5. 决策矩阵

| 维度 | 方案 A 候选预加载 | 方案 B 明确不可逆 |
|------|------------------|------------------|
| 旧版本继续工作 | 失败时是 | 失败时否（保持 unloaded） |
| 实现复杂度 | 高（双库、临时路径/dlmopen、探针） | 零代码 |
| 平台风险 | Linux/macOS 差异、全局符号 | 无新增 |
| 内存峰值 | 切换窗口双份库 | 不增加 |
| 测试成本 | 高（多 fixture + RSS） | 低（补断言） |
| 对 AI native 迭代 | 更好（失败不停服） | 较差（失败停服） |
| 与 ADR-0030 D3 原文 | 能满足“回滚” | 需要修订 D3 |
| 与 ADR-0029 D4 | 兼容，仍在安全点 | 兼容，仍在安全点 |

## 6. 代行决策记录（2026-11-04）

用户不在期间，按以下默认决策继续：

- **暂不改 ADR-0030 原文**，运行语义按方案 B：
  - 同路径 native reload 失败 = 插件保持 `unloaded` + `lastError`；
  - 不假装满足“回滚到上一份已加载实现”；
  - 代码注释已标注在
    [adapters/dynamic/src/lib.rs L347](../../crates/adapters/dynamic/src/lib.rs#L347)；
  - ADR 原文待用户回来决定是否改措辞或上方案 A。
- 理由：不改 ADR 原文是用户明确约束；方案 B 与现有 T4c 行为一致，零代码/零兼容性风险。
- 可否回退：本决策只涉及文档/注释，不改变运行行为；删除注释即可回退，ADR 未动。

## 7. 建议

**当前先按方案 B 运行，不改 ADR-0030 原文；待用户决定后再改措辞或上方案 A。**
理由：

- 现有 T4c 实现已经是 B；代码注释已标明“不是回滚”，先消除认知缝，但不越过用户
  “暂不改 ADR 原文”的约束。
- 方案 A 的双库同进程风险与 RSS 峰值需要独立评估，不适合在 T7 前顺带做。
- 用户要求“不要改 ADR 原文”，因此本文只给选项；等用户决定后再改 ADR 或实现 A。
