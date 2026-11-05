# SnowLuma 适配器：改名与平台能力实现调研

> 状态：调研 + 设计，不含实现，不提交。
> 代码基线：workspace `0.1.0`，内置适配器 `crates/adapters/onebot11`。
> 上游资料抓取日期：2026-11-04。站点页面自报最后更新：2026-10-27。
> 主要来源：
> - <https://snowluma.github.io/zh/docs/guide/introduction>
> - <https://snowluma.github.io/zh/docs/guide/configuration>
> - <https://snowluma.github.io/zh/docs/guide/connect-bot>
> - <https://snowluma.github.io/zh/docs/guide/developer>
> - <https://snowluma.github.io/zh/docs/guide/deploy/update>
> - <https://snowluma.github.io/zh/docs/api>
> - <https://snowluma.github.io/zh/docs/api/system/get_version_info>
> - <https://snowluma.github.io/zh/docs/api/system/get_status>
> - <https://snowluma.github.io/zh/docs/sdk>
> - <https://snowluma.github.io/zh/docs/mcp>
> - <https://github.com/SnowLuma/SnowLuma/releases>（当前最新 tag `v2.4.0`，2026-11-03）

---

## 0. 结论先行

- **命名推荐**：`onebot-snowluma`。crate / Rust 类型 / 显示名改成这个名字；配置 `kind = "onebot11"` 和 provider id `acb.adapter.onebot11` **保持不动**，新增 `onebot-snowluma` 作为 kind 别名。这样名字同时涵盖协议（OneBot）与实现（SnowLuma），又不破坏现有配置和 LKG 持久化。
- **缺口总量**：按功能域拆成 25 项，其中 2 项不适用；适用的 23 项里，**完全实现 8 项、部分实现 9 项、未实现 6 项**。动作级：现有 catalog 暴露 91 个层级 op / 94 个 OneBot action；SnowLuma 中文 API 页分类合计 200 个、英文页 219 个（两页计数本身不一致，只能当量级参考）。
- **实施顺序**：批次 0 改名 → 批次 1 版本/状态采集 → 批次 2 事件通道与平台健康 → 批次 3 消息/群管/成员/好友/请求/公告目录 → 批次 4 文件 CRUD 与表情扩展 → 批次 5 连接形态与鉴权 → 批次 6 流式与媒体 → 批次 7 相册/空间/收藏/闪传等扩展域。批次 6 必须先完成批次 5；若部署只用 HTTP API，批次 5 也要提前到任何消息能力之前。

---

## 1. 现状盘查

### 1.1 适配器身份

| 项目 | 当前值 | 位置 |
|---|---|---|
| crate 目录 | `crates/adapters/onebot-snowluma` | workspace 成员 |
| package 名 | `acb-adapter-onebot-snowluma` | [Cargo.toml](../../crates/adapters/onebot-snowluma/Cargo.toml) |
| Rust 工厂类型 | `OneBotSnowLumaAdapterFactory` | [factory.rs](../../crates/adapters/onebot-snowluma/src/factory.rs#L76) |
| 出站类型 | `OneBotSnowLumaOutbound` | [outbound.rs](../../crates/adapters/onebot-snowluma/src/outbound.rs#L32) |
| manifest `id` | `acb.adapter.onebot11` | [factory.rs](../../crates/adapters/onebot-snowluma/src/factory.rs#L51) |
| manifest `kind` | `onebot11` | [factory.rs](../../crates/adapters/onebot-snowluma/src/factory.rs#L53) |
| manifest `platform` | `qq` | [factory.rs](../../crates/adapters/onebot-snowluma/src/factory.rs#L52) |
| manifest 标题 | `OneBot SnowLuma` | [factory.rs](../../crates/adapters/onebot-snowluma/src/factory.rs#L64) |
| 配置 `kind` | `onebot11`（规范值）/ `onebot-snowluma`（等价别名） | [config.example.toml](../../config.example.toml#L108) |
| 配置 `platform` | `qq` | [config.example.toml](../../config.example.toml#L107) |
| 连接 id | `qq` / `qq2` | [config.example.toml](../../config.example.toml#L106-L148) |
| transport | `ws-client` | [lib.rs](../../crates/adapters/onebot-snowluma/src/lib.rs) |
| 房间写法 | `<连接>:group/<群号>`、`<连接>:private/<号码>` | [lib.rs](../../crates/adapters/onebot-snowluma/src/lib.rs) |
| 依赖表里的平台 id | **连接 id**（`qq`），不是 adapter kind | [deps.rs](../../crates/kernel/src/deps.rs#L248-L252) |

关键事实：`/healthz` 和 `/api/dependencies` 里的 `platform` 条目 id 来自 `Connection.id`（[deps.rs](../../crates/kernel/src/deps.rs#L248-L252)），与适配器名无关。改名不会自动改这个 id；也不应该改。

### 1.2 全部落点

**代码与构建**

| 落点 | 说明 |
|---|---|
| `crates/adapters/onebot-snowluma/` | 8 个源文件：`catalog.rs`、`event.rs`、`factory.rs`、`forward.rs`、`lib.rs`、`link.rs`、`outbound.rs`、`segment.rs` |
| 根 `Cargo.toml:8` | workspace 成员路径 |
| `crates/bridge/Cargo.toml:17` | 依赖 `acb-adapter-onebot-snowluma` |
| `crates/bridge/src/main.rs:19,191` | `use acb_adapter_onebot_snowluma::OneBotSnowLumaAdapterFactory;`，并 `register_adapter` |
| `crates/kernel/src/adapter.rs:63-67` | manifest 文档示例直接写 `acb.adapter.onebot11` / `onebot11` |
| `crates/kernel/src/config.rs:293,302` | 配置字段注释写「内置 `onebot11`」和「OneBot 实现」 |
| `crates/bridge/src/console.html:756` | 新建连接表单默认值 `value="onebot11"` |
| `crates/bridge/src/config_patch.rs:317,800,1013` | 配置补丁示例与测试 fixture 写 `kind":"onebot11"` |
| `crates/bridge/src/console.rs:1299-1300` | 配置示例片段 |

**i18n**

- `crates/i18n/locales/zh-CN.json` 与 `en.json` 各有 49 个 `adapter.onebot11.*` key（zh-CN 在 L81-L129）。
- 文案同时承担显示与排障：连接成功/失败、重连、token、房间、目标校验、事件解析等。

**测试与 fixture**

- `kind = "onebot11"` 在 `crates/kernel` 的测试里出现 20+ 处，包括 `turn_flow.rs`、`platform_flow.rs`、`example_config.rs`、`endpoint_flow.rs`、`hot_reload_flow.rs`、`plugin_lkg.rs`、`deps.rs` 等。
- `platform_flow.rs:339-366` 有一个假 manifest `acb.adapter.fake-onebot`，`kind` 也写 `onebot11`。
- 适配器自身测试：`event.rs`、`segment.rs`、`outbound.rs`、`link.rs` 的单元测试；`crates/adapters/onebot11/src/*_test` 没有独立 integration 目录。

**文档与部署**

- `docs/05-adapters.md:32-36,50-58,206-304`：配置示例、连接方式、事件/元素映射、排障。
- `docs/03-architecture.md:128`：目录说明「adapters/onebot11 OneBot 11 适配器」。
- `docs/02-glossary.md:45`：适配器定义举例「OneBot 11、Satori」。
- `docs/guide/config.md:49-52`、`docs/guide/getting-started.md:38`：用户配置示例。
- `docs/adr/0012-platform-connections.md:15-27`：`kind` / `transport` 语义。
- `docs/adr/0026-plugin-host.md:38`：内置 provider 保留 namespace `acb`，例举 `acb.adapter.onebot11`。
- `docs/adr/0027-platform-adapter-registration.md:89-97`：Phase 2c 落地说明。
- `docs/plan/capability-gap-matrix.md:301-302`：`adapter_id` / `kind` 示例。
- `deploy/snowluma-compose.yml`：真实 SnowLuma 部署（容器内 3000/3001，映射 53000 等）。
- `README.md`、`docs/01-vision.md`、`HANDOFF.md`、`docs/guide/troubleshooting.md` 等有泛称。

### 1.3 改名波及清单

| 落点 | 现名 | 建议 | 风险 |
|---|---|---|---|
| crate 目录 / package 名 | `onebot11` / `acb-adapter-onebot11` | 可改 `onebot-snowluma` / `acb-adapter-onebot-snowluma` | 安全；构建期一次性 |
| Rust 类型名 | `OneBotAdapterFactory`、`OneBotOutbound` | 可改 `OneBotSnowLumaAdapterFactory`、`OneBotSnowLumaOutbound` | 安全；仅源码 |
| 注释、日志文案、i18n **value** | 「OneBot 11」 | 可改成「OneBot 11 · SnowLuma」 | 安全；不影响 key |
| i18n **key** | `adapter.onebot11.*` | 不建议改；若改必须同步两份 locale + 所有 `t!` 调用点 | 机械可做，但无收益，漏改会缺翻译 |
| manifest `metadata.title/summary` | `OneBot 11` | 可改「OneBot 11 · SnowLuma」 | 安全；只影响展示 |
| 文档措辞 | 泛称 OneBot | 可补 SnowLuma | 安全 |
| 测试函数名 / 注释 | OneBot | 可改 | 安全 |
| 配置 `kind` | `onebot11` | **保留**为规范值；新增 `onebot-snowluma` 别名 | 改则已部署配置报 `unknown_kind`，启动失败 |
| manifest `id` / provider id | `acb.adapter.onebot11` | **保留**；若要新 id，必须显式迁移 | LKG 状态文件 `state/plugins-lkg.toml` 以 id 为 key（[lkg.rs](../../crates/kernel/src/lkg.rs#L266-L274)）；改 id 会孤立回滚状态，`/api/plugins` 的消费者也会失配 |
| manifest `kind`（wire descriptor） | `onebot11` | **保留** | 外置/动态载体按 kind 匹配；改则跨载体断链 |
| 配置 `platform` | `qq` | **保留** | 平台名，不是适配器名；平台特有内容、显示、日志都用它 |
| 连接 id | `qq` / `qq2` | **保留** | 订阅、房间、已持久化记录、`/healthz` 平台条目都以它为准 |
| 房间前缀 | `qq:group/...` | **保留** | 订阅配置与流水里的房间串；改则旧记录不可回读 |
| transport | `ws-client` | **保留** | 配置值；改则旧配置不认 |
| 协议帧 | 无适配器名字段 | 无需改 | 不存在兼容问题 |

**结论**：真正会破坏兼容的只有三样：配置 `kind`、持久化 provider id、连接 id / 房间串。前两样可以靠「保留规范值 + 新增别名」解决；第三样根本不该动，因为它是账号身份，不是适配器身份。

### 1.4 当前实现的能力边界

- **连接**：正向 WebSocket（`ws-client`）与反向 WebSocket（`ws-server`，桥监听，SnowLuma 连入）；HTTP API、HTTP 事件上报与 `wss://` 未实现。
- **鉴权**：`ws-client` 用 `Authorization: Bearer`（地址里的 `?access_token=` 原样保留）；`ws-server` 接受 `?access_token=` 或 `Authorization: Bearer`，不匹配直接 401。
- **消息**：群/私聊文本、@、引用、图片、语音、视频、文件（单文件走 `upload_group_file` / `upload_private_file`）、自定义收藏表情（`msg.face`）、合并转发发送与读取。
- **平台方法**：`catalog.rs` 的 `ROUTES` 共 91 个层级 op，映射 94 个 action（`msg.send` / `msg.history` / `msg.poke` 按群/私聊分动作）。`platform_call` 只允许调用 catalog 里有的 op，所以**没列出的 SnowLuma 动作，agent 根本调不到**。
- **目录体积**：不带 `op` 列目录时，91 个 op 的原始清单约 18–19 KB；索引化后约 13 KB，仍超过 4 KiB 阈值，所以 `platform_call` 会自动只回一页（默认 20 个 op，约 3 KB，含 tags 概览），`_page` 给 `total` / `next_offset`，可用 `offset` 接着翻，或按 `path` / `tags` 收窄。
- **事件**：`message` / `message_sent` / `notice` / `request` / `meta_event` 有分流；`message_sent` 直接忽略（`reportSelfMessage = true` 时不重复记账）；`meta_event/heartbeat` 只刷新平台依赖条目的 `lastOkAt`，生命周期忽略；`bot_status` / `bot_offline` 走账号状态，不再写进私聊房间；`notice` 只对 `group_increase`、`group_decrease`、`group_ban`、`group_recall`、`friend_recall`、`group_admin` 与 `notify/poke` 有人话，其余降级为 `[类型]（未解析事件）` + Opaque 原载荷（[event.rs](../../crates/adapters/onebot-snowluma/src/event.rs#L55-L200)）。
- **账号**：连上后另起线程调 `get_login_info` 核对 `account`，并调 `get_version_info` / `get_status` 采集三层版本与在线状态（[link.rs](../../crates/adapters/onebot-snowluma/src/link.rs)）。
- **健康**：连接建立/断开决定 up/down；`get_status` 的 `online` / `good` 写进 `accountOnline` / `accountGood`；`bot_status` 的上下线按失败阈值迁移依赖状态，心跳只刷新 `lastOkAt`。版本信息写进 `version`。

---

## 2. SnowLuma 能力清单

### 2.1 资料性质说明

SnowLuma 文档站的 API 参考是**从运行时动作目录生成的 OpenAPI**（[开发者文档](https://snowluma.github.io/zh/docs/guide/developer) 页说明由 `collectActionDocs()` 产出 `catalog.json` 再生成页面）。这意味着：

- 每个动作页有参数、应答、示例，可信度较高；
- 但**页面计数不等于运行时能力**。中文页分类合计 200，英文页合计 219；同一分类计数也不一致（群管理 21 vs 24、空间 10 vs 26）。这只能当量级参考，不能当精确清单。
- 站点**没有独立的事件参考页**；事件形状主要靠 OneBot v11 兼容声明、SDK 事件助手和零散示例（例如 `bot_status`）。

### 2.2 按功能域

「明确」= 文档有专门页面或明确段落；「示例」= 有请求/响应示例；「可选」= 是否属于可选配置或扩展能力。

| # | 功能域 | 文档明确度 | 示例 | 可选性 | 关键内容 |
|---|---|---|---|---|---|
| 1 | HTTP API | 明确 | 有 curl | 配置可选（`httpServers`） | `POST /{action}`；默认 `0.0.0.0:3000`；Bearer 或 `?access_token=` |
| 2 | 正向 WebSocket | 明确 | 有 URL | 配置可选（`wsServers`） | 默认 `0.0.0.0:3001`；`role` 为 `Api` / `Event` / `Universal` |
| 3 | 反向 WebSocket | 明确 | 有 URL 示例 | 配置可选（`wsClients`） | SnowLuma 主动连机器人；`reconnectIntervalMs` 默认 5000，最小 1000 |
| 4 | HTTP 事件上报 | 明确 | 无请求示例 | 配置可选（`httpClients`） | 主动把事件 POST 到外部端点；`timeoutMs` 默认 5000；**鉴权/签名未成文** |
| 5 | Token 鉴权 | 明确 | 有 curl | 可选（默认自动生成） | 每个网络适配器一份 `accessToken`；HTTP 用 Bearer 或 query；WS 用 query |
| 6 | `messageFormat` | 明确 | 有默认值 | 可选 | `array`（默认）/ `string`；string 模式为字符串（OneBot 惯例是 CQ 码，文档未展开） |
| 7 | `reportSelfMessage` | 明确 | 无 | 可选，默认 `false` | 为 true 时上报 `message_sent` |
| 8 | `wss://` / TLS | 未成文 | 无 | 未知 | 文档只给 `ws://`；没有 TLS 配置说明 |
| 9 | 消息发送 | 明确 | 有 curl | 核心 | `send_group_msg`、`send_private_msg`、`send_msg`、`send_custom_face` |
| 10 | 消息读取/撤回 | 明确 | 有 curl | 核心 | `get_msg`、`delete_msg` |
| 11 | 消息历史 | 明确 | 有响应示例 | 扩展 | `get_group_msg_history`、`get_friend_msg_history`；支持锚点与方向 |
| 12 | 合并转发 | 明确 | 有页面 | 扩展 | `send_forward_msg`、`get_forward_msg`、`upload_forward_msg`、群/私聊专用转发 |
| 13 | 消息段 | 明确 | 有 SDK 示例 | 核心 | string / segment[] / object；SDK 列 18 种段；`message_id` 是有符号 32 位整数，负值有效 |
| 14 | 群管理 | 明确 | 有页面 | 核心/扩展 | 禁言、踢人、批量踢、管理员、名片、退群、群名、头像、头衔、全员禁言、加群选项、搜索、成员权限、邀请策略、历史可见性、标签、管理设置；匿名相关上游标注未实现 |
| 15 | 群信息/成员 | 明确 | 有页面 | 核心 | 群列表/信息/成员信息/成员列表/荣誉/系统消息/分享链接；扩展还有详情、禁言列表、打卡列表 |
| 16 | 好友 | 明确 | 有页面 | 核心/扩展 | 好友列表、陌生人信息、删好友；扩展有备注、分组、单向好友 |
| 17 | 请求处理 | 明确 | 有页面 | 核心/扩展 | 好友/加群请求；扩展有可疑申请、被忽略申请 |
| 18 | 群公告 | 明确 | 有页面 | 扩展 | `_get_group_notice`、`_send_group_notice`、`_del_group_notice`；发送支持置顶/弹窗/新成员等参数 |
| 19 | 群文件 | 明确 | 有页面 | 核心/扩展 | 根目录/子目录列表、下载链接、文件系统信息、上传（群/私聊）、删除；扩展有文件夹 CRUD、移动、重命名、分享链接、转存 |
| 20 | 流式传输 | 明确 | 有页面 | 扩展 | 6 个动作：上传、下载图片/语音/文件、测试下载、清理临时文件 |
| 21 | 表情回应 | 明确 | 有页面 | 核心/扩展 | `set_msg_emoji_like`、`get_msg_emoji_likes`、`fetch_emoji_like`、`set_group_reaction` |
| 22 | 自定义表情/系统表情 | 明确 | 有页面 | 扩展 | 添加/删除/获取/备注/置顶；系统表情查询、搜索、超级表情判断 |
| 23 | 语音与媒体 | 明确/部分 | 有页面 | 扩展 | `get_record`、`fetch_ptt_text`、`get_image`、`get_file`、`ocr_image`、AI 语音生成/发送、`musicSignUrl` |
| 24 | 账号与系统状态 | 明确 | 有 curl | 核心/扩展 | `get_login_info`、`get_status`、`get_version_info`、`can_send_image`、`can_send_record`、在线状态、资料、头像、在线客户端、退出 |
| 25 | 事件类型 | **部分** | 仅 `bot_status` 有 JSON | 核心 | 见 2.3；除 `bot_status` 外没有专门事件参考页 |
| 26 | 群相册 | 明确 | 有页面 | 扩展 | 9 个动作：列表、上传图片/视频、评论、点赞、删除 |
| 27 | QQ 空间 | 明确 | 有页面 | 扩展 | 说说列表/详情/发布/删除/评论/点赞/权限/黑名单 |
| 28 | 收藏/闪传/其他扩展 | 明确 | 有页面 | 扩展 | 收藏表情、收藏列表、闪传文件集、原始 SSO 包、数据库密钥等；部分标注未实现/占位 |
| 29 | WebUI 运维通知 | 明确 | 无 | 运维通道 | 邮件 / Webhook，和机器人协议是两条路，不在适配器范围 |
| 30 | MCP / TypeScript SDK | 明确 | 有示例 | 独立包 | MCP 提供 `list_actions` / `get_action` / `snowluma://onebot/actions`；SDK 提供类型化客户端 |

### 2.3 事件类型（文档能确认到什么程度）

| 事件 | 文档状态 | 说明 |
|---|---|---|
| `message`（group/private） | OneBot v11 兼容声明 + SDK `onGroupMessage` / `onPrivateMessage` 示例 | 没有字段级事件页；形状按 OneBot v11 |
| `message_sent` | 配置页明确 `reportSelfMessage` | 为 true 才上报；默认 false |
| `notice` | 只有 `bot_status` 有明确 JSON 示例 | `sub_type` 为 `online` / `offline`；被踢下线仍是 `bot_offline`；进程退出不发 `offline` |
| `request` | SDK `onRequest('friend')` 示例 + 两个处理动作页 | 好友/加群请求 |
| `meta_event` | SDK `onMetaEvent` | 心跳/生命周期；没有字段页 |
| 其他 notice（`group_upload`、`essence`、`notify.*` 等） | **未成文** | 只能按 OneBot v11 推断，需实机抓包确认 |

适配器现状（批次 2）：`bot_status` / `bot_offline` 走账号状态，`meta_event/heartbeat` 只刷新
平台依赖条目的 `lastOkAt`，`message_sent` 忽略。`group_upload` / `essence` / `notify.*`
仍按「未成文」处理：不猜字段，统一降级为 `[类型]（未解析事件）` + Opaque 原载荷。

### 2.4 连接与鉴权的实际形态

- **正向**：机器人连 SnowLuma 的 HTTP `:3000` 或 WS `:3001`；Docker 同网络用服务名 `snowluma`，宿主机用 `127.0.0.1`。
- **反向**：SnowLuma 的 `wsClients` 连机器人；适合 AstrBot / NoneBot2。
- **HTTP 上报**：SnowLuma 的 `httpClients` POST 事件到外部端点；和 WebSocket 事件是并列通道，不是 webhook 的替代品。
- **鉴权**：HTTP 与 WS 可能各有各的 token；`accessToken` 默认自动生成；WebUI 可改。
- **多账号**：一个 UIN 一份 `onebot_<uin>.json`；默认都抢 3000/3001，第二个号要换端口或用反向 WS。

---

## 3. 对照矩阵

状态定义：
- **已实现**：当前适配器可直接用，且有对应 action / 解析逻辑。
- **部分**：有主路径，但缺动作、缺字段、缺模式或事件处理不完整。
- **未实现**：当前适配器没有这条通道或这个域。
- **不适用**：不属于平台适配器职责（运维通道、独立包）。

| # | 能力域 | SnowLuma | 现有 onebot 适配器 | 缺口 |
|---|---|---|---|---|
| 1 | 正向 WS | `wsServers` | **已实现**，`ws-client` | 无 |
| 2 | HTTP API | `httpServers` | **未实现** | 没有 HTTP transport；出站只走 WS 队列 |
| 3 | 反向 WS | `wsClients` | **已实现** | `transport = "ws-server"`，桥监听 `options.listen`；握手校验 `?access_token=` 或 Bearer；一次服务一条活动连接 |
| 4 | HTTP 事件上报 | `httpClients` | **未实现** | 没有入站 HTTP 端点；也没有签名/令牌校验设计 |
| 5 | `wss://` | 未成文 | **未实现** | 只接受 `ws://`；TLS 依赖未引入 |
| 6 | Token 鉴权 | Bearer / query / 每适配器独立 | **已实现** | 单一 `Connection.token`；多 transport 时无法区分不同 token |
| 7 | `messageFormat` | array / string | **部分** | array 完整；string 被当纯文本，CQ 码不解析（[segment.rs](../../crates/adapters/onebot11/src/segment.rs#L58-L68,L258-L259)） |
| 8 | 消息收发 | 群/私聊发送、读取、撤回 | **已实现** | 群/私聊文本、图片、语音、视频、文件、自定义收藏表情（`msg.face`）都有；`send_msg` 不需要（桥按房间选动作） |
| 9 | 合并转发 | 发送、读取、上传 | **部分** | `send_forward_msg` / `get_forward_msg` 有；`upload_forward_msg` 与群/私聊专用别名未暴露（预上传返回 `res_id`，现有发送路径只接受节点数组，且它会是任意大小转发载荷的搬运通道） |
| 10 | 群管理 | 21（中文页）/ 24（英文页） | **部分** | 现有禁言/踢人/批量踢/管理员/名片/退群/群名/头像/头衔/全员禁言/成员权限/邀请策略/历史可见性/管理设置读取；缺加群选项、群搜索、群分类标签（参数取值语义未成文）；匿名相关上游本身未实现 |
| 11 | 群信息/成员 | 7 + 扩展 | **已实现** | 群列表/信息/成员信息/成员列表/荣誉/系统消息/分享链接/详情/禁言名单/打卡名单 |
| 12 | 好友 | 3 + 扩展 | **已实现** | 列表/陌生人/删好友/备注/分组/单向好友 |
| 13 | 请求处理 | 2 + 扩展 | **已实现** | 好友/加群请求处理、可疑好友申请列表与处理、被过滤加群请求 |
| 14 | 群公告 | 3 | **已实现** | 获取/发送/删除 |
| 15 | 群文件 | 12 | **部分** | 根/子目录列表、下载链接、系统信息、群/私聊上传、群删除、文件夹创建/删除、文件移动/重命名、私聊下载链接有；缺文件夹重命名（两个名称参数优先级未成文）、群文件分享链接（`get_share_link` 只收 `fileset_id`）、转存（`trans_group_file` 没有目标参数） |
| 16 | 流式传输 | 6 | **未实现** | 没有 HTTP 流式通道；现有文件中继（ADR-0025）不是流式 |
| 17 | 表情回应 | 4 + 自定义 7 + 系统 4 | **部分** | `set_msg_emoji_like` / `set_group_reaction`、回应汇总、回应用户列表、收藏表情列表/详情/改/删、系统表情列表/搜索/单查有；缺收藏表情新增（`file` 格式未成文）、置顶、`get_emoji_likes`（与 `fetch_emoji_like` 重叠） |
| 18 | 语音/媒体 | `get_record`、`fetch_ptt_text`、`get_image`、`get_file`、OCR、AI 语音 | **部分** | `get_record` / `get_image` / `get_file`、语音转写（`msg.voice_text`）、OCR（`msg.ocr`）、AI 语音角色/生成/发送有；缺原始流式上传下载、`download_file`（任意 URL/base64，无大小上限）、音乐签名（`musicSignUrl` 是配置项不是动作） |
| 19 | 账号/系统状态 | login / status / version / 能力查询 / 资料设置 | **部分** | 启动已调用 `get_login_info` / `get_status` / `get_version_info`；三层版本 + 兼容下限进依赖元数据。仍缺 `can_send_*`、资料设置、在线状态设置 |
| 20 | 事件类型 | message / message_sent / notice / request / meta | **部分** | 有分流；notice 只解析 6 类 + poke；`bot_status` / `bot_offline` 已走账号状态并驱动依赖健康，心跳只刷新 `lastOkAt`，`message_sent` 已忽略。仍缺 `group_upload`、`essence`、`notify.*` 的字段解析（SnowLuma 未成文），未知 notice 保留 Opaque |
| 21 | 群相册 | 9 | **部分** | 相册列表/相册媒体列表（只读）有；创建/删除相册动作文档里不存在，上传/删除/评论/点赞按产品范围不暴露 |
| 22 | QQ 空间 | 10（中文）/ 26（英文） | **未实现** | 产品决定不暴露：账号级社交面，权限/审计成本高于聊天核心收益 |
| 23 | 收藏/闪传/其他扩展 | 115 个 extended 里的大部分 | **未实现** | 产品决定不暴露：收藏/闪传/SSO/数据库密钥等账号级或敏感面；`send_packet`、`request_decrypt_key`、`get_credentials` 永不暴露 |
| 24 | WebUI 邮件/Webhook | 运维通道 | **不适用** | 不在适配器职责 |
| 25 | MCP / TypeScript SDK | 独立包 | **不适用** | 不是 OneBot 运行时协议 |

**计数**：25 项中 2 项不适用；适用 23 项 = 已实现 8 + 部分 9 + 未实现 6。
批次 1 完成后计数不变：第 19 项仍是「部分」，但缺口从「启动不调用版本/状态」缩小为
「缺能力查询、资料设置、在线状态设置」。批次 2 完成后计数仍不变：第 20 项仍是「部分」，
但缺口从「`bot_status` 污染私聊房间、心跳不更新健康」缩小为「`group_upload` / `essence` /
`notify.*` 的字段未成文，只能显式降级」。批次 3 完成后：#8 补上自定义收藏表情，
#11 群信息 / #12 好友 / #13 请求 / #14 公告从「部分」升为「已实现」；
#10 群管理仍是「部分」，缺口缩为加群选项、群搜索、群分类标签。批次 4 完成后计数不变：
#15 群文件 / #17 表情回应仍是「部分」，缺口分别缩为文件夹重命名/分享链接/转存与
收藏表情新增/置顶。批次 5 完成后：#3 反向 WS 从「未实现」升为「已实现」，
计数变为 **8 完全 / 8 部分 / 7 未实现**；HTTP API、HTTP 事件上报、`wss://` 仍未实现。
批次 6 完成后计数不变：#18 语音/媒体仍是「部分」，缺口缩为原始流式上传下载、
`download_file` 与音乐签名；#16 流式传输仍未实现。批次 7 完成后：#21 群相册从
「未实现」升为「部分」，计数变为 **8 完全 / 9 部分 / 6 未实现**；#22 QQ 空间与
#23 收藏/闪传按产品范围保持「未实现」。

**动作级覆盖**：
- 现有 `ROUTES` 91 个层级 op / 94 个 action（[catalog.rs](../../crates/adapters/onebot-snowluma/src/catalog.rs)）。
- SnowLuma 中文 API 页分类合计 200，英文 219；两页不一致，不能当精确总数。
- 由于 `platform_call` 只认 catalog（[engine.rs](../../crates/kernel/src/engine.rs#L3820-L3849)），未列入的动作不是「文档没写」，而是**agent 实际调不到**。这是「完整实现 SnowLuma 能力」的主要工作量。

**曾经最容易造成实际事故的三个缺口，已由批次 1 / 2 收口**：
1. `bot_status` 不再写成 `private/<self_id>`，改走账号状态（批次 2）。
2. 账号上下线进入平台依赖健康，`/healthz` 能看到 `accountOnline` / `accountGood`（批次 1 / 2）。
3. `get_version_info` 已采集三层版本并写进依赖元数据（批次 1）。

现在的主要风险转为动作覆盖面：未列入 catalog 的 SnowLuma 动作 agent 调不到，见批次 3 / 4 / 7。

---

## 4. 版本信息

### 4.1 三层，不要压成一个字段

| 层 | 含义 | 来源 | 拿不到时 | 用途 |
|---|---|---|---|---|
| **协议版本** | OneBot 11 / 12，决定消息语义与字段形状 | `get_version_info.protocol_version` | `unknown` | 决定解析器分支；低于/高于支持范围时告警 |
| **实现版本** | 对面跑的实现：SnowLuma / NapCat / Lagrange / go-cqhttp | `get_version_info.app_name` + `app_version` | `unknown` | 排障第一问；决定实现特有动作是否可用 |
| **适配器自身版本** | 本仓库 crate 版本 | `CARGO_PKG_VERSION`（workspace 当前 `0.1.0`） | 不会缺 | 说明「我们支持哪个协议范围、跟哪个实现版本对齐」 |

三者要分别记录，不能只留一个 `version` 字符串；否则「OneBot 11 + SnowLuma 2.4.0 + ACB 0.1.0」会被压成无法解释的值。

### 4.2 现状核实

**`get_version_info` 已在批次 1 调用。**

- `catalog.rs` 仍有 `route("account.version", "get_version_info")`，agent 也可手动调。
- `link.rs` 的连接后探测（`probe_identity`）现在会调 `get_login_info`、`get_version_info`、`get_status`。
- 结论：启动路径已采集协议版本与实现版本；拿不到记 `unknown`，不回退猜测。

**`get_status` 已在批次 1 调用。**

- `catalog.rs` 仍有 `route("account.status", "get_status")`。
- 连接健康仍来自 WS 建立/断开；`get_status` 的 `online` / `good` 额外写进平台依赖条目的 `accountOnline` / `accountGood`。

**适配器自身版本已暴露。**

- `PlatformVersionInfo.adapter_version` 由适配器用 `CARGO_PKG_VERSION` 填入，进入平台依赖条目的 `version.adapter.version`。
- `AdapterManifest` 仍不新增 crate 版本字段：依赖元数据是运行期事实，manifest 是静态声明，二者不混。

### 4.3 SnowLuma 侧两个方向

**A. 声明式兼容下限**

- 上游没有版本兼容矩阵；文档明确说 SnowLuma 仍处早期开发，接口、配置结构、发行布局都可能变化，生产环境要固定具体 tag（[介绍](https://snowluma.github.io/zh/docs/guide/introduction)、[更新](https://snowluma.github.io/zh/docs/guide/deploy/update)）。
- 可观测的最新 release：`v2.4.0`（2026-11-03，GitHub Releases）。
- 已落地的声明方式：
  - 适配器默认 `snowluma_min_version = "2.4.0"`（调查基线），并说明「上游无兼容承诺，升级前看 release notes」；
  - `connections[].options.snowluma_min_version` 可以覆盖（`options` 是适配器自有 JSON，core 不解释，[config.rs](../../crates/kernel/src/config.rs#L300-L301)）；
  - 实际版本低于声明下限时：启动日志警告，依赖元数据 `version.adapter.compat.state = below_declared_floor`，**不断连**。
    版本字符串解析不了（如带预发布后缀）记 `unknown`，不猜大小。
- 不要写死「兼容 2.x 全系」；上游自己都不承诺。

**B. 运行时实际版本**

- 有接口：`get_version_info` 返回 `app_name` / `app_version` / `protocol_version`，有 JSON 示例（[get_version_info](https://snowluma.github.io/zh/docs/api/system/get_version_info)）。
- 连接成功后自动调一次；失败、超时、返回空字符串都记 `unknown`，不回退到镜像 tag、包名或目录名猜测。
- `app_name` 只用于展示/分类；**不要**用 `app_name == "snowluma"` 来 gate 核心行为，大小写和实际取值未成文。
- `protocol_version` 只做宽松校验：能识别 `11` / `v11` 就按 OneBot 11；识别不了记 `unknown` 并保持现有 v11 解析器，不切换到 OneBot 12 语义。

### 4.4 版本/能力查询接口的核实结果

| 问题 | 结论 |
|---|---|
| SnowLuma 文档站是否提供版本查询接口 | **有**：`get_version_info` 动作页，返回 `app_name` / `app_version` / `protocol_version`；另有 `get_status` 返回 `online` / `good` / `time` |
| 是否提供能力查询接口 | **运行时没有**。OneBot 动作目录里没有 `get_capabilities` / `list_actions` 之类的动作。文档站的 API 目录是构建期生成的页面；`@snowluma/mcp` 有 `list_actions` / `get_action` 和 `snowluma://onebot/actions` 资源，但那是独立 MCP 包的目录快照，不是运行时查询 |
| 是否提供版本兼容矩阵 | **没有**。只有「早期开发、固定 tag、升级看 release notes」的警告；GitHub Releases 是版本来源 |
| OneBot `get_version_info` 在当前适配器是否已调用 | **已调用**（批次 1，2026-11-05）：`probe_identity` 在连接后调用 `get_version_info` / `get_status`，结果写进依赖元数据；拿不到记 `unknown` |

### 4.5 记录位置（设计）

建议在平台依赖条目上增加一个结构化元数据，而不是新增顶层 `version`：

```json
{
  "kind": "platform",
  "id": "qq",
  "state": "up",
  "versionObservedAt": 1760000000000,
  "version": {
    "protocol": {
      "kind": "onebot11",
      "name": "OneBot",
      "version": "v11",
      "source": "get_version_info"
    },
    "impl": {
      "name": "snowluma",
      "version": "2.4.0",
      "source": "get_version_info"
    },
    "adapter": {
      "version": "0.1.0",
      "source": "crate",
      "compat": {
        "protocol": "onebot11",
        "snowlumaMin": "2.4.0",
        "state": "ok"
      }
    }
  }
}
```

- `source` 取值至少包括 `get_version_info`、`crate`、`unknown`。
- `compat.state` 取值：`ok` / `below_declared_floor` / `unknown`；低于下限只告警，不断连。
- `unknown` 是合法结果，不是错误；UI 要显示「未知」，不能显示空或猜测值。
- 日志只打一行摘要（三层各一段），不打 header、token、完整响应。

---

## 5. 命名提案

约束：名字要同时涵盖 OneBot 与 SnowLuma；改名不能悄悄破坏现有配置与已持久化的 id。

### 候选 A：`onebot`

- crate：`acb-adapter-onebot`
- 类型：`OneBotAdapterFactory`
- 显示：`OneBot 11（SnowLuma / NapCat / Lagrange）`
- 配置 `kind`：保留 `onebot11`
- provider id：保留 `acb.adapter.onebot11`

**配置键**：不需要改；`kind = "onebot11"` 就是规范值。
**别名**：不需要保留额外别名。
**依赖 id**：不迁移。
**代价**：最低。只改 crate/类型/显示，不动配置和持久化。
**问题**：名字没有出现 SnowLuma；用户要求「同时涵盖」时，只能靠副标题补，不够直接。

### 候选 B：`onebot-snowluma`（推荐）

- crate：`acb-adapter-onebot-snowluma`
- 类型：`OneBotSnowLumaAdapterFactory`、`OneBotSnowLumaOutbound`
- 显示：`OneBot 11 · SnowLuma`
- 配置 `kind`：**规范值仍为 `onebot11`**；新增 `onebot-snowluma` 作为别名
- provider id：**保留 `acb.adapter.onebot11`**；如果未来要新 id，必须显式迁移 LKG，不做静默切换

**配置键**：规范值仍是 `onebot11`；新增 `onebot-snowluma` 作为等价别名。
**别名**：需要保留 `onebot11`（旧配置）并接受 `onebot-snowluma`（新写法）。
**依赖 id**：不迁移；继续用 `acb.adapter.onebot11`。
**代价**：中等。crate 目录、workspace 成员、bridge 依赖、Rust import、类型名、文档要一起改。
**优点**：名字直接回答「这是什么协议 + 主要面向哪个实现」；同时保留 `onebot11` 兼容层，旧配置、旧 LKG、旧房间串都不动。
**问题**：名字偏实现，可能让人误以为只支持 SnowLuma。文档必须写明「协议仍是 OneBot 11，NapCat / Lagrange 的通用路径继续可用」。

### 候选 C：`snowluma`

- crate：`acb-adapter-snowluma`
- 配置 `kind`：`snowluma`
- provider id：`acb.adapter.snowluma`
- 显示：`SnowLuma`

**配置键**：改成 `snowluma`。
**别名**：必须保留 `onebot11` 兼容；否则所有旧配置启动即失败。
**依赖 id**：必须显式迁移，或同时注册新旧两个 id；不能静默换。
**代价**：最高。所有现有 `kind = "onebot11"` 配置必须迁移；`state/plugins-lkg.toml` 里的 provider id 会失配；`/api/plugins` 消费者可能按 id 匹配；等于把「OneBot 11 通用适配器」降格成「SnowLuma 专用适配器」。
**适用场景**：只有决定放弃 NapCat / Lagrange 通用支持时才值得。

### 推荐

**选 B：`onebot-snowluma`。**

理由：
1. 名字同时涵盖协议与实现，满足命名目标。
2. 兼容性代价可控：`kind` 保留 `onebot11` 并加别名，provider id 不动，连接 id / 房间串不动。
3. 比 C 安全：不需要迁移已持久化的 LKG 状态，不会让旧配置启动失败。
4. 比 A 清晰：用户和排障日志能直接看到 SnowLuma。

落地时的兼容规则：
- 配置：`kind = "onebot11"` 继续有效；`kind = "onebot-snowluma"` 解析到同一个工厂。二者都接受，文档把 `onebot11` 标为「兼容写法」。
- provider id：继续用 `acb.adapter.onebot11`；manifest 显示名改为 `OneBot SnowLuma`。
- 连接 id / 房间前缀：完全不动。
- i18n key：继续用 `adapter.onebot11.*`；只改文案 value。改 key 没有用户收益，反而容易漏翻译。
- 迁移：不写自动迁移脚本。旧配置不需要迁移；新配置可以用别名。
- 文档：在 `docs/05-adapters.md` 和 `docs/guide/config.md` 写清「`onebot11` 与 `onebot-snowluma` 等价」。

---

## 6. 实现拆分

原则：每批能独立交付、独立验证；批与批之间尽量不共享未完成的抽象。批次 0 只是改名，不改行为；批次 1 之后才开始加能力。

### 批次 0：改名与别名（已完成，2026-11-05）

**能力范围**
- crate / 类型 / 显示名改为 `onebot-snowluma`。
- 配置 `kind` 接受 `onebot11` 与 `onebot-snowluma`，都落到同一个工厂。
- provider id、连接 id、房间串、transport 值全部不变。

**涉及文件**
- `Cargo.toml`、`crates/bridge/Cargo.toml`、`crates/bridge/src/main.rs`
- `crates/adapters/onebot-snowluma/` 目录与 `factory.rs`、`lib.rs`、`outbound.rs`
- `crates/kernel/src/adapter.rs`、`config.rs`、`plugin.rs`（别名解析与文档示例）
- `crates/i18n/locales/{zh-CN,en}.json`（只改 value）
- `docs/05-adapters.md`、`docs/guide/config.md`、`docs/03-architecture.md`、`docs/02-glossary.md`

**验证方式**
- `cargo test --workspace` 全绿。
- 用旧配置（`kind = "onebot11"`）启动，连接正常，`/healthz` 平台 id 仍是 `qq`。
- 用新配置（`kind = "onebot-snowluma"`）启动，落到同一个 manifest。
- `state/plugins-lkg.toml` 前后不变；`/api/plugins` 里 provider id 不变。

**前置**：无。

### 批次 1：版本与状态采集（已完成，2026-11-05）

**能力范围**
- 连接成功后自动调 `get_version_info` 与 `get_status`。
- 记录协议版本、实现名/版本、适配器 crate 版本；拿不到记 `unknown`。
- 在平台依赖条目元数据里暴露三层版本；`/healthz` / `/api/plugins` 可见。
- 记录声明式兼容下限 `snowluma_min`；低于下限只告警，不断连。
- 不改消息解析、不改重试、不改会话。

**涉及文件**
- `crates/adapters/onebot11/src/link.rs`（连接后探测）
- `crates/adapters/onebot11/src/factory.rs`（manifest 版本/兼容元数据）
- `crates/kernel/src/adapter.rs`（manifest 字段或元数据载体）
- `crates/kernel/src/deps.rs`（平台条目元数据）
- `crates/bridge/src/console.rs`、`console.html`
- `crates/i18n/locales/{zh-CN,en}.json`
- `docs/05-adapters.md`、`docs/guide/config.md`

**验证方式**
- 单元测试：正常响应、缺字段、超时、`retcode != 0`、非法 `protocol_version`；都落到 `unknown` 而不是 panic。
- 集成测试：假 WS 服务端返回 `get_version_info` / `get_status`，断言 `/healthz` 三层字段。
- 断言日志不含 token / Authorization header。
- 旧配置启动不受影响。

**前置**：批次 0 的展示名可选；功能上不依赖改名。

### 批次 2：事件通道与平台健康（已完成，2026-11-05）

**能力范围**
- `bot_status`（`online` / `offline` / `bot_offline`）不再写进私聊房间；映射到平台依赖条目的 `up/down`。
- `meta_event` 心跳/生命周期用于健康探测，不写流水。
- `group_upload`、`essence`、`notify.*` 等补可读摘要或明确降级；未知 notice 继续保留 Opaque。
- `reportSelfMessage = true` 时不重复记账（现有 `message_sent` 忽略逻辑要覆盖）。
- 恢复时发一条通知（沿用 connectivity-notices 的故障窗口设计，不在本批改协议）。

**涉及文件**
- `crates/adapters/onebot-snowluma/src/event.rs`
- `crates/adapters/onebot-snowluma/src/link.rs`（状态上报）
- `crates/kernel/src/adapter.rs`、`crates/kernel/src/deps.rs`
- 测试：`crates/kernel/src/deps.rs`

**验证方式**
- fixture 覆盖 `bot_status` 三种状态：不产生房间记录；依赖条目从 `up` 到 `down` 再恢复。
- 心跳连续 10 条不写流水。
- `message_sent` 与桥自己的发送记录不重复。
- 未知 notice 仍带 Opaque 原载荷。

**实际落地**
- `bot_status` / `bot_offline` → `Parsed::AccountStatus` → `AdapterStatus::account`；不产生房间记录。
  离线按现有失败阈值迁移（不因单条事件立刻打断），上线立即 `up`，并发出
  `NoticeEvent::DependencyEdge`，由既有 notifier 消费，不新增协议。
- `meta_event/heartbeat` → `Parsed::Heartbeat` → `AdapterStatus::heartbeat` →
  `DependencyRegistry::platform_heartbeat`；只在 `state = Up` 时刷新 `lastOkAt`，
  不会把已 `down` 的条目拉回 `up`。`lifecycle` 忽略。心跳字段未成文，不解析
  `online` / `good`。
- `message_sent` 继续忽略，测试覆盖 `reportSelfMessage = true` 的重复风险。
- `group_upload` / `essence` / `notify.*` 的字段在 SnowLuma 文档中未成文，按「记缺口」
  处理：不猜字段、不加摘要，保留 `[类型]（未解析事件）` + Opaque 的显式降级。
- **新增 op：0**。本批全部是事件分流与健康上报，不需要新的平台动作；`account.status` /
  `account.version` 已在批次 1 之前的 catalog 中。没有新增 `PlatformTag`，没有第二张
  op→risk 表。

**前置**：批次 1（元数据字段存在后更好复用）。

### 批次 3：目录扩展——消息 / 群管 / 成员 / 好友 / 请求 / 公告（已完成，2026-11-05）

**能力范围**
- 消息：`send_custom_face`；合并转发补 `upload_forward_msg` 与专用别名（如果决定暴露）。
- 群管理：批量踢、加群选项、群搜索、成员权限、邀请策略、历史可见性、标签、管理设置。
- 群信息：分享链接、详情扩展、禁言列表、打卡列表。
- 好友：备注、分组、单向好友。
- 请求：可疑申请、被忽略申请。
- 公告：删除。
- 每个方法都要有 `MethodSpec`、`TargetSpec`、risk / effect / record，不能只加一条路由。

**涉及文件**
- `crates/adapters/onebot-snowluma/src/catalog.rs`
- `crates/adapters/onebot-snowluma/src/outbound.rs`（目标参数、特殊 action 与参数校验）
- `crates/i18n/locales/{zh-CN,en}.json`
- 测试：`catalog` 快照、参数校验、目标校验

**验证方式**
- `cargo test -p acb-adapter-onebot-snowluma` 与 `cargo test --workspace`。
- 目录快照断言方法存在、参数声明正确。
- 群/私聊目标错配仍被 core 拒绝。
- 破坏性方法默认 `allow_destructive = false` 仍然生效。

**实际落地**
- **新增 18 个 op（50 → 68），映射 18 个新 action（53 → 71）**：
  - 消息：`msg.face` → `send_custom_face`（Write，按房间补 `group_id` / `user_id`，写入流水）。
  - 群管理：`member.kick_many` → `set_group_kick_members`；`group.member_permissions` →
    `set_group_member_permissions`；`group.invite_policy` → `set_group_member_invite_policy`；
    `group.history_visibility` → `set_group_new_member_history_visibility`；
    `group.admin_settings` → `get_group_admin_settings`（Read）。
  - 群信息：`group.share_link` / `group.detail` / `group.shut_list` / `group.signed_list`。
  - 好友：`friend.remark`、`friend.categories`、`friend.set_category`、`friend.one_way`、
    `friend.doubt_list`、`friend.doubt_approve`（连接级）。
  - 请求：`group.ignored_requests` → `get_group_ignored_notifies`（连接级）。
  - 公告：`group.notice.delete` → `_del_group_notice`（Destructive，`effect = Delete`）。
- **risk 显式**：所有群管理写操作标 `Risk::Destructive`，继续受 `allow_destructive` 约束；
  读取类标 `Risk::Read`；好友备注/分组/可疑申请处理标 `Risk::Write`。`requires_admin`
  只是标签，core 不判断群内角色，权限不足由平台拒绝。
- **参数校验分两层**：core 校验必填、类型和枚举；适配器在 `validate_call_params` 补
  数组长度与二选一约束：
  - `msg.face`：`emoji_id` 非空且不超过 256 字符（仅收平台 id，不收文件内容）。
  - `member.kick_many`：`user_ids` 非空、最多 20 个整数。
  - `group.notice.delete`：`fid` / `notice_id` 至少一个非空。
  - `friend.set_category`：`categoryId` / 非空 `categoryName` 至少一个。
- **按文档判断不做（记缺口）**：
  - `upload_forward_msg`（含 `upload_foward_msg` 拼写别名）与群/私聊专用转发别名：现有
    `msg.send_forward` 已覆盖发送；预上传返回 `res_id`，现有发送路径不接受它，且它会把
    任意大小的转发节点载荷搬进 core，属媒体/附件类通道，先不暴露。
  - `set_group_add_option`：`add_type` 的取值语义文档未列，不猜。
  - `set_group_search`：`no_finger_open` / `no_code_finger_open` 是未文档化的整数开关，
    不猜 0/1 以外的取值。
  - `add_group_tag` / `delete_group_tag` / `get_group_tags`：参数是 `group_class`（群分类），
    不是群号，映射关系未成文，不暴露。
  - `get_group_ignore_add_request`：与 `get_group_ignored_notifies` 语义重叠的 NapCat 兼容
    字段，只保留后者，避免两个近似 op。
- **没有新增 `PlatformTag`，没有第二张 op→risk 表**；全部走 `catalog()` + `ROUTES`。

**前置**：无；可与批次 2 并行。

### 批次 4：文件 CRUD 与表情扩展（已完成，2026-11-05）

**能力范围**
- 群文件：文件夹创建/删除/重命名、文件移动/重命名、私聊下载链接、分享链接、转存。
- 表情：`get_msg_emoji_likes`、`fetch_emoji_like`、`set_group_reaction`；自定义表情增删查改；系统表情查询/搜索。
- 仍走 WS；不引入流式。

**涉及文件**
- `crates/adapters/onebot-snowluma/src/catalog.rs`
- `crates/adapters/onebot-snowluma/src/outbound.rs`
- `crates/kernel/src/engine.rs`、`crates/kernel/src/tools.rs`（目录分页）
- `crates/i18n/locales/{zh-CN,en}.json`
- `harnesses/shared/tools.mjs`
- 测试与文档

**验证方式**
- 目录快照 + 假平台回包。
- 文件上传仍受 45 秒超时与大小限制；失败不重试。
- 表情回应发送走 `set_msg_emoji_like` 的现有路径，新读取方法只读。

**实际落地**
- **新增 15 个 op（68 → 83），映射 15 个新 action（71 → 86）**：
  - 群文件：`group.file.folder_create` → `create_group_file_folder`；`group.file.folder_delete` →
    `delete_group_file_folder`（Destructive，`Effect::Delete`）；`group.file.move` →
    `move_group_file`；`group.file.rename` → `rename_group_file`；
    `file.private_url` → `get_private_file_url`（连接级，要求显式 `user_id`）。
  - 表情回应：`msg.reactions` → `get_msg_emoji_likes`；`msg.reaction_users` →
    `fetch_emoji_like`；`group.reaction` → `set_group_reaction`（按房间补 `group_id`）。
  - 收藏/系统表情：`face.custom_list`、`face.custom_detail`、`face.custom_delete`
    （Destructive，`Effect::Delete`）、`face.custom_modify`、`face.system_list`、
    `face.system_search`、`face.entity`。
- **状态前提写进描述**：`group.file.folder_delete`、`group.file.move`、
  `group.file.rename`、`face.custom_delete`、`msg.reaction_users`、`group.reaction`
  都写明「id/路径/回应必须已存在」。平台返回非 0 retcode 时由既有
  `parse_call_response` 分类成 `bad_params` / `forbidden` / `not_implemented`，
  只有 `retcode = 0/1` 算成功，不会静默成功。
- **删除语义**：`group.file.folder_delete` 与 `face.custom_delete` 都进
  `DELETE_METHODS`，`Effect::Delete`；审计能看到删的是文件/表情，而不是只看到调了 op。
- **参数校验**（core 校验必填/类型/枚举，适配器补空值与范围）：
  - 文件夹名、文件夹 id、文件 id、源/目标目录、新文件名不能为空。
  - `file.private_url` 的 `user_id` 必须是正整数，避免猜文件属于谁。
  - `msg.reaction_users` / `group.reaction` 的消息 id、`emojiId` / `code` 不能为空。
  - `face.system_search` 的 `query` 不能为空；`face.entity` 的 `face_id` 必须 >= 0。
- **目录体积实测与分页**：83 个 op 的未分页清单原始约 17,329 B，索引化后约
  11,975 B（都超过 `LIST_INDEX_BYTES = 4096`）。因此 `platform_call` 在列目录时
  自动只回一页：默认 20 个 op，约 2.9 KB 的 ops 主体（另加 tags 概览与信封），
  `_page` 给 `total` / `offset` / `returned` / `has_more` / `next_offset`；
  列目录也可显式给 `limit` / `offset`（上限 60 条/页），或按 `path` / `tags` 收窄。
- **按文档判断不做（记缺口）**：
  - `rename_group_file_folder`：`new_folder_name` 与 `name` 两个名称参数都存在且默认空，
    优先级未成文，不猜。
  - `get_share_link`：只接受 `fileset_id`（闪传文件集），不接受群 `file_id`，不属于群文件域。
  - `trans_group_file`：没有任何目标参数，文档没说转存到哪里，不暴露。
  - `add_custom_face`：`file` 的格式（URL / 本地路径 / base64）未成文，且属附件上传通道，
    先不暴露，避免 core 变成替模型搬文件的通道。
  - `get_emoji_likes`：与 `fetch_emoji_like` 重叠，只保留后者（支持 count/cookie 翻页）。
- **没有新增 `PlatformTag`，没有第二张 op→risk 表**；全部走 `catalog()` + `ROUTES`。

**前置**：批次 3 的 catalog 结构可复用。

### 批次 5：连接形态与鉴权（已完成，2026-11-05）

**能力范围**
- HTTP API transport（`httpServers`）。
- 反向 WS（`wsClients`，桥当服务端）。
- `wss://`（如果确认需要）。
- HTTP 事件上报（`httpClients`，桥当接收端）。
- 多 transport 的 token 模型：HTTP header、WS query、每 transport 独立 token。
- 明确哪些形态是「必须」：现有部署是正向 WS；如果只做 SnowLuma 完整能力，HTTP + 正向 WS 至少要有一个，反向 WS 和 HTTP 上报可以按需。

**涉及文件**
- `crates/adapters/onebot-snowluma/src/link.rs`（拆分 transport）
- `crates/adapters/onebot-snowluma/src/factory.rs`（manifest `transports`）
- `crates/i18n/locales/{zh-CN,en}.json`
- `config.example.toml`、`docs/guide/config.md`、`docs/05-adapters.md`
- 测试：`link.rs` 的监听地址与 token 单元测试

**验证方式**
- 正向 WS、HTTP、反向 WS 各自最小集成测试。
- token 矩阵：无 token、正确 Bearer、错误 Bearer、query token。
- HTTP 事件上报：接收、去重、鉴权失败拒绝。
- 断线重连不重复投递。

**实际落地**
- **反向 WS（`ws-server`）**：
  - `factory.rs` 的 manifest `transports` 增加 `ws-server`；`create` 只在
    `ws-server` 缺监听地址时拒绝，不再一律要求 `url`。
  - `link.rs` 新增 `supervise_server`：绑定 `options.listen`（或 `url = "ws://host:port"`），
    非阻塞 accept，握手完成即 `status.connected()` 并复用原有 `drive_ws`（安装发送槽、
    探测身份、跑 pump）。一次只服务一条活动连接；客户端断开后继续等待下一条。
  - 握手鉴权：配了 `token` 时接受 `?access_token=`（含 `%XX` 解码）或
    `Authorization: Bearer`；不匹配返回 401，日志不打印 token。
  - 监听/accept 失败是配置或环境问题：标 `failed` 并退出，不在原地无限重试。
- **没有新增 op、没有新增工具位**：工具数保持 28，`platform_call` 仍是唯一平台入口；
  本批只改连接方式，不改事件/消息解析。目录仍是 83 op，默认 20 条/页一页约 3–4 KB。
- **按文档/实施成本判断不做（记缺口）**：
  - HTTP API transport（`httpServers`）：需要新增 HTTP 客户端依赖与独立动作通道；
    当前没有部署只用 HTTP，先不做。
  - HTTP 事件上报接收端（`httpClients`）：需要入站 HTTP 服务、去重与鉴权设计，
    文档也未给出签名/令牌细节，先不做。
  - `wss://`：需要 TLS 依赖；现有部署用同机/内网 `ws://`，没确认前不声明支持。
  - 多 transport 独立 token：目前只落了两条 WS 方向的 token 校验，没有 HTTP transport，
    拆 token 字段没有实际收益。

**前置**：这是「必须先解决鉴权/连接形态」的批次本身。批次 6 依赖它；如果部署只用 HTTP，批次 3/4 也要等它。

### 批次 6：流式传输与媒体增强（媒体增强已完成，流式按判断不做）

**能力范围**
- 6 个流式动作：上传、下载图片/语音/文件、测试下载、清理临时文件。
- `fetch_ptt_text`（语音转文字）、`ocr_image`、AI 语音、`get_file`、`download_file`、`musicSignUrl`。
- 流式响应的取消、超时、临时文件清理。

**涉及文件**
- `crates/adapters/onebot-snowluma/src/outbound.rs`、`crates/adapters/onebot-snowluma/src/catalog.rs`
- `crates/i18n/locales/{zh-CN,en}.json`
- `docs/05-adapters.md`、`docs/guide/troubleshooting.md`

**验证方式**
- 真机 SnowLuma：上传/下载各一次，校验大小、校验取消、校验临时文件清理。
- 大文件不整段进内存；超时后不重试。
- 语音转写/OCR 失败时错误信息可读。

**实际落地**
- **新增 6 个 op（83 → 89），映射 6 个新 action（86 → 92）**：
  - `msg.voice_text` → `fetch_ptt_text`（Read）：语音消息转文字，前提「message_id 必须是一条语音消息」。
  - `msg.ocr` → `ocr_image`（Read）：图片 URL 或缓存 file_id，前提「识别不了时平台报错」。
  - `file.info` → `get_file`（Read，连接级）：缓存图片/语音的路径、地址、大小；`file_id` / `file` 至少一个。
  - `group.ai_characters` → `get_ai_characters`（Read）：群可用 AI 语音角色。
  - `group.ai_voice` → `get_ai_record`（Write）：只生成语音，不发送。
  - `group.ai_voice_send` → `send_group_ai_record`（Write，`Effect::Send`，`RecordMode::Message`）：
    生成并发送 AI 语音，流水写「[AI 语音]」。
- **参数校验**：语音消息 id 非空；OCR 图片地址非空且 ≤ 2048 字符；`file.info` 至少一个标识；
  AI 语音 `character` / `text` 非空。
- **工具位不变**：仍为 28 个工具；全部新能力走 `platform_call` 的层级 op。
- **按文档/风险判断不做（记缺口）**：
  - 原始流式动作（`upload_file_stream`、`download_file_image_stream` /
    `download_file_record_stream` / `download_file_stream`、`test_download_stream`）：
    多帧协议、无逐请求取消、WS 无背压，且会把大文件字节搬进 core；不在本轮接。
  - `download_file`：接受任意 URL / base64 且没有文档化大小上限，属于把平台当下载器，
    先不暴露。
  - `clean_stream_temp_file`：没有流式上传/下载就没必要先给清理入口；留给实现流式时一起做。
  - `musicSignUrl`：它是 SnowLuma 配置项，不是 OneBot 动作，不能变成 op。
- **连接失败路径**：本批没有改连接代码，沿用批次 5 的反向 WS 鉴权/监听测试；
  role 仍是「单活动连接 + 必须配 Universal」，没有新增多连接分流。

**前置**：**批次 5**。流式大概率只能走 HTTP；WS 不适合分段响应。

### 批次 7：扩展域（已完成：只读群相册；其余按产品范围不暴露）

**能力范围**
- 群相册 9 个、QQ 空间 10/26 个、收藏与闪传、系统表情、其他 extended。
- 先做「暴露 / 不暴露」的决策：这些多数是账号级社交功能，不属于聊天核心；暴露会增加权限面和审计成本。
- 如果暴露，必须逐个给 risk / effect；账号级动作考虑 `Scope::Connection`，默认拒绝破坏性操作。

**涉及文件**
- `crates/adapters/onebot-snowluma/src/catalog.rs`
- `crates/adapters/onebot-snowluma/src/outbound.rs`
- `docs/05-adapters.md`

**验证方式**
- 目录快照 + 访问策略测试。
- 真机 spot check（每个分类至少一个动作）。
- 未实现/占位动作不标为可用。

**实际落地**
- **新增 2 个 op（89 → 91），映射 2 个 action（92 → 94）**：
  - `group.album.list` → `get_group_album_list`（Read）：群相册列表。
  - `group.album.media` → `get_group_album_media_list`（Read）：相册里的照片/视频，
    `album_id` 必填，`attach_info` 原样带回上一页的 `nextAttachInfo`。
  - 两者都是 `Scope::Room`、`Risk::Read`、`Effect::Read`，没有状态变更。
- **参数校验**：`group.album.media` 的 `album_id` 不能为空。
- **工具位不变**：仍为 28 个工具；新能力全部走 `platform_call` 的层级 op。
- **产品范围判断不做（记缺口）**：
  - 群相册写操作：`upload_image_to_qun_album`、`upload_video_to_qun_album`、
    `del_group_album_media`、`do_group_album_comment`、点赞/取消点赞。上传/删除属于
    相册媒体写面，评论/点赞属于社交互动，不纳入本轮聊天核心。
  - 文档里没有创建/删除相册的动作，只有媒体级操作；不能凭空补一个 op。
  - QQ 空间 26 个动作全部不暴露：账号级社交发布/删除/点赞/关注，权限和审计成本高。
  - 收藏/闪传 115 个 extended：只读的 `get_collection_list` / 闪传读取也没有排期；
    `send_packet`、`request_decrypt_key`、`get_credentials`、`get_cookies`、
    `get_clientkey`、`set_restart`、`bot_exit`、`clean_cache` 永不暴露。
- **没有新增 `PlatformTag`，没有第二张 op→risk 表**；全部走 `catalog()` + `ROUTES`。

**前置**：无强依赖；但媒体相关动作可能依赖批次 5/6。

### 批次之间的依赖

```
批次 0（改名）
   └─ 批次 1（版本/状态）
        └─ 批次 2（事件/健康）
批次 3（目录：消息/群管/成员/好友/请求/公告）  ── 可与 2 并行
   └─ 批次 4（文件 CRUD/表情）
批次 5（连接形态/鉴权）
   └─ 批次 6（流式/媒体）
批次 7（扩展域） ── 可选，按产品范围决定
```

**哪些批次必须先解决鉴权/连接形态**：
- 批次 5 本身就是连接形态/鉴权批次；
- 批次 6 必须先完成批次 5；
- 如果部署只开 `httpServers` 而不开 `wsServers`，批次 3/4 也需要先有批次 5 的 HTTP transport，否则一条动作都发不出去。

---

## 7. 风险与未知

### 7.1 文档层面的不确定

1. **动作总数不一致**：中文页 200、英文页 219，分类计数也不同。不要把页面数字写进代码或验收标准；以运行时目录为准。
2. **没有事件参考页**：除 `bot_status` 外，notice / request / meta_event 的字段只能按 OneBot v11 推断。`bot_offline` 到底是 `notice_type` 还是 `sub_type`，文档一句话里不足以定论。
3. **`app_name` / `app_version` 实际值未成文**：`get_version_info` 只给了 `string`。不能按固定值 gate 行为。
4. **`protocol_version` 格式未成文**：可能是 `11`、`v11`、`onebot11`。只能宽松匹配，识别不了就 `unknown`。
5. **`get_status.good` 语义未细化**：文档只说「已确认的收发链路健康状态」，没有状态机、没有恢复条件。适合展示，不适合作为唯一健康判据。
6. **`httpClients` 鉴权未成文**：配置表只有 `url` / `timeoutMs`；事件 POST 是否带 token、是否有签名都没写。若要实现 HTTP 上报接收端，不能假设有鉴权。

### 7.2 协议与实现层面的不确定

7. **retcode 语义不完整**：文档只明确 `retcode = 0` 成功；权限不足、动作不存在、参数错误分别是什么 retcode，没有统一表。现有适配器把 1400/1401/1403/1404 做了映射（[outbound.rs](../../crates/adapters/onebot11/src/outbound.rs#L510-L520)），但 SnowLuma 是否完全一致需要实机核对。
8. **动作不存在时的行为未知**：`platform_call` 只查本地 catalog，不会问平台。SnowLuma 对未实现动作可能返回 `retcode` 或占位成功；需要真机确认。
9. **流式传输的 transport 限制未知**：6 个流式动作大概率依赖 HTTP，但文档没有明确说 WS 不支持；需要实测。
10. **反向 WS 的 role 语义**：`Api` / `Event` / `Universal` 是 SnowLuma 侧配置。当前实现按
    `Universal` 处理：同一条连接既收事件也收动作回包；若 SnowLuma 只配 `Api` 或只配
    `Event`，这条连接会缺一半能力。Adapter 没有按 role 分流，部署时必须配 `Universal`。
11. **多 transport 的 token**：HTTP 和 WS 可能各有各的 token，现有 `Connection.token` 只有一个字段；批次 5 已落两条 WS 方向的校验，HTTP 没实现前不拆字段。
12. **`wss://` 未成文**：如果生产需要 TLS，要么反向代理，要么加 TLS 依赖；不要在没确认前声明支持。

### 7.3 行为与安全风险

13. **`bot_status` 不再写进 `private/<self_id>`**：批次 2 已改为账号状态，`bot_offline` 同理。
14. **账号在线 ≠ 桥连得上**：WS 断开报 `down`；账号掉线由 `bot_status` / `get_status` 写进
    `accountOnline`，依赖条目按失败阈值迁移，不会因单条通知误报。
15. **`reportSelfMessage = true` 的重复风险**：`message_sent` 仍忽略，并有测试守护。
16. **HTTP 上报没有签名**：如果批次 5 实现接收端，必须自己加 token/网络隔离，不能假设 SnowLuma 会签名。
17. **token 出现在 URL query**：`?access_token=` 会进日志/代理日志；文档要提醒，生产优先用 Bearer 头。
18. **有符号 message_id**：SnowLuma 明确负值有效；现有代码按字符串保留（`id_of` / `platform_id`），不要为了「看起来像数字」取绝对值。
19. **破坏性动作面扩大**：catalog 每加一个方法，agent 就多一个可调用动作；`allow_destructive = false` 是唯一闸门，批次 3/4/7 必须逐个标 risk，不能批量放行。
20. **「完整实现」的范围没有边界**：SnowLuma 有 200+ 动作，其中相册、空间、闪传、收藏、SSO 包等与聊天核心关系不大。批次 7 要先做产品决策：全部暴露、只暴露聊天相关、还是明确「不适用」。不做这个决策，工作量会失控。

### 7.4 需要实机验证的点

- `get_version_info` 的实际 `app_name` / `app_version` / `protocol_version` 字符串。
- `get_status` 的 `online` / `good` 在账号掉线、网络抖动、刚启动时的取值。
- `bot_status` 的 `online` / `offline` / `bot_offline` 三种事件的真实字段。
- `notice` 其他子类型（`group_upload`、`essence`、`notify.*`）的真实字段。
- `messageFormat = "string"` 时 CQ 码的实际形状。
- 未实现动作（`set_restart`、匿名、分词、收藏创建、最近会话等）的 retcode。
- 流式动作在 WS 与 HTTP 下的行为差异。
- 反向 WS 的 role 与 token 组合。
- 多账号同机端口冲突的实际表现。

---

## 8. 附：本次调研的判定口径

- 「已实现」只算当前代码里能走通并有测试/目录支撑的路径，不算「文档里有、agent 可以手调」。
- 「部分」必须指出缺的是动作、字段、模式还是事件处理。
- 计数以功能域为主，动作数为辅；因为 SnowLuma 的页面计数本身不稳定，不能作为验收标准。
- 所有 SnowLuma 结论都来自 2026-11-04 抓取的站点内容；站点标注最后更新 2026-10-27。上游处于早期开发，结论有时效性。

---

## 9. `platform_call` 单入口设计（已实现，实装偏差见 9.9）

> 状态：已实现（2026-11-05）；第 9.2–9.8 节保留设计原稿，实装差异见 9.9。

### 9.1 为什么不能把 200 个 action 平铺成 tool

实现前 agent 看到的是 `platform_methods`（发现）+ `platform_call`（执行）两个工具，而不是 52 个独立工具。这个方向必须保留；如果每个 action 变成一个 tool：

- 每个 tool 至少要有名称、一句描述、参数 JSON schema、枚举/范围说明。按保守估算，简单 op 约 80–150 tokens，带多参数/示例的 op 约 200–400 tokens。
- SnowLuma 中文页 200 个 action、英文页 219 个，取中位数按 150 tokens/op 估算，仅工具定义就是 **30k tokens 量级**；如果描述带示例，接近 **60k–80k**。这还没算模型在几十个相似工具里做选择的注意力成本。
- 这些 token 每轮都在上下文里，和会话长度无关；200 个 tool 还会显著降低工具选择准确率，并让新增 op 变成一次全局 prompt 变更。

单入口的代价是固定的：两个工具、两份 schema，约 **300–600 tokens**，与 op 数量无关。`list` 只返回当前查询命中的子集，模型看多少由查询范围决定。

### 9.2 两个入口，不是三类工具

对外始终只有两个入口：

```json
{"name": "platform_call", "args": {"op": "message.send", "params": {...}}}
```

```json
{"name": "platform_call.list", "args": {"path": "group", "tags": ["destructive"]}}
```

- `op` 是目录里的规范名；`params` 原样交给平台动作。
- 默认用当前轮次的房间；跨房间操作仍可用现有的可选 `room` 覆盖字段（不属于 op 目录，也不混进平台 `params`）。这样不改变现有的 target 校验。
- `platform_call.list` 是唯一的发现入口；`path` / `tags` 只是**查询维度**，不会各自膨胀成一个工具。
- 旧 `platform_methods` 在迁移期保留为 `platform_call.list` 的别名，最终下线。

**token 恒定的理由**：工具定义只有两份；层级和 tag 是数据，不是工具。增加一个 op 只改目录数据，不改 prompt 的工具列表。这是本节最重要的约束。

### 9.3 目录：层级 + tag

**层级（path）表达「在哪」**：

| path | 含义 | 示例 op |
|---|---|---|
| `message` | 消息收发、历史、撤回、转发 | `message.send`、`message.history`、`message.recall`、`message.forward.get` |
| `group` | 群本身 | `group.info`、`group.list` |
| `group.member` | 群成员 | `group.member.list`、`group.member.kick`、`group.member.mute` |
| `group.admin` | 群管理设置 | `group.admin.set`、`group.admin.ban_all`、`group.admin.notice` |
| `friend` | 好友 | `friend.list`、`friend.delete` |
| `file` | 文件 | `file.group.list`、`file.group.upload`、`file.private.url` |
| `reaction` | 表情回应/表情 | `reaction.set`、`reaction.list`、`reaction.custom.add` |
| `account` | 账号自身 | `account.login_info`、`account.status`、`account.version` |
| `system` | 系统/安全 | `system.check_url` |
| `album` / `qzone` / `flash` / `collection` | 扩展域 | 按批次 7 决定是否暴露 |

**tag 表达跨类属性**，值域是封闭枚举，不允许自由字符串：

| tag | 含义 |
|---|---|
| `destructive` | 踢人、禁言、撤回、解散、改群名等不可逆或高影响操作 |
| `requires_admin` | 需要群主/管理员身份或更高权限 |
| `idempotent` | 重复执行结果一致，可安全重试 |
| `snowluma_only` | SnowLuma 实现特有，其他 OneBot 实现不保证 |
| `common` | 日常聊天/查询常用，默认优先展示 |

**`list` 的过滤语义**：

- `path` 是前缀匹配：`group` 命中 `group.info`、`group.member.kick`、`group.admin.set`；`group.member` 只命中成员子路径。
- `tags` 是 AND：给多个 tag 时，op 必须同时带这些 tag。
- `path` + `tags` 可叠加。
- 不带参数：返回精简清单（op + path + tags + 一句摘要），不全量倒参数表。
- 命中后：返回该 op 的参数表、示例、risk、所需确认方式；仍然只返回命中的子集。

### 9.4 护栏：tag 不能当唯一判据

只靠 tag 的失败模式很具体：新增一个 `group.member.ban` 时忘了打 `destructive`，它就会被当成普通写操作放行；护栏正确性依赖一条容易漏写的注释/字段。tag 是分类，不是安全策略。

设计分三层：

1. **标签（tag）**：只用于 `list` 查询、文档分组和展示。它可以提示风险，但不参与最终放行判断。
2. **策略表（policy table）**：`op -> risk` 的显式映射，独立于 tag 维护。risk 是封闭枚举，例如 `read` / `write` / `destructive` / `admin`。策略表是放行判断的唯一依据。
3. **上下文判定**：调用方身份（哪个 session / 哪个 agent / 是否人工确认）、房间归属（当前轮次房间、订阅与 access 策略）、是否 agent 自动执行。同样的 op，在不同上下文里可以有不同的结论。

**fail closed**：没有策略条目的 op 一律拒绝执行，即使它没有 `destructive` 标签，即使参数看起来无害。忘记加策略 = 不可用，而不是放行。

**destructive 二次确认**：`risk = destructive` 的 op 必须满足以下之一：

- 显式 `confirm = true`（放在 `platform_call` 的 envelope 上，不混进平台 `params`）；
- 先 `dry_run = true` 调用一次，拿到将执行的动作与目标，再以 `confirm = true` 执行。

不允许只在 prompt 里写「这是危险操作，请谨慎」；prompt 可能被注入、被截断，也不能作为审计依据。

**requires_admin 在验证层拦截**：调用方身份/房间归属不满足时直接拒绝，错误里说明缺什么；不要把「你是不是管理员」写进 prompt 让模型自判。`requires_admin` 同时是 tag（供 `list` 展示）和策略表里的 `risk = admin`，但最终判定只看策略表 + 上下文。

**为什么不能只靠 tag**：tag 是新增 op 时最容易忘的注释；策略表是独立的、可枚举的、可测试的映射。两者分开后，漏写 tag 最多影响展示，漏写策略会直接让 op 不可用（fail closed），不会静默放行。

**目录完整性约束（写入文档并建议测试）**：

- 每个 op 必须有父路径（不能是裸名）；
- 每个 op 至少有一个 tag；
- tag 值域是封闭枚举，解析期拒绝未知 tag；
- **每个 op 必须有策略条目**；缺条目时测试/构建失败，不是运行时才拒绝。

### 9.5 校验失败如何回带合法 op

模型写错 op 时，错误不能只说 `unknown_op`，也不能把 200 个 op 全倒出来。返回结构建议：

```json
{
  "error": "unknown_op",
  "given": "group.kick",
  "did_you_mean": [
    {"op": "group.member.kick", "path": "group.member", "tags": ["destructive", "requires_admin"]},
    {"op": "group.admin.set", "path": "group.admin", "tags": ["requires_admin"]}
  ],
  "hint": "call platform_call.list with path=group or tags=[destructive] to see the full group subtree"
}
```

- `did_you_mean` 最多 5 条，按路径编辑距离 + tag 命中排序。
- 参数校验失败时回带 `op`、缺失/非法字段、该 op 的参数表；不回带整张目录。
- 房间目标错配时保留现有提示，并给出正确的 `room` 写法。

### 9.6 常用 op 短别名

短别名只减少输入长度，不新增工具，也不绕过策略表：

| 别名 | 规范 op |
|---|---|
| `send` | `message.send` |
| `reply` | `message.reply` |
| `history` | `message.history` |
| `recall` | `message.recall` |
| `whoami` | `account.login_info` |
| `groups` | `group.list` |
| `members` | `group.member.list` |
| `kick` | `group.member.kick` |
| `mute` | `group.member.mute` |

- 别名表是封闭的，解析后先还原成规范 op，再查策略表。
- 别名必须有测试：别名与规范 op 的 risk、tag、参数校验完全一致。
- 不提供「别名 + 路径拼接」这类开放式简写，避免模型发明新 op。

### 9.7 迁移路径：现有 52 个 method 怎么收敛

现有 catalog 当时是 52 个方法 / 53 个 action（批次 3 后为 68 个 op / 71 个 action），方法名本身已经带点号分组（`message.send_group`、`group.ban`）。迁移不是重写实现，而是给每个旧方法补一份目录元数据：

**映射表结构**（先给结构，不逐条列举）：

| 列 | 含义 |
|---|---|
| `old_method` | 现有 catalog 方法名，如 `group.ban` |
| `new_op` | 新目录规范 op，如 `group.member.mute` |
| `path` | 父路径，如 `group.member` |
| `tags` | 至少一个封闭枚举 tag |
| `risk` | 策略表条目，如 `destructive` |
| `aliases` | 允许的短别名 |
| `note` | 语义变化/兼容说明 |

**归并规则**：

- `message.*` → `message.*`，按发送/历史/撤回/转发拆子路径。
- `group.*` 里「看群/看成员」归 `group.info` / `group.member`；「改群/踢人/禁言/公告」归 `group.admin` / `group.member`。
- `friend.*`、`account.*`、`system.*` 保持原前缀。
- `group.files*`、`group.upload_file`、`group.delete_file` → `file.group.*`。
- `message.react` → `reaction.set`；新增的读表情回应归 `reaction.list`。
- `poke.send` → `message.poke`（群/私聊目标由 `params` 和 `room` 决定）。

**迁移阶段**：

1. **双目录**：`platform_call` 同时接受旧 method 和新 op；`platform_call.list` 返回新目录，旧 `platform_methods` 仍可用。旧 method 作为别名解析到规范 op。
2. **新入口默认**：prompt 与文档只推荐新 op；旧 method 仍可调用，但 `platform_call.list` 标记 `deprecated`。
3. **观察期**：记录旧 method 调用量；没有调用后再删。
4. **下线**：移除旧 method 别名与 `platform_methods`，只保留两个入口。

**不一起做的事**：不把所有旧 method 一次性改写成新 op 的内部实现；旧 method 只做别名映射，实际执行仍走现有 `Outbound::call` / `platform_call` 路径。这样迁移期间行为不变，回退只需删别名表。

### 9.8 待实现的验证清单

- 目录完整性：每个 op 有父路径、至少一个 tag、封闭 tag 值域、策略表条目；缺一项测试失败。
- `platform_call.list`：path 前缀、多 tag AND、path+tag 叠加、无参数精简模式、命中后完整模式。
- 未知 op：回带最多 5 条合法候选 + `list` 提示；不回带整张目录。
- 策略：无策略条目 fail closed；`destructive` 无 `confirm`/`dry_run` 拒绝；`requires_admin` 在验证层拒绝。
- 短别名：别名与规范 op 的 risk/tag/参数校验一致。
- 迁移：旧 method 与规范 op 双解析；旧 method 下线后调用得到明确的 `deprecated`/`unknown_op` 错误。

以上均为**待实现**；本文件只记录设计。

### 9.9 实装结果（2026-11-05）

- **单工具，不是两个工具**：`platform_call` 同时承担执行与发现。给 `op` = 执行；
  不给 `op` = 列目录（`path` 前缀、`tags` 封闭枚举 AND、`query` 关键词、`all` 连接摘要）。
  MCP 工具名不允许点号，`platform_call.list` 不能作为真实工具名；合并成一个工具位更省上下文。
- **op 命名**：按照第 9.3 节的层级思路落成短路径（`msg.send`、`msg.history`、`member.list`、
  `member.kick`、`group.mute`、`group.file.list`…）。当时原 52 个 method 收敛为 50 个 op
  （批次 3 后为 68 个）：`message.send_group` / `message.send_private` 合并成 `msg.send`，
  `message.history_group` / `message.history_private` 合并成 `msg.history`，按房间选 OneBot 动作。
- **策略表**：每个 op 的 `MethodSpec.risk` 是显式值（read / write / destructive），
  不从 tag 推导；不在 catalog 的 op 一律 `unknown_op` 拒绝。`destructive` 仍由
  `allow_destructive` 第二道闸门控制。`requires_admin` 只是展示标签；core 目前不猜群管理员身份，
  平台侧权限不足会拒绝。
- **tag**：`MethodSpec.tags` 改为封闭枚举 `PlatformTag`（30 个固定值），wire 入口未知 tag 直接拒绝。
  catalog 自动补 `read/write/destructive`、`room/connection`、`idempotent`，再保留作者声明的用途标签。
- **不做兼容层**：旧 method 名直接删除，不保留别名；短别名（send/kick 等）也未实现。
  目录、路由、测试、harness schema 一次改齐。
- **票据**：`access_request` 的 `platform_call:<op>` 与普通 `platform_call` 共用
  `find_platform_op` + `assemble_platform_call`；批准即执行的路径不另起校验。
- **证据**：adapter `every_op_is_hierarchical_tagged_and_routed`、
  `unified_send_and_history_dispatch_by_room`；kernel `unknown_op_suggests_a_direct_neighbour`、
  `destructive_method_is_refused_by_default`、`ticket_and_normal_call_share_unknown_op_rejection`；
  `platform_call` schema 从 6,747 bytes（两工具）降到 3,922 bytes（单工具）。

