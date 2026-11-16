# ADR-0032 细粒度权限字符（permission strings）

状态：**已决定（2026-11-14 用户裁定匹配语义）；实现已落地**
（878cd2d / 51a12f1 / f42f93c / d6f6c97）。

来源：用户裁定。2026-11-14 讨论中确立：方法除粗粒度 Risk 档外，应可选声明
层级化权限串（如 `qq:group.mute`）；匹配语义为**精确匹配 + 末段单层通配**
（`*` 只能是最后一段）。

## 背景

现有授权粒度是 coarse 档：`Effect`（读/写）+ `Verification`（verified /
unverified）+ `GrantCapability`（room_read … critical）。同一档内部的方法
无法区分——「群禁言」和「群文件列表」如果同属 write 档，规则只能全放或全收。
另外 ToolAccessProvider（否决插件）只拿到工具名与参数，没有稳定的权限身份
可判；后续插件自描述（C期表单）也需要可声明的权限身份。

## 决定

### D1. 语法

`<平台>:<op>[.<op>...]`。`*` 只允许作为**最后一段**且前面有非空前缀
（`qq:group.*` 合法）；裸 `*`、居中 `*`（`qq:gr*oup`）、跨界段
（`qq:group*.list`）、空段、首尾空白一律非法。配置侧由
`validate_permission_string` 拒绝（`GrantsConfig::validate` fail-closed）。

### D2. 匹配语义（用户裁定）

精确匹配，或末段通配：`pattern.strip_suffix('*')` 后按前缀匹配，通配覆盖
结尾所有层级（`qq:group.*` 同时命中 `qq:group.mute` 与
`qq:group.file.list`）。实现为 `permission_matches`，规则 target id 与
permissions **共用同一 matcher**，不另造第二套。

### D3. 声明通道

- `MethodSpec.permissions: Option<Vec<PermissionSpec>>`（grant / title /
  description / default），`None` = 派生 `<平台>:<方法名>`；
  `permission_strings(platform)` 是唯一出口。
- wire 对应 `WirePermissionSpec` + `WireMethodSpec.permissions`
  （`#[serde(default)]`，老适配器不声明则原样派生），core 侧解析并校验
  `default` 枚举。

### D4. 判定接线

- `GrantRule.permissions: Vec<String>`（serde default 空）；空 = 不过滤，
  沿用粗粒度 scope 判定，行为不变。
- `GrantQuery.permissions` 是本次要求串；PlatformOp 规则声明了 permissions
  时，在 method（精确/通配）+ connection 匹配之外追加
  `rule_permissions_cover`（required 至少一项被规则声明覆盖）。
- 规则 target id 依旧承载方法模式（`group.*`），permissions 是它之外的第二
  道过滤——两者正交。

### D5. 执行与展示同源

- 执行层 `prepare_platform_call` 与展示层 `high_risk_grant` 都用
  `spec.permission_strings(平台)` 派生 required——否则出现「展示说要票据、
  执行却放行」的口径分裂。
- `check_tool_access` 对 `platform_call` 从 `args.op` 派生
  `<平台>:<op>` 注入 `ToolAccessContext.required_permissions`；否决插件经
  `call_timed` 拿 owned 快照。空 = 无细粒度身份，行为不变。

### D6. 第一版范围

- 注入与判定全链路落地并有测试（精确 / 通配 / 裸 `*` 拒绝 / coverage /
  配置校验）。
- **未实现**：适配器侧 `permissions` 声明示例（现有适配器均走派生路径）；
  控制台规则编辑器对 permissions 的表单（C期）。

## 不这样会怎样

- 只靠 Risk 档：同档方法无法区分授权，规则只能全放全收。
- 允许任意位置的 `*`：`qq:*.mute` 这类模式语义歧义，通配层级无法解释。
- target id 与 permissions 各写一套 matcher：语义漂移只是时间问题。
- 展示层不派生 required：高危提示与真实判定分叉，用户被引导去申请不必要的
  票据。

## 参考

- 声明与派生：`crates/kernel/src/outbound.rs`（`PermissionSpec`、
  `permission_strings`）；`crates/adapter-api/src/lib.rs`（`WirePermissionSpec`）
- 匹配与校验：`crates/kernel/src/grants.rs`（`permission_matches`、
  `validate_permission_string`、`rule_permissions_cover`）
- 接线：`crates/kernel/src/engine.rs`（`prepare_platform_call`、
  `high_risk_grant`、`check_tool_access`）；`crates/kernel/src/plugin.rs`
  （`ToolAccessContext.required_permissions`）
- 能力档位：[ADR-0020](0020-platform-capabilities.md)
