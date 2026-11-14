# ADR-0031 适配器自有存储与连接注入（双轨制第一阶段）

状态：**已决定（2026-11-14 用户裁定）；实现已落地**（7fdc97e / aa10422 / 7d587b5，
返工要点 3a613c1 / b909224）。

来源：用户裁定。2026-11-14 讨论中确立四条：

1. 连接相关数据由连接提供者管理，数据库可以是 builtin（自有文件）或外部；
2. 提供者的库是唯一事实来源，重启时回放注册（现状：随桥重启注入生效）；
3. 凭据边界给开发者自由：走 core secrets 引用，或适配器自管，两种都支持；
4. onebot-snowluma 迁成 provider（**未实现**，见文末）。

## 背景

连接原本只存在于 `config.toml` 的 `[connections]`：单一来源但把平台相关的
连接结构强行塞进 core 配置；一个适配器管多条连接（如 onebot 多账号）只能
hack `options_json`；前端可配置化（控制台生成表单）没有落点。

qqbot 适配器接入时暴露了第一个合并点设计错误的实例：早期把合并放在
bridge 的 `main.rs`（`AdapterFactory::connections` 钩子），晚于
`Config::parse` 的订阅校验——删掉 config 连接行后，引用它的订阅触发
`UnknownConnection`，桥启动失败、supervisor 连续重启。该钩子已撤销。

## 决定

### D1. 适配器自有目录与自有库

- `AdapterHost.state_dir`（`state/adapters/<目录>/`，桥启动时创建）归适配器
  所有，core 不读写其中的适配器私有文件。
- 连接库路径固定为 `state/adapters/<目录>/connections.db`，使用 **core 定义
  的标准 schema**（`ADAPTER_CONNECTIONS_SCHEMA_SQL`）：id / platform / kind /
  transport / url / account / token_secret / admin_json / routes_json /
  options_json / created_at。
- schema 归 core（注入器要读懂它），数据归适配器——适配器可以加自己的表。

### D2. core 在配置构建阶段只读注入

- `inject_adapter_connections` 在 `finish_loaded` 里、**订阅校验之前**把各
  适配器库的行转成连接（JSON 字段转 TOML），引用它们的订阅与 config 连接
  同权。
- 规则：config 连接优先（id 冲突的行跳过并告警）；结构不合法的行跳过并告警
  （单行故障不拖垮启动）；语义错误（secret 引用不存在、url 非法）沿用
  fail-closed：启动失败并点名连接。
- core 以只读方式打开适配器的库。

### D3. 凭据边界

- `token_secret` 列存 **secrets 体系的引用名**，值留在 secrets 存储
  （agenix / 控制台 / 库内密钥），解析走普通 secret 通道。
- 适配器自管凭证同样允许（开发者自由）：onebot11 / qqbot 的自有库支持
  AcbKey 加密与明文两种形态，解不开的行跳过。

### D4. 状态根解析

`ACB_DATA_DIR` 环境变量（绝对路径，桥与组件同源继承，不依赖各进程 CWD）→
`[storage].path` → 默认存储路径。相对路径按 CWD 解析；组件进程必须与桥同源
继承，否则注入只在桥进程生效、预设派生的 acbctl 全部落空（3a613c1 修的实测
事故）。

### D5. 变更生效时机

现状 = 重启生效（与 config 连接的 reload 语义一致）。运行时热注册依赖
ADR-0029 的热重载成熟，**未实现**。

## 未实现（显式标注）

- B期 wire 注册协议：**已落地**（2026-11-15，`connections` 推送 +
  `connections_list` 协议变体 + engine 运行时注册表；onebot11 已回放）。
  剩余：core 侧 `connections_list` 触发器（等第一个外置 provider）；
  运行时创建适配器实例（新增 id 仍要重启才起实例）。
- C期：控制台 schema 表单生成（JSON Schema → 表单，`x-secret-ref` 密钥
  选择器）：**已落地**（2026-11-15，`PluginManifest.config_schema` +
  `POST /api/plugins/<id>/secret-bind` + `GET /api/secrets`）。控制台内
  新建具名密钥也已落地（`POST /api/secrets`：值文件 + config.d 声明 +
  AcbKey 密文入库，与铸造流程同构；密钥库卡 + 引用处下拉/内联创建）。
- D期：onebot-snowluma 的 cutover（自有库已就绪，删 config 行待运维窗口）；
  qqbot 的连接注册回放推送（等 AppSecret 录入后按 onebot11 同形补）。
- 外部数据库形态（非本文件 SQLite）。

## 不这样会怎样

- 维持 config 单轨：多连接适配器只能 hack `options_json`，前端配置无落点。
- 把注入留在 bridge 合并层（已撤销的钩子）：合并晚于订阅校验，删 config 行
  直接把桥打死（实测事故）。
- 适配器库存明文凭据：state 目录备份/日志泄漏即凭据泄漏。

## 参考

- 注入实现：`crates/kernel/src/config.rs`（`ADAPTER_CONNECTIONS_SCHEMA_SQL`、
  `inject_adapter_connections`）
- 适配器宿主：`crates/kernel/src/adapter.rs`（`AdapterHost.state_dir`）
- 首个提供者：`crates/adapters/qqbot-official/src/factory.rs`、
  `crates/adapters/onebot-snowluma/src/factory.rs`
- 相关裁定：[ADR-0012](0012-platform-connections.md)、[ADR-0014](0014-state-on-disk.md)、
  [ADR-0027](0027-platform-adapter-registration.md)
