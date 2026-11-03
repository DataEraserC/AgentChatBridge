# 上下文注入片段协议设计（只出文档，不实施）

状态：设计草案（2026-11-04）。**等待用户确认后再动代码、协议或 harness。**

目标：给插件一个受预算约束的、声明式的「往本轮模型上下文里加一段文字」的能力。
它不是让插件改写对话历史，也不是让插件改投递记录。

依据：用户代行决策 3a（2026-11-04）；`capability-gap-matrix.md` 的 D5/E6/§6.2；
`extension-point-catalog.md` 的「人设/提示词注入」缺口。

---

## 0. 完成时用户能做什么

- 在会话配置里列一个 prompt provider，该 provider 可以在每轮投递时附带一小段
  带来源、带优先级的上下文（例如 Lore、规则提示、检索摘要）。
- 关掉配置里的 provider 引用，注入立即停止；不需要改插件代码。
- 任何一段片段都受单段上限与整轮总上限约束；超限时 core 明确截断或丢弃并留痕，
  不会把超长内容塞进模型上下文。
- 审计日志能回答：这一轮注入了哪些来源、各多少字、是否截断、丢了多少。
- 插件仍然**不能**改记录、人格、记忆、`elements` 真值、水位或工具权限。

## 1. 非目标

- 不提供「插件改写整段历史」「插件替换系统提示词」的接口。
- 不让片段参与 route / wake / access / 水位判定；片段对 core 是不透明数据。
- 不在 core 里做 RAG、摘要、模型调用或网络请求。
- 不为注入单独建一套持久化表；片段不是对话事实。

## 2. 协议字段（`Delivery`，`crates/proto/src/frame.rs`）

新增一个**可选、默认空**的字段：

```rust
pub struct Delivery {
    // ...现有字段不变...
    /// 本轮插件贡献的上下文片段。core 已按预算裁剪、按优先级排好序。
    /// 空 = 没有注入；旧 harness 忽略这个字段即可。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompts: Vec<PromptFragment>,
}

pub struct PromptFragment {
    /// 来源 provider id（core 填，插件不能伪造别的来源）。
    pub source: String,
    /// 片段正文；core 已保证不超过单段上限。
    pub text: String,
    /// 优先级，数值越大越靠前；同优先级按 source 字典序稳定排序。
    pub priority: i32,
    /// `true` = 这段被 core 从更长文本裁剪过，避免 harness 误以为原文就这么短。
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}
```

设计约束：

- `text` 不含来源标签；来源单独在 `source`，由 harness 决定怎么展示。
- `priority` 由插件声明，core 只做排序与预算；不解释业务含义。
- 不提供 `enabled` 字段：开关是「配置里有没有引用这个 provider」，不是片段自述。
- 不加 `PromptAudit` 大对象：审计走日志（见 §6），避免每轮投递再长一截。

## 3. 扩展点形状（实现时再加）

```rust
pub struct PromptContext<'a> {
    pub session: &'a str,
    pub room: &'a RoomId,
    pub platform: &'a str,
    pub reason: TurnReason,
    pub wake_reason: Option<&'a str>,
    /// 本轮开始时间（毫秒），供插件判断时效；不是当前时间。
    pub now: i64,
}

pub struct PromptDraft {
    pub text: String,
    pub priority: i32,
}

pub trait PromptProvider: Send + Sync {
    fn prompts(&self, ctx: &PromptContext<'_>) -> Result<Vec<PromptDraft>, String>;
}
```

- `PromptContext` 只给会话/房间/平台/开轮原因，不给记录列表、Store、票或水位。
  插件要基于历史做判断，自己用 `RecordObserver` 维护状态，不要求 core 把历史递进来。
- 扩展点：`ExtensionPoint::Prompt`；能力：`Capability::PromptContribute`；
  实例化：`ProviderOptions::Prompt` / `ProviderInstance::Prompt`。
- provider 名注册方式与 Observer 相同（安装即注册）；调用点在投递组装时，一轮一次，
  不在每条记录上调用。

## 4. 开关

采用**会话级 provider 引用**（与 `outbound` / `delivery` / `access` 同一模式）：

```toml
[[sessions]]
id = "main"
# 空 = 关闭上下文注入
prompts = ["demo.lore"]
```

- 引用存在 = 启用；删掉引用 = 关闭；不需要新增 `enabled` 布尔。
- 新字段必须同步进 `Config::provider_references()`；被引用的 provider 禁止热卸载，
  否则会出现「配置还引用着、实现已经没了」的悬空引用。
- 插件仍可在某一轮返回空列表 = 这一轮不注入（软开关）；这是插件行为，不是绕过预算。

## 5. 预算、排序与降级

默认常量（实现时可调，先不新增配置项）：

| 项 | 默认 | 理由 |
|---|---|---|
| 单段上限 `PROMPT_FRAGMENT_MAX_CHARS` | 2000 字符 | 一段提示词不该比一条长反馈还长 |
| 整轮上限 `PROMPT_TOTAL_MAX_CHARS` | 6000 字符 | 给记录/人格/记忆留余量；帧上限仍是 1 MiB |
| 最多片段数 `PROMPT_MAX_FRAGMENTS` | 8 | 防止大量小片段把上下文切碎 |

处理顺序：

1. 逐个 provider 同步调用，收集 `PromptDraft`；空串/纯空白直接丢。
2. 单段超 `PROMPT_FRAGMENT_MAX_CHARS`：按字符裁剪，`truncated = true`，记日志。
3. 按 `priority` 降序、`source` 升序稳定排序。
4. 从头累加字符：能完整放下的收；会超整轮上限的整段跳过（不切一半），记跳过数；
   如果**第一段**单独就超过整轮上限，裁剪到整轮上限并标 `truncated`——保证高优先级
   来源至少能说出一部分，而不是整轮为空。
5. 片段数超 `PROMPT_MAX_FRAGMENTS`：保留排序后的前 N 段，其余整段丢弃并记日志。
6. 最终仍受 `MAX_FRAME_BYTES` 兜底：编码后超帧上限按现有断连逻辑处理，不静默截断。

降级原则：**宁可少注入，不可超预算；截断必须可见，丢弃必须留痕。**

## 6. 审计落点

- **不写 store**：片段不是对话事实。写进记录会污染水位、未读、`fetch_records` 和
  重启后的历史重放，等于让插件间接改写历史。
- core 在投递组装时写结构化日志（现有 stderr → journald），每条投递至少一行：
  `prompt_audit ticket=… session=… room=… sources=… chars=… truncated=… dropped=…`
- 每个来源单独一行（便于对账）：
  `prompt_source provider=… chars=… priority=… truncated=… status=ok|error|skipped`
- 调用失败/超限/被丢弃的 provider 必须出现在日志里，不能静默。
- `Delivery.prompts[].source` 让 harness 侧日志也能把片段归因到 provider。
- 以后若需要控制台审计环，从日志聚合即可；本设计不新增持久表。

## 7. 失败与超限

| 情况 | 行为 |
|---|---|
| provider 返回 `Err` | 丢掉该 provider 的全部片段，记 `status=error`，其它 provider 继续 |
| provider 返回空列表 | 合法，等于本轮不贡献 |
| 单段超长 | 裁剪 + `truncated = true` + 日志 |
| 整轮预算不足 | 低优先级整段跳过 + 日志；高优先级优先保留 |
| 片段数超限 | 保留前 N 段 + 日志 |
| 同步调用很慢 | MVP 不设 core 超时；provider 契约要求快、只读、不做网络/磁盘 I/O；慢调用会拖慢整轮，靠日志中的耗时定位 |
| provider panic | 与其他 provider hook 一致：视为插件 bug，不在 core 里 `catch_unwind` 掩盖；实现阶段再评估是否需要隔离 |

## 8. Harness 渲染契约（实现时同步改 harness）

- `dsh` 的 `#composePrompt` 与 `opencode` 的 `renderTurn` 增加 `renderPrompts(delivery.prompts)`。
- 渲染位置：人格之后、记录正文之前；用独立的标题块，例如
  `[插件上下文]`，每个片段前标 `[来源: <provider id>]`。
- **不得**把片段拼进记录行、不得把它伪装成用户消息、不得据此生成 `record`。
- 片段顺序由 core 排好；harness 不再重排。
- 旧 harness 不认识 `prompts` 时直接忽略；新 harness 收到空列表时输出为空。
- 片段是插件提供的不可信文本；harness 必须用标题/分隔符标明来源，不能把它当作
  系统指令或用户原话的一部分。

## 9. 兼容与发布顺序

1. 先加 `Delivery.prompts`（可选字段，旧帧/旧 harness 兼容），不动 `PROTOCOL_VERSION`
   （这是向后兼容的加法，不是破坏性变更）。
2. 再加 core 的 `PromptProvider` 扩展点、会话配置引用、预算与审计。
3. 最后改 dsh / opencode 渲染；在 harness 未升级前，片段只会被忽略，不会报错。

每一步独立提交；harness 渲染与 core 预算要各有用例。

## 10. 验收标准（实现阶段）

1. 配置一个 provider → `Delivery.prompts` 出现该片段，`source` 是 provider id，
   顺序符合优先级。
2. 单段超 2000 字符被裁剪且 `truncated = true`；整轮总量不超过 6000 字符。
3. 两个 provider 一个报错、一个正常：投递仍成功，正常片段还在，错误留日志。
4. 删除会话配置里的引用：本轮没有片段；被引用的 provider 卸载被拒绝。
5. harness 测试断言：片段渲染在人格之后、记录之前，带来源标签，不进入 `records`。
6. 注入前后 `records`、`persona`、`memories`、水位、未读计数不变。

## 11. 待确认

1. 开关用**会话级引用**（本稿推荐），还是全局 provider 列表 + 全局开关？
2. 上限数值（2000 / 6000 / 8）是否合适；字符还是字节？
3. 整轮超预算时「整段跳过」是否可接受，还是宁可把最后一段切一半？
4. 审计只进 journald 日志，还是控制台要一个可查询的最近 N 轮审计环？
5. harness 渲染放在人格之后，还是单独塞进系统提示词层（不同 harness 能力不同）？
6. 时钟轮（`reason = clock`）是否也允许注入？本稿默认允许，由 provider 自己判断。
