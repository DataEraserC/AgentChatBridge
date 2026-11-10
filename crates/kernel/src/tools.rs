//! 工具面：agent 能调的东西。
//!
//! 执行端全在桥里（流水、笔记、订阅表都在桥手上），harness 只负责让 agent 看见这些
//! 工具、把调用原样转发过来。参数形状统一用 snake_case 的 JSON 对象。
//!
//! 铁律：没有任何工具能发到订阅之外的房间；没有自由改配置或改水位的工具。
//!
//! 房间参数的写法一律是 `连接:段/段`（`qq:group/123`）。`send_message` 回当前轮次会话，
//! `send_message_to` 的目标只落在**当前会话订阅的房间**范围内——跨会话发送已砍（ADR-0017）。

use serde::Deserialize;

/// 工具清单。顺序即给 harness 注册的顺序。
pub const TOOL_NAMES: &[&str] = &[
    "send_message",
    "send_message_to",
    // 一次性票据：群内申请（单条 = 单张，多条 = 批量），私信管理员批准。
    "access_request",
    // 只读查询自己发起的票据：按 id 查单张，按 batch_id 查整批。
    "access_ticket_get",
    // 只读：回答「此刻能不能用」，不回答「世界上有什么」。
    "capabilities",
    "ack_messages",
    "get_history",
    "fetch_records",
    "wait_message",
    "list_rooms",
    // 笔记：单入口，op = list / set / del。
    "notes",
    "get_platform_events",
    // 平台能力：单入口。给 op = 执行，不给 op = 按层级/标签列目录（ADR-0020）。
    "platform_call",
    // 插件目录：本会话实际用了哪些 wake/access/delivery 策略，它们声称做什么。
    "describe_plugins",
    // 动态扩展目录 authoring（仅管理员会话；T6b）。
    "extensions_list",
    "extensions_read",
    "extensions_write",
    "extensions_reload",
    // 后台任务记录（T7a）：单入口，op = list / get。
    "tasks",
    // 反馈：把「哪里卡住了」写回桥里（ADR-0021）。活干不下去时用它，不要直接摸平台端口。
    "report_feedback",
    // 反馈查询（T7c）：单入口，op = list / get。回复单独一件，避免迟到调用被放宽。
    "feedback",
    "feedback_reply",
    // 插件工具：索引 + 通用调用口。插件自己的工具不必挤进 core 工具表。
    "plugin_tools",
    "plugin_tool_call",
    // 记忆：文件层
    "memory_read",
    "memory_write",
    "memory_list",
    "memory_search",
    // 记忆：git 层。单入口，op = status / log / diff / blame / add / commit / push / branch。
    "memory_git",
    // 管理面：不设身份分支，一律先问 Policy（config_read/config_write/policy_write/plugin_admin）。
    "config",
    "policy",
    "plugin_config",
    "plugin_action",
];

/// 常驻工具：**显式白名单**，每条 harness 都永远注入这一层。
///
/// 只放高频路径与「能力入口」：收发消息、列房间、能力自检、平台调用、票据申请/查询、
/// 结束轮次确认。补上下文/回看由 harness 在投递溢出时按需注入，不占常驻位。
/// 新增工具时不要写成「除长尾外都常驻」——反向定义会让常驻层自动膨胀。
pub const RESIDENT_TOOLS: &[&str] = &[
    "send_message",
    "send_message_to",
    "list_rooms",
    "capabilities",
    "platform_call",
    "access_request",
    "access_ticket_get",
    "ack_messages",
];

/// 按需工具：不进常驻层；harness 在投递溢出时给完整 schema。
///
/// `get_history` / `fetch_records` 的使用场景是「投递溢出」和「补齐上下文」——
/// 这两件事 harness 自己知道（`delivery.omitted` / `delivery.more`）。但 MCP
/// 客户端只在连接时拉一次 `tools/list`，所以 MCP facade 启动时必须把目录层
/// （含这两个工具）先注册出来，否则会话里永远是 unknown tool；开轮后是否给
/// 完整 schema 仍由 harness 按溢出标志决定。
pub const ON_DEMAND_TOOLS: &[&str] = &["get_history", "fetch_records"];

/// 这个工具是否按需注入（只影响 harness 的可见集，不影响可调用性）。
pub fn is_on_demand(tool: &str) -> bool {
    ON_DEMAND_TOOLS.contains(&tool)
}

/// 目录层的一条索引：名字 + 一句话用途 + 何时用。
///
/// 长尾工具的完整 schema 仍在 harness 的 `TOOLS` 里；core 只给索引与权限判定。
pub struct ToolCatalogEntry {
    pub name: &'static str,
    pub purpose: &'static str,
    pub when: &'static str,
}

/// 非驻留工具的目录索引。覆盖 `TOOL_NAMES` 里所有不在 [`RESIDENT_TOOLS`] 的条目；
/// 测试会锁住这个覆盖关系。
pub const TOOL_CATALOG: &[ToolCatalogEntry] = &[
    ToolCatalogEntry {
        name: "get_history",
        purpose: "翻房间历史记录，补齐上下文",
        when: "投递溢出或需要往前翻时（harness 溢出轮次给完整 schema）",
    },
    ToolCatalogEntry {
        name: "fetch_records",
        purpose: "按水位编号精确回看记录",
        when: "投递溢出、只拿到部分编号时（harness 溢出轮次给完整 schema）",
    },
    ToolCatalogEntry {
        name: "wait_message",
        purpose: "阻塞等同房间下一条消息",
        when: "没事可做、想等消息时",
    },
    ToolCatalogEntry {
        name: "notes",
        purpose: "房间共享笔记（list/set/del）",
        when: "跨轮记房间约定时",
    },
    ToolCatalogEntry {
        name: "get_platform_events",
        purpose: "查最近平台事件",
        when: "需要事件原始载荷时",
    },
    ToolCatalogEntry {
        name: "describe_plugins",
        purpose: "看本会话的策略 provider",
        when: "怀疑插件改写行为时",
    },
    ToolCatalogEntry {
        name: "extensions_list",
        purpose: "列动态扩展模块",
        when: "管理员排查扩展时",
    },
    ToolCatalogEntry {
        name: "extensions_read",
        purpose: "读扩展源码",
        when: "管理员审查扩展时",
    },
    ToolCatalogEntry {
        name: "extensions_write",
        purpose: "写扩展源码",
        when: "管理员改扩展时",
    },
    ToolCatalogEntry {
        name: "extensions_reload",
        purpose: "热重载扩展目录",
        when: "改完扩展后",
    },
    ToolCatalogEntry {
        name: "tasks",
        purpose: "查后台任务（list/get/wait）",
        when: "异步任务有结果时",
    },
    ToolCatalogEntry {
        name: "report_feedback",
        purpose: "记录卡点反馈",
        when: "工具反复失败时",
    },
    ToolCatalogEntry {
        name: "feedback",
        purpose: "查反馈记录（list/get）",
        when: "确认问题是否已记录时",
    },
    ToolCatalogEntry {
        name: "feedback_reply",
        purpose: "回复反馈",
        when: "问题有补充/已解决时",
    },
    ToolCatalogEntry {
        name: "plugin_tools",
        purpose: "列插件工具",
        when: "要调第三方工具时",
    },
    ToolCatalogEntry {
        name: "plugin_tool_call",
        purpose: "调用插件工具",
        when: "拿到名字和参数后",
    },
    ToolCatalogEntry {
        name: "memory_read",
        purpose: "读记忆文件",
        when: "需要长期记忆原文时",
    },
    ToolCatalogEntry {
        name: "memory_write",
        purpose: "写记忆文件",
        when: "要落结论进记忆时",
    },
    ToolCatalogEntry {
        name: "memory_list",
        purpose: "列记忆目录",
        when: "不知道有哪些文件时",
    },
    ToolCatalogEntry {
        name: "memory_search",
        purpose: "搜记忆文本",
        when: "只记得关键词时",
    },
    ToolCatalogEntry {
        name: "memory_git",
        purpose: "记忆 git（status/log/diff/add/commit/push）",
        when: "提交/推送/看历史时",
    },
    ToolCatalogEntry {
        name: "config",
        purpose: "读写连接/会话/人格/记忆/harness（get/list/set/delete）",
        when: "被明确要求改桥配置、且持有 config_read/config_write 时",
    },
    ToolCatalogEntry {
        name: "policy",
        purpose: "读或整份替换授权策略（get/set）",
        when: "被明确要求调整权限、且持有 policy_write 时",
    },
    ToolCatalogEntry {
        name: "plugin_config",
        purpose: "读写插件配置（get/set）",
        when: "调整插件参数、且持有 config_write 时",
    },
    ToolCatalogEntry {
        name: "plugin_action",
        purpose: "执行插件声明的动作",
        when: "插件提供动作、且持有 plugin_admin 时",
    },
];

/// 这个工具是否在常驻白名单里。
pub fn is_resident(tool: &str) -> bool {
    RESIDENT_TOOLS.contains(&tool)
}

/// 查目录层的索引；常驻工具或未知工具返回 `None`。
pub fn catalog_entry(tool: &str) -> Option<&'static ToolCatalogEntry> {
    TOOL_CATALOG.iter().find(|entry| entry.name == tool)
}

/// 元素数组的包装：解析走 proto 的人话通道（别名兼容 + 报错带原文），
/// 不用 serde 的干巴巴缺字段报错。
#[derive(Debug)]
pub struct Elements(pub Vec<acb_proto::Element>);

impl<'de> serde::Deserialize<'de> for Elements {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        acb_proto::parse_elements(value)
            .map(Elements)
            .map_err(serde::de::Error::custom)
    }
}

/// 发给当前轮次房间的参数。
#[derive(Debug, Deserialize)]
pub struct SendArgs {
    pub elements: Elements,
}

/// 发到别的房间的参数。形状和 [`SendArgs`] 一样，只多一个目标。
#[derive(Debug, Deserialize)]
pub struct SendToArgs {
    pub elements: Elements,
    /// 目标房间，形如 `qq:group/123`。必须落在当前会话订阅范围之内。
    pub to: String,
}

/// 申请一张或多张一次性访问票据。申请本身不执行任何平台动作。
#[derive(Debug, Deserialize)]
pub struct AccessRequestArgs {
    /// 一条 = 单张票据；多条 = 批量（共享 batch_id，每张仍是独立票据）。
    pub items: Vec<AccessRequestItem>,
}

#[derive(Debug, Deserialize)]
pub struct AccessRequestItem {
    /// 目标房间，形如 `qq:group/123`；一张票据只覆盖这一个房间。
    pub target: String,
    /// 要放行的具体操作。形如 `send_message_to`、`get_history`、
    /// `platform_call:member.kick`；不接受模糊的「整个房间」。
    pub ops: Vec<String>,
    /// 挂起动作的参数快照。发送类给 `elements` / `text`；平台调用给 params 对象。
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    /// 申请人自述理由；只用于审批展示与审计，不参与授权判定。旧调用可省略。
    #[serde(default)]
    pub reason: Option<String>,
}

/// 只读查询票据。`id` 与 `batch_id` 必须且只能给一个。
#[derive(Debug, Default, Deserialize)]
pub struct AccessTicketGetArgs {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub batch_id: Option<String>,
}

/// 能力自检：`scope=session` 看整体，`scope=room` 看房间，`scope=tool` 看单个工具/op。
#[derive(Debug, Default, Deserialize)]
pub struct CapabilitiesArgs {
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub room: Option<String>,
    /// scope=tool 时的工具名或平台 op 名。
    #[serde(default)]
    pub name: Option<String>,
    /// scope=surface 的投影：visible（默认）/ all / hidden。只给 view 不给 scope 时按 surface。
    #[serde(default)]
    pub view: Option<String>,
    /// view=all / hidden 的分页偏移；visible 不使用。
    #[serde(default)]
    pub offset: Option<usize>,
    /// view=all / hidden 的页大小；visible 不使用。
    #[serde(default)]
    pub limit: Option<usize>,
}

/// 确认这批消息处理完了。
#[derive(Debug, Deserialize)]
pub struct AckArgs {
    pub ids: Vec<u64>,
}

/// 管理面：业务对象读写。`value` 是这一条的 TOML 原文。
#[derive(Debug, Deserialize)]
pub struct ConfigToolArgs {
    /// get / list / set / delete
    pub action: String,
    /// connection / session / persona / memory / harness
    #[serde(default)]
    pub section: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    /// set 时必需：一条对象的 TOML，形如 `[[sessions]]\nid = "..."`。
    #[serde(default)]
    pub value: Option<String>,
}

/// 管理面：读或整份替换策略。
#[derive(Debug, Deserialize)]
pub struct PolicyToolArgs {
    /// get / set
    pub action: String,
    /// set 时的整份 `[grants]` 形状。
    #[serde(default)]
    pub grants: Option<serde_json::Value>,
}

/// 管理面：插件配置。
#[derive(Debug, Deserialize)]
pub struct PluginConfigToolArgs {
    /// get / set
    pub action: String,
    pub plugin: String,
    /// set 时：要写的键值；只写提到的键。
    #[serde(default)]
    pub config: Option<serde_json::Map<String, serde_json::Value>>,
}

/// 管理面：执行插件动作。
#[derive(Debug, Deserialize)]
pub struct PluginActionToolArgs {
    pub plugin: String,
    pub action: String,
    #[serde(default)]
    pub args: Option<serde_json::Value>,
}

/// 查流水。
#[derive(Debug, Default, Deserialize)]
pub struct HistoryArgs {
    /// 房间；不填就是当前轮次的房间。
    #[serde(default)]
    pub room: Option<String>,
    /// 最多几条，默认 20，上限 200。
    #[serde(default)]
    pub limit: Option<usize>,
    /// 只看编号小于它的记录（往前翻）。
    #[serde(default)]
    pub before: Option<u64>,
    /// 只看编号大于它的记录（往后翻，按从旧到新返回）。给了它就走「补看更新消息」模式。
    #[serde(default)]
    pub after: Option<u64>,
    /// 只返回这些列。缺省给一个精简子集（id/at/sender/elements）。
    /// 非法列名会报错，并在错误里列出全部合法列名。
    #[serde(default)]
    pub columns: Option<Vec<String>>,
}

/// 按水位编号精确查一批记录。
#[derive(Debug, Default, Deserialize)]
pub struct FetchRecordsArgs {
    /// 房间；不填就是当前轮次的房间。水位编号是房间内递增的，跨房间会撞号，
    /// 所以必须限定在一个房间内查。
    #[serde(default)]
    pub room: Option<String>,
    /// 要查的水位编号（投递记录前面的 `#编号`）。
    pub ids: Vec<u64>,
    /// 只返回这些列。缺省给一个精简子集（id/at/sender/elements）。
    /// 非法列名会报错，并在错误里列出全部合法列名。
    #[serde(default)]
    pub columns: Option<Vec<String>>,
}

/// 在当前轮次房间里等新消息。
#[derive(Debug, Default, Deserialize)]
pub struct WaitMessageArgs {
    /// 房间；不填就是当前轮次的房间。只允许当前轮次房间，因为读到的编号
    /// 要能直接 `ack_messages`。
    #[serde(default)]
    pub room: Option<String>,
    /// 最多等多久，默认 10 秒、上限 60 秒。到点没有新消息就返回空。
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// 最多返回几条，默认 20、上限 200。
    #[serde(default)]
    pub limit: Option<usize>,
    /// 只返回这些列，规则同 `get_history`。
    #[serde(default)]
    pub columns: Option<Vec<String>>,
}

/// `wait_message` 的默认超时与上限。
pub const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
pub const MAX_WAIT_TIMEOUT_MS: u64 = 60_000;

/// 一条记录能按需取的全部列。顺序即文档/错误提示里的列出顺序。
pub const RECORD_COLUMNS: &[&str] = &[
    "id",
    "room",
    "kind",
    "sender",
    "at",
    "elements",
    "platform_id",
    "quote_ref",
    "at_self",
    "reply_to_self",
    "labels",
];

/// 不指定 `columns` 时的精简默认：够 agent 读内容、知道是谁、什么时候说的。
pub const DEFAULT_COLUMNS: &[&str] = &["id", "at", "sender", "elements", "labels"];

/// 查笔记。
#[derive(Debug, Default, Deserialize)]
pub struct NotesArgs {
    #[serde(default)]
    pub room: Option<String>,
    /// 只看某种笔记：`term` / `member` / `topic`。
    #[serde(default)]
    pub kind: Option<String>,
    /// 只看某个键；给 key 时通常是一条，用于索引降级后取回完整 value。
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

/// 写一条笔记。
#[derive(Debug, Deserialize)]
pub struct SetNoteArgs {
    #[serde(default)]
    pub room: Option<String>,
    /// 默认 `term`。
    #[serde(default)]
    pub kind: Option<String>,
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub label: Option<String>,
    /// 只有 `topic` 用得上：`open` / `closed`。
    #[serde(default)]
    pub status: Option<String>,
}

/// 删一条笔记。
#[derive(Debug, Deserialize)]
pub struct DelNoteArgs {
    #[serde(default)]
    pub room: Option<String>,
    /// 默认 `term`。
    #[serde(default)]
    pub kind: Option<String>,
    pub key: String,
}

/// 笔记单入口：`op = list / set / del`，其余字段按 op 取用。
#[derive(Debug, Default, Deserialize)]
pub struct NotesToolArgs {
    pub op: String,
    #[serde(default)]
    pub room: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

/// 查平台事件。
#[derive(Debug, Default, Deserialize)]
pub struct EventsArgs {
    #[serde(default)]
    pub room: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// 从第几条开始给（按时间正序数，翻页用上一次返回的 `next_offset`）。
    #[serde(default)]
    pub offset: Option<usize>,
}

/// 分页默认值：一次给多少条。来源：实现默认。
///
/// 100 条对绝大多数平台列表（群成员、好友、群列表）够用；大群才需要翻页。
pub const DEFAULT_PAGE_LIMIT: usize = 100;

/// 分页上限：一次最多给多少条。再大就该翻页，而不是硬塞进上下文。
pub const MAX_PAGE_LIMIT: usize = 500;

/// `platform_call` 的统一输入。
///
/// 给 `op` = 执行；不给 `op` = 列目录。二者字段互斥，由 core 明确拒绝混用。
#[derive(Debug, Default, Deserialize)]
pub struct PlatformToolArgs {
    #[serde(default)]
    pub room: Option<String>,
    /// 列目录时指定另一条连接；执行时不使用。
    #[serde(default)]
    pub connection: Option<String>,
    /// 层级化 op，如 `msg.send`、`member.list`、`group.mute`。
    #[serde(default)]
    pub op: Option<String>,
    /// 平台参数；core 原样转给适配器。
    #[serde(default)]
    pub params: serde_json::Value,
    /// 列目录：按路径前缀过滤，如 `group`、`group.file`。
    #[serde(default)]
    pub path: Option<String>,
    /// 列目录：按封闭标签过滤（AND），如 `destructive`、`requires_admin`。
    #[serde(default)]
    pub tags: Vec<String>,
    /// 列目录：按关键词搜 op / 说明 / 标签。
    #[serde(default)]
    pub query: Option<String>,
    /// 列目录：只给每条订阅连接的摘要，不展开 op。
    #[serde(default)]
    pub all: bool,
    /// 执行：结果里的主数组最多给几条。
    #[serde(default)]
    pub limit: Option<usize>,
    /// 执行：从第几条开始给。
    #[serde(default)]
    pub offset: Option<usize>,
    /// 执行：过滤 / 投影 / 排序 / 分组。
    #[serde(flatten)]
    pub shape: crate::query::Query,
}

/// 真正执行一次平台调用。分页与结果过滤都在桥这层做（ADR-0022 / ADR-0023）。
#[derive(Debug, Default, Deserialize)]
pub struct PlatformCallArgs {
    #[serde(default)]
    pub room: Option<String>,
    pub op: String,
    #[serde(default)]
    pub params: serde_json::Value,
    /// 结果里的主数组最多给几条。默认 [`DEFAULT_PAGE_LIMIT`]，上限 [`MAX_PAGE_LIMIT`]。
    #[serde(default)]
    pub limit: Option<usize>,
    /// 从第几条开始给（翻页用，值取上一次返回的 `_page.next_offset`）。
    #[serde(default)]
    pub offset: Option<usize>,
    /// 过滤 / 投影 / 排序 / 分组。四个都不给就是平台原样的结果。
    #[serde(flatten)]
    pub query: crate::query::Query,
}

/// 列平台目录（`platform_call` 不给 `op` 时的形状）。
#[derive(Debug, Default)]
pub struct PlatformListArgs {
    pub room: Option<String>,
    pub connection: Option<String>,
    pub path: Option<String>,
    pub tags: Vec<crate::outbound::PlatformTag>,
    pub query: Option<String>,
    pub all: bool,
    /// 目录分页：最多给几条；缺省按目录预算自动取一页。
    pub limit: Option<usize>,
    /// 目录分页：从第几条开始给。
    pub offset: Option<usize>,
}

/// 查插件/策略目录。默认只看当前会话实际用到的 provider。
#[derive(Debug, Default, Deserialize)]
pub struct DescribePluginsArgs {
    /// `session`（默认）或 `global`。
    #[serde(default)]
    pub scope: Option<String>,
    /// 只看某个扩展点（`wake` / `access` / `delivery` / `turn_error` / `clock` /
    /// `receive` / `platform_adapter`）。只对 `scope=global` 生效。
    #[serde(default)]
    pub extension: Option<String>,
}

/// 动态扩展目录：列模块。`namespace` 默认 `dyn`。
#[derive(Debug, Default, Deserialize)]
pub struct ExtensionsListArgs {
    #[serde(default)]
    pub namespace: Option<String>,
}

/// 动态扩展目录：读一个相对路径（只允许模块目录下的 `.toml`）。
#[derive(Debug, Deserialize)]
pub struct ExtensionsReadArgs {
    pub path: String,
    #[serde(default)]
    pub namespace: Option<String>,
}

/// 动态扩展目录：写一个相对路径（只允许模块目录下的 `.toml`，原子写入）。
#[derive(Debug, Deserialize)]
pub struct ExtensionsWriteArgs {
    pub path: String,
    pub text: String,
    #[serde(default)]
    pub namespace: Option<String>,
}

/// 动态扩展目录：排队重载。
#[derive(Debug, Default, Deserialize)]
pub struct ExtensionsReloadArgs {
    #[serde(default)]
    pub namespace: Option<String>,
}

/// 后台任务列表：按状态过滤 + 分页。
#[derive(Debug, Default, Deserialize)]
pub struct TaskListArgs {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

/// 后台任务详情。
#[derive(Debug, Deserialize)]
pub struct TaskGetArgs {
    pub id: String,
}

/// 后台任务单入口：`op = list / get`。
#[derive(Debug, Default, Deserialize)]
pub struct TasksToolArgs {
    pub op: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub id: Option<String>,
    /// `op=wait` 的单次等待时长（毫秒）。由调用方按所在 harness 和任务大小自选；
    /// 桥只按 `[tasks].wait_max_ms`（缺省 45 秒）截断，截断会在返回里注明。
    /// **不用于 op=get**——get 永远立即返回。
    #[serde(default)]
    pub wait_ms: Option<u64>,
}

/// 一条反馈（ADR-0021）。
///
/// 刻意做成「写建议」而不是「填表」：正文一个字段，想写多长写多长；
/// 出处编号、发生次数这些用来让看的人省事，但都有缺省——只写正文也能入库。
#[derive(Debug, Default, Deserialize)]
pub struct FeedbackArgs {
    /// 正文。第一行会被当成标题；多写几行没关系（上限 4000 字，超出会截断并告知）。
    pub text: String,
    /// 这个坑撞了几次。默认 1；说清楚次数，排序才有意义。
    #[serde(default)]
    pub count: Option<u32>,
    /// 出处：水位编号（`#620`）、平台消息 id、工具名、错误码、接口名，随便贴。
    #[serde(default)]
    pub refs: Vec<String>,
    /// 类型：`bug`（坏了）/ `gap`（缺能力）/ `friction`（能用但别扭）/ `idea`（想要什么）。
    /// 缺省 `friction`。
    #[serde(default)]
    pub kind: Option<String>,
    /// 影响：`blocker`（活干不下去）/ `major` / `minor`。缺省按类型推。
    #[serde(default)]
    pub severity: Option<String>,
    /// 建议怎么改（可空）。有就给，没有也行。
    #[serde(default)]
    pub suggestion: Option<String>,
    /// 出事时在调哪个桥工具（如 `platform_call`）。缺省取当前正在执行的工具。
    #[serde(default)]
    pub tool: Option<String>,
}

/// 列反馈：只看本会话写过的。`status` 过滤生命周期。
#[derive(Debug, Default, Deserialize)]
pub struct FeedbackListArgs {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

/// 查一条反馈：id + 回复。
#[derive(Debug, Deserialize)]
pub struct FeedbackGetArgs {
    pub id: i64,
}

/// 反馈查询单入口：`op = list / get`。回复单独一个工具，保留原来的迟到调用边界。
#[derive(Debug, Default, Deserialize)]
pub struct FeedbackToolArgs {
    pub op: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub id: Option<i64>,
}

/// 回复一条反馈。维护方会话或该反馈的原会话可以写。
#[derive(Debug, Deserialize)]
pub struct FeedbackReplyArgs {
    pub id: i64,
    pub text: String,
}

/// 插件工具索引：不给 name 只给 name + summary；给 name 返回完整 schema。
#[derive(Debug, Default, Deserialize)]
pub struct PluginToolsArgs {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

/// 插件工具调用必须显式声明的权限作用域。
///
/// 插件工具不像 `platform_call` 有 `MethodSpec.scope` 可查，所以必须由调用方声明；
/// core 按声明执行与平台调用同一套房间/连接检查。
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginToolScope {
    Room { room: String },
    Connection { connection: String },
}

/// 调用插件工具：工具名 + 原样参数 + 权限作用域。
#[derive(Debug, Deserialize)]
pub struct PluginToolCallArgs {
    pub tool: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
    /// 没声明就拒绝；不能默认成当前房间。
    #[serde(default)]
    pub scope: Option<PluginToolScope>,
}

/// 记忆工具公共参数：操作哪个记忆 + worktree 内的相对路径。
///
/// `memory` 是记忆 id（必填——一个会话可能引用多份记忆）；`path` 是相对 worktree 的
/// 路径，空串或 `.` 表示仓库根。
#[derive(Debug, Default, Deserialize)]
pub struct MemoryArgs {
    pub memory: String,
    #[serde(default)]
    pub path: String,
}

/// 写文件：`path` + `content`。
#[derive(Debug, Deserialize)]
pub struct MemoryWriteArgs {
    pub memory: String,
    pub path: String,
    pub content: String,
}

/// 列目录：`path` + 分页。
#[derive(Debug, Default, Deserialize)]
pub struct MemoryListArgs {
    pub memory: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

/// 搜索：`path`（搜索起点）+ `needle`（关键字）+ 分页。
#[derive(Debug, Default, Deserialize)]
pub struct MemorySearchArgs {
    pub memory: String,
    #[serde(default)]
    pub path: String,
    pub needle: String,
    /// 最多给多少条命中。默认 100，上限 500——搜出一万行对谁都没用。
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

/// git log：`limit` 条。
#[derive(Debug, Default, Deserialize)]
pub struct MemoryLogArgs {
    pub memory: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// git add：`path` 空串表示暂存全部。
#[derive(Debug, Default, Deserialize)]
pub struct MemoryAddArgs {
    pub memory: String,
    #[serde(default)]
    pub path: String,
}

/// git commit：`message` 提交信息。
#[derive(Debug, Deserialize)]
pub struct MemoryCommitArgs {
    pub memory: String,
    pub message: String,
}

/// git blame：`path` 必填。
#[derive(Debug, Deserialize)]
pub struct MemoryBlameArgs {
    pub memory: String,
    pub path: String,
}

/// 记忆 git 单入口：`op = status / log / diff / blame / add / commit / push / branch`。
/// 各 op 只取自己用得到的字段；`commit` 要 `message`，`blame` 要非空 `path`。
#[derive(Debug, Default, Deserialize)]
pub struct MemoryGitToolArgs {
    pub op: String,
    pub memory: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub message: Option<String>,
}

/// 工具的真假判定：这个名字认识不认识。
pub fn is_known(tool: &str) -> bool {
    TOOL_NAMES.contains(&tool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resident_set_is_explicit_and_bounded() {
        assert_eq!(RESIDENT_TOOLS.len(), 8, "常驻层固定 8 个，别让反向定义膨胀");
        for tool in RESIDENT_TOOLS {
            assert!(TOOL_NAMES.contains(tool), "常驻工具 {tool} 不在 TOOL_NAMES");
            assert!(
                !TOOL_CATALOG.iter().any(|entry| entry.name == *tool),
                "常驻工具 {tool} 不能再进目录层"
            );
        }
        for tool in ON_DEMAND_TOOLS {
            assert!(TOOL_NAMES.contains(tool), "按需工具 {tool} 不在 TOOL_NAMES");
            assert!(!is_resident(tool), "按需工具 {tool} 不能同时在常驻层");
            assert!(
                catalog_entry(tool).is_some(),
                "按需工具 {tool} 仍要有目录索引"
            );
        }
    }

    #[test]
    fn catalog_covers_every_non_resident_tool() {
        for tool in TOOL_NAMES {
            if RESIDENT_TOOLS.contains(tool) {
                continue;
            }
            let entry = catalog_entry(tool).unwrap_or_else(|| {
                panic!("长尾工具 {tool} 没有目录索引；新增工具必须显式决定放哪一层")
            });
            assert!(!entry.purpose.trim().is_empty(), "{tool} purpose 为空");
            assert!(!entry.when.trim().is_empty(), "{tool} when 为空");
        }
    }
}
