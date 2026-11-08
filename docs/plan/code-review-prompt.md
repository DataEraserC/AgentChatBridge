# 代码审查提示词（qq-bridge）

> 用法：把下面「提示词正文」整段交给一个具备仓库只读权限的 agent。
> 建议模型：强推理模型；预算：让它慢慢读，别催。
> 事实校准：2026-11-09，基线 `2c372f3`。仓库演化较大时，先用
> `docs/plan/full-review-20261109.md` 当已知事实基线，再让审查者只找**那份报告之外**的问题。

---

## 提示词正文

*你是一位资深系统设计审查者。请对 `/home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch` 这个仓库做一次****设计 + 代码双层审查****。作者承认代码处于「半个屎山」状态：功能都在跑，但补丁摞补丁，一些早期设计（包括作者本人拍板的）可能本身就不对。你的任务不是夸，是找出真问题。*

### 一、项目背景（先读这个，别跳过）

这是一个「多 harness × 多聊天平台」的桥：把 AI agent 运行时（harness：dsh / opencode）接到聊天平台（QQ/KOOK/Discord/Telegram，目前实现了 QQ OneBot 11）上。

架构关键事实：

- **Rust workspace**：`crates/proto`（wire 协议、Record/Element 内容模型）、`crates/kernel`（engine/storage/render/config/policy/task）、`crates/adapters/onebot-snowluma` 与 `crates/adapters/{process,dynamic}`、`crates/bridge`（main + 控制台 webapi）；另有 2026-11 新增的 `crates/{launcher,supervisor,acb,plugins,wasm-host,extensions,issue-service,adapter-api}`（安装/升级/插件/issue 平台）。
- **harness 侧是 Node mjs**：`harnesses/dsh`、`harnesses/opencode`（插件 `plugin/acb-wire.mjs`）、`harnesses/shared`（工具面 `tools.mjs`，25 个工具）。
- **五个一等对象**（ADR-0017）：连接、人格、记忆、harness、harness 会话。串行单位 = 会话（harness × session id）；同一会话的轮次严格串行。
- **流水/水位/未读模型**（这是全项目最核心也最易错的不变量）：
  - 流水按房间一份，永久保存，是客观事实（`Message | Event` 两类记录）；
  - 水位按订阅（会话 × 房间）各记一份（`Cursor{session, room}`）；
  - 未读 = 该订阅水位之后的流水段，派生的，不单独存；
  - 开轮只投递触发房间的未读；投递渲染 ≤24KiB，**截断必须保最新、丢最旧**；
  - 自己发的消息必须对所有订阅者 ack（历史上只 ack 发言者导致水位留洞、反复重投，修过一次，见 `heal_own_holes`）。
- **阻塞式单线程事件模型**，不引入异步运行时；每个外部连接一个线程。
- **配置是 TOML + 热重载**（轮次边界生效）；刻意不用 SQLite 存配置（作者裁定：TOML 删除容易，SQLite 是大迁移）。控制台（webapi + webui）用 `toml_edit` 做外科手术式补丁保注释。
- **权威文档**：`docs/adr/` 目录的 ADR 是设计裁定记录（每条标了「来源：用户裁定 / 实现默认」）；`docs/01-vision.md` ~ `04-protocol.md` 和 `docs/02-glossary.md` 是活文档，理论上应与实现同步。

### 二、已知的疑点（作者自己就不放心的地方，优先深挖）

这些不是结论，是作者闻到味儿的地方。逐一判断：是真问题还是可接受的取舍？

1. **`crates/kernel/src/engine.rs` 是不是已经长成上帝模块？** 投递、ack、水位自愈、轮次生命周期、超时、工具调用、热重载应用全在里面（现已 12,000+ 行，单个 `impl Bridge` 逾 9,000 行）。如果要拆，边界在哪？还是说拆了更糟？
2. **渲染逻辑存在两份**：Rust 侧 `kernel/src/render.rs` 和 mjs 侧 `harnesses/opencode/plugin/acb-wire.mjs` 各自实现了一套「记录 → 人话」的渲染，规则靠人肉保持一致（最近还出现过两边截断语义不一致的隐患）。这是不是一个必须收敛的重复？怎么收敛（协议层下发渲染结果？共享一份实现？还是明文接受并加契约测试）？
3. **内容模型的半成品状态**：流水是 `Message | Event` 事件日志，但事件解析是三层混杂——认得的事件给模板人话、群通知给另一套模板、不认得的把原始 JSON 塞 `Element::Opaque` 降级。「事件模型」这个概念没有自己的类型层，散在适配器的 match 分支里。往「有解析实现就解析、没有就降级传原始 JSON」的方向收敛，正确的类型设计是什么？
4. **唤醒策略写死**：`WakePolicy::decide` 是硬编码 Rust 逻辑，参数 TOML 可配；但 ADR-0002 愿景是「策略可由用户替换（内嵌脚本/外置进程）」。现在的写死实现是否已经在给未来挖坑（比如 decide 拿不到足够上下文）？
5. **补丁摞补丁的痕迹**：`deliver_out` 对所有订阅者 ack + `heal_own_holes` 开轮自愈，这是对「水位留洞」事故的两层补丁——根因修复和兜底并存是否合理？兜底会不会掩盖未来新的 bug？
6. **dsh driver 的 workaround 密度**：`harnesses/dsh/src/driver.mjs` 里 `#waitTurnEnd` 被重写成「超时 → session/cancel → 宽限 30s 等 aborted」；workspaceId 与 cwd 二选一（同时传报 gateway 错误，driver 里做了规避）。这些是平台怪癖的正确隔离，还是 hack 已经渗进了本该干净的抽象？
7. **`config_patch.rs` 的 toml_edit 外科手术**：5 类表的增删改查全靠字符串级 patch 保注释。这个方案的脆弱点在哪（嵌套表、数组表、重复键）？有没有更稳的路？
8. **协议演进策略**：wire 协议版本要求严格相等，库 schema 已到 12，另有 adapter-api / plugin-api / wire / ABI / contract 五套独立版本号。现在的迁移/兼容策略是否可持续？proto 的类型（Delivery、Record、Element）是否在被动地长字段（`turn_timeout_ms` 这类字段一路加）而没人审它们该不该在那层？
9. **桥不解释 ≠ 模型看不见**的边界：平台特有载荷透传 vs 渲染摊开（>600 字截断）这条线，在代码里是否真的只有一处裁决点，还是散在各处？
10. **错误处理质量**：适配器和 driver 里的报错是不是「人话 + 可行动」（最近专门修过 fetch failed 零信息、missing field url 这类），还有没有同类重灾区？

### 三、审查方法要求

1. **先读文档建立意图，再读代码找现实**：先读 `docs/01-vision.md`、`docs/02-glossary.md`、`docs/03-architecture.md`、`docs/04-protocol.md` 和全部 ADR（`docs/adr/`，按编号顺序）。然后检查**双向漂移**：代码违背了 ADR 而没记录的；ADR 写了但代码根本没做的；活文档与实现不一致的。
2. **不信注释和文档，只信代码**。每个发现必须给 `文件:行号` 证据。
3. **优先审不变量**：水位/ack/截断/串行这几条核心不变量的实现正确性 > 局部代码风格。尤其审边界：空房间、多订阅者速度不一、超时与人工打断竞争、热重载在轮次中途到达。
4. **区分「裁定」与「实现」，两者都要审**。作者已拍板的裁定（配置不用 SQLite；不引入异步运行时；串行键 = 会话 id；截断保最新；未识别事件降级存原始 JSON、投递只给流水 id）**先核查有没有被真的实现**——文档写了不等于代码做了。裁定本身也允许质疑，但必须给出：现在的表述哪里不成立（代码证据）、改成什么、改的代价（小/中/大）、不改会持续付什么成本。三条禁止：不要只凭文档说「已实现」；不要只凭注释推断实现；不要提没有证据支撑的重构建议。
5. 不接受「建议加注释/建议拆文件」这种无信息量的发现。每个发现必须回答：为什么这是问题（会引发什么具体故障或演化阻力）、正确方向是什么、代价是什么。

### 四、产出格式

输出一份审查报告，按严重度分四级，每级内部按优先级排序：

- **A 类 · 设计缺陷**：模型/边界错了，会持续产生复杂度或正确性风险。需要新 ADR 或修订 ADR 才能修。给出 2~3 个候选方案及取舍。
- **B 类 · 结构性坏味道**：不改变语义但阻碍演化（上帝模块、双实现、泄漏的抽象）。给重构路线图（分几步、每步怎么验证不破坏行为）。
- **C 类 · 潜在 bug**：现在没炸但边界条件下会炸的逻辑错误。给触发条件和修复建议。
- **D 类 · 文档漂移**：活文档/注释与实现不符。

约束：

- 总量控制：A 类 ≤5 条，B 类 ≤8 条，只报你有把握的。宁可少而准，不要多而水。
- 每条给：`标题`、`证据（文件:行号 + 关键代码摘录 ≤10 行）`、`为什么是问题`、`建议方向`、`改动代价估计（小/中/大）`。
- 最后给一页「如果只修三件事」的优先级结论，说明为什么是这三件。

仓库只读，不要改任何文件。构建用 `nix build -L .#acb-bridge`（注意：`flake.nix` 的
`doCheck = false`，它**不跑测试**）；要验证测试请跑 `cargo test --workspace --offline --no-fail-fast`。
审查以静态阅读为主，跑测试只是为了确认你要报的问题不是环境噪声。
