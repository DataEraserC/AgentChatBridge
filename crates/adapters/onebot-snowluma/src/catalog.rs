//! 平台方法表：这台 QQ 号声明给模型看的**全部**能力。
//!
//! 目标是一句话：**人在一个房间里能做的事，模型都能做**。所以这里不是「挑几个安全的」，
//! 而是把 OneBot 11（含 go-cqhttp / NapCat / LLOneBot 的常用扩展）里跟「聊天」有关的
//! 动作全列上；不安全的那些不删，只标 `Risk::Destructive`，由配置决定放不放行
//! （`[[connections]] allow_destructive`）。
//!
//! 故意不列的东西，理由都是同一条——**那不是「人能做的事」**：
//! - `get_cookies` / `get_csrf_token` / `get_credentials`：那是账号凭据，不是聊天能力。
//! - `set_restart` / `clean_cache`：那是机器人进程运维，模型碰它只会把桥搞挂。
//! - `get_status` 之外的探活类接口价值低，占了工具目录的位置不划算。
//!
//! core 不认识这里任何一个名字，它只认 `MethodSpec` 这个形状（ADR-0020）。

use std::sync::OnceLock;

use acb_kernel::{
    Effect, MethodSpec, ParamSpec, ParamType, PlatformTag, RecordMode, Risk, Scope, TargetSpec,
    outbound::tags,
};

// ── 参数写法 ──────────────────────────────────────────────────────────────
//
// 一张表几百行，字段名全写出来会淹掉内容，所以统一用这几个短构造。

/// 必填字符串。
fn req(name: &'static str, desc: &'static str) -> ParamSpec {
    ParamSpec::required(name, desc).with_example(example_for(name))
}

/// 可选字符串。
fn opt(name: &'static str, desc: &'static str) -> ParamSpec {
    ParamSpec::optional(name, ParamType::String, desc).with_example(example_for(name))
}

/// OneBot 参数名的示例值。平台语义留在适配器，core 不再按名字猜。
fn example_for(name: &str) -> &'static str {
    match name {
        "group_id" | "group" => "123456",
        "user_id" | "uin" | "qq" | "target" | "user" => "10000",
        "file" | "url" | "image" => "https://example.com/a.png",
        "text" | "message" => "你好",
        "name" | "card" | "nickname" => "新名字",
        "keyword" | "query" | "word" => "关键词",
        "emoji_id" => "0123456789abcdef0123456789abcdef",
        "policy" => "require_approval",
        "categoryId" | "categoryName" => "分组名",
        _ if name == "id" || name.ends_with("_id") => "123456",
        _ => "示例值",
    }
}

/// 可选数字。
const fn num(name: &'static str, desc: &'static str) -> ParamSpec {
    ParamSpec::optional(name, ParamType::Integer, desc)
}

/// 可选布尔。
const fn flag(name: &'static str, desc: &'static str) -> ParamSpec {
    ParamSpec::optional(name, ParamType::Bool, desc)
}

/// 必填但形状不固定（消息段：字符串或数组都合法）。
fn any(name: &'static str, desc: &'static str) -> ParamSpec {
    let param = ParamSpec::any(name, desc);
    match name {
        "message" => param.with_example(serde_json::json!([
            { "type": "text", "text": "你好" }
        ])),
        "messages" => param.with_example(serde_json::json!([{
            "type": "node",
            "data": {
                "name": "某人",
                "uin": "10000",
                "content": [{ "type": "text", "text": "你好" }],
            }
        }])),
        "user_ids" => param.with_example(serde_json::json!([10000, 10001])),
        _ => param,
    }
}

/// 先按可选构造，再打上必填：数字/布尔/枚举参数没有现成的必填构造。
fn require(mut param: ParamSpec) -> ParamSpec {
    param.required = true;
    param
}

/// 必填的枚举字符串参数。
fn choices(name: &'static str, desc: &'static str, values: &'static [&'static str]) -> ParamSpec {
    let mut param = ParamSpec::required(name, desc);
    param.values = values
        .iter()
        .map(|value| std::borrow::Cow::Borrowed(*value))
        .collect::<Vec<_>>()
        .into();
    param
}

fn m(
    name: &'static str,
    summary: &'static str,
    tags: &[PlatformTag],
    risk: Risk,
    params: &[ParamSpec],
) -> MethodSpec {
    MethodSpec {
        name: name.into(),
        summary: summary.into(),
        tags: tags.to_vec().into(),
        risk,
        scope: Scope::Room,
        params: params.to_vec().into(),
        targets: Vec::new().into(),
        // 下面两个字段由 `catalog()` 按方法名/风险推导；这里只是静态表的缺省值。
        effect: Effect::Read,
        record: RecordMode::None,
    }
}

/// 连接级方法：这个号上的事，跟房间无关（好友列表、账号资料、版本）。
///
/// 模型查好友列表时不该被迫先编一个群号出来。这类方法 `room` 可以不提；
/// 提了也只用来「会话订阅了多个连接时选是哪个号」。
fn cm(
    name: &'static str,
    summary: &'static str,
    tags: &[PlatformTag],
    risk: Risk,
    params: &[ParamSpec],
) -> MethodSpec {
    MethodSpec {
        name: name.into(),
        summary: summary.into(),
        tags: tags.to_vec().into(),
        risk,
        scope: Scope::Connection,
        params: params.to_vec().into(),
        targets: Vec::new().into(),
        // 下面两个字段由 `catalog()` 按方法名/风险推导；这里只是静态表的缺省值。
        effect: Effect::Read,
        record: RecordMode::None,
    }
}

// ── 方法表 ────────────────────────────────────────────────────────────────
//
// op 是层级名（`msg.send`、`group.file.list`、`member.kick`）；同一个顶层路径下的动作成组。
// 每条 op 的 `risk` 在这里显式写出——这是 catalog 的唯一 op→risk 表，不从 tag 推导；
// 不在 `methods()` 里的 op，core 查目录时就拒绝（fail closed）。
// tags 是封闭枚举 `PlatformTag` 的跨类属性：作者写用途，`catalog()` 再补风险/作用范围标签。

fn methods() -> Vec<MethodSpec> {
    vec![
        // ── 账号自己 ──
        cm(
            "account.login_info",
            "这台号登的是谁：QQ 号、昵称",
            &[tags::ACCOUNT],
            Risk::Read,
            &[],
        ),
        cm(
            "account.status",
            "账号在线状态：是否在线、是否正常",
            &[tags::ACCOUNT, tags::SYSTEM],
            Risk::Read,
            &[],
        ),
        cm(
            "account.version",
            "平台实现的版本信息。排查「这个方法到底支不支持」时有用",
            &[tags::SYSTEM],
            Risk::Read,
            &[],
        ),
        // ── 好友 ──
        cm(
            "friend.list",
            "好友列表：QQ 号、昵称、备注",
            &[tags::FRIEND, tags::MEMBER],
            Risk::Read,
            &[],
        ),
        cm(
            "friend.get_info",
            "查一个陌生人/好友的资料：昵称、性别、年龄、签名",
            &[tags::FRIEND, tags::MEMBER],
            Risk::Read,
            &[req("user_id", "QQ 号")],
        ),
        cm(
            "friend.approve",
            "同意或拒绝一个加好友请求。flag 从好友请求事件里拿",
            &[tags::FRIEND, tags::APPROVE],
            Risk::Write,
            &[
                req("flag", "请求的 flag，来自好友请求事件"),
                ParamSpec::optional("approve", ParamType::Bool, "true 同意，false 拒绝"),
                opt("remark", "同意时给他设的备注名"),
            ],
        ),
        cm(
            "friend.delete",
            "删好友。删掉之后对方从列表里消失",
            &[tags::FRIEND, tags::REQUIRES_ADMIN],
            Risk::Destructive,
            &[req("user_id", "要删的 QQ 号")],
        ),
        cm(
            "friend.remark",
            "给好友设备注名。空字符串表示清空备注",
            &[tags::FRIEND, tags::MEMBER],
            Risk::Write,
            &[
                req("user_id", "好友号"),
                req("remark", "备注名；空字符串 = 清空"),
            ],
        ),
        cm(
            "friend.categories",
            "按分组列出好友：分组名和每组里的人",
            &[tags::FRIEND, tags::MEMBER],
            Risk::Read,
            &[],
        ),
        cm(
            "friend.set_category",
            "把好友移到一个分组。categoryId 和 categoryName 至少给一个",
            &[tags::FRIEND],
            Risk::Write,
            &[
                req("uin", "好友号"),
                num("categoryId", "分组 id"),
                opt("categoryName", "分组名；没有现成分组时用它新建"),
            ],
        ),
        cm(
            "friend.one_way",
            "单向好友列表：你加了他、他没加你",
            &[tags::FRIEND, tags::MEMBER],
            Risk::Read,
            &[],
        ),
        cm(
            "friend.doubt_list",
            "可疑的好友申请列表。判定规则由平台决定，core 不猜",
            &[tags::FRIEND, tags::APPROVE],
            Risk::Read,
            &[num("count", "取多少条，默认 50")],
        ),
        cm(
            "friend.doubt_approve",
            "处理一条可疑好友申请。flag 从 friend.doubt_list 拿",
            &[tags::FRIEND, tags::APPROVE],
            Risk::Write,
            &[
                req("flag", "申请 flag，来自 friend.doubt_list"),
                ParamSpec::optional("approve", ParamType::Bool, "true 同意，false 拒绝"),
            ],
        ),
        // ── 收藏表情 / 系统表情 ──
        cm(
            "face.custom_list",
            "列出收藏表情。return_type=url 给图片地址，=id 给 emoji_id；默认 url",
            &[tags::IMAGE, tags::MEDIA, tags::ACCOUNT],
            Risk::Read,
            &[
                num("count", "取多少个，默认 10"),
                opt("return_type", "url 或 id，默认 url"),
            ],
        ),
        cm(
            "face.custom_detail",
            "列出收藏表情的详情：emoji_id、url、md5、描述",
            &[tags::IMAGE, tags::MEDIA, tags::ACCOUNT],
            Risk::Read,
            &[num("count", "取多少个，默认 48")],
        ),
        cm(
            "face.custom_delete",
            "删除一个收藏表情。emoji_id 必须存在于收藏列表；不存在时平台返回错误，不会静默成功",
            &[tags::IMAGE, tags::ACCOUNT],
            Risk::Destructive,
            &[req("emoji_id", "要删的表情 id")],
        ),
        cm(
            "face.custom_modify",
            "改收藏表情的描述。emoji_id 必须存在；desc 为空表示清空描述",
            &[tags::IMAGE, tags::ACCOUNT],
            Risk::Write,
            &[
                req("emoji_id", "收藏表情 id"),
                opt("desc", "新描述；空 = 清空"),
            ],
        ),
        cm(
            "face.system_list",
            "列出系统表情包：包名和每个表情的 id、名称、别名、图片地址",
            &[tags::IMAGE, tags::SYSTEM],
            Risk::Read,
            &[flag("refresh", "true = 强制刷新缓存，默认 false")],
        ),
        cm(
            "face.system_search",
            "搜索系统表情：按编号/名称/别名/分组名匹配。query 不能为空",
            &[tags::IMAGE, tags::SYSTEM],
            Risk::Read,
            &[req("query", "搜索词")],
        ),
        cm(
            "face.entity",
            "按编号查一个系统表情。face_id 不存在时平台返回 null",
            &[tags::IMAGE, tags::SYSTEM],
            Risk::Read,
            &[require(ParamSpec::optional(
                "face_id",
                ParamType::Integer,
                "系统表情编号，>= 0",
            ))],
        ),
        // ── 群：看 ──
        cm(
            "group.list",
            "这台号在哪些群里：群号、群名。想给别的群发东西时先用它拿群号",
            &[tags::GROUP, tags::MEMBER],
            Risk::Read,
            &[],
        ),
        m(
            "group.info",
            "查一个群的资料：群名、人数上限、当前人数、群主",
            &[tags::GROUP],
            Risk::Read,
            &[opt("group_id", "群号；在群里调用可以不填")],
        ),
        m(
            "member.info",
            "查一个群成员的昵称、群名片、角色（群主/管理员/成员）、入群时间",
            &[tags::MEMBER, tags::GROUP],
            Risk::Read,
            &[
                req("user_id", "成员号"),
                opt("group_id", "群号；在群里调用可以不填"),
            ],
        ),
        m(
            "member.list",
            "列出群成员。大群会很长，慎用",
            &[tags::MEMBER, tags::GROUP],
            Risk::Read,
            &[opt("group_id", "群号；在群里调用可以不填")],
        ),
        m(
            "group.honor",
            "群荣誉：龙王、群聊之火、活跃天数榜之类。type 可填 all/talkative/performer/legend",
            &[tags::GROUP, tags::MEMBER],
            Risk::Read,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                opt("type", "荣誉类型，默认 all"),
            ],
        ),
        m(
            "group.system_msg",
            "群系统消息：谁申请加群、谁被邀请进群、哪些还没处理（含处理用的 flag）",
            &[tags::GROUP, tags::APPROVE],
            Risk::Read,
            &[],
        ),
        m(
            "group.notice.get",
            "读群公告：内容、发布者、发布时间",
            &[tags::GROUP, tags::MESSAGE],
            Risk::Read,
            &[opt("group_id", "群号；在群里调用可以不填")],
        ),
        m(
            "group.file.list",
            "列出群文件的根目录：文件名、大小、file_id、busid（下载要这后两个）",
            &[tags::GROUP, tags::FILE],
            Risk::Read,
            &[opt("group_id", "群号；在群里调用可以不填")],
        ),
        m(
            "group.file.folder",
            "列出群文件某个文件夹里的东西",
            &[tags::GROUP, tags::FILE],
            Risk::Read,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("folder_id", "文件夹 id，从 group.file.list 拿"),
            ],
        ),
        m(
            "group.file.url",
            "拿群文件的下载地址（会过期，要用的时候现取）",
            &[tags::GROUP, tags::FILE, tags::DOWNLOAD],
            Risk::Read,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("file_id", "文件 id"),
                num("busid", "业务 id，通常 102；从 group.file.list 拿"),
            ],
        ),
        m(
            "group.file.info",
            "群文件系统概况：文件总数、已用空间",
            &[tags::GROUP, tags::FILE],
            Risk::Read,
            &[opt("group_id", "群号；在群里调用可以不填")],
        ),
        cm(
            "file.private_url",
            "拿私聊文件的下载地址（会过期，要用时现取）。user_id 必须给，避免猜文件属于谁",
            &[tags::FILE, tags::DOWNLOAD],
            Risk::Read,
            &[
                req("user_id", "私聊对方的 QQ 号"),
                req("file_id", "文件 id"),
                opt("file_hash", "文件 hash；平台需要时再给"),
            ],
        ),
        cm(
            "file.info",
            "查平台缓存里的图片/语音文件元数据：本地路径、地址、大小、文件名。\
             file_id 和 file 至少给一个；群文件请用 group.file.url",
            &[tags::FILE, tags::DOWNLOAD],
            Risk::Read,
            &[
                opt("file_id", "缓存文件 id"),
                opt("file", "文件标识；和 file_id 至少给一个"),
            ],
        ),
        m(
            "group.essence.list",
            "群精华消息列表",
            &[tags::GROUP, tags::MESSAGE],
            Risk::Read,
            &[opt("group_id", "群号；在群里调用可以不填")],
        ),
        m(
            "group.at_all_remain",
            "还剩几次 @全体成员 能用（问的是这台号自己）",
            &[tags::GROUP, tags::REQUIRES_ADMIN],
            Risk::Read,
            &[opt("group_id", "群号；在群里调用可以不填")],
        ),
        m(
            "group.admin_settings",
            "读这个群的群管理设置当前值：加群方式、成员权限、邀请策略等",
            &[tags::GROUP, tags::SYSTEM],
            Risk::Read,
            &[opt("group_id", "群号；在群里调用可以不填")],
        ),
        m(
            "group.share_link",
            "拿群的分享/邀请链接",
            &[tags::GROUP],
            Risk::Read,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                flag("need_short_url", "true = 要短链，默认 true"),
                num("src_id", "来源 id，默认 73"),
                opt("additional_param", "附加参数，平台原样带走"),
            ],
        ),
        m(
            "group.detail",
            "群详情：群备注、群等级、全员禁言状态等",
            &[tags::GROUP],
            Risk::Read,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                flag("no_cache", "true = 不走缓存，默认 false"),
            ],
        ),
        m(
            "group.shut_list",
            "群禁言名单：当前被禁言的成员和剩余时间",
            &[tags::GROUP, tags::MUTE],
            Risk::Read,
            &[opt("group_id", "群号；在群里调用可以不填")],
        ),
        m(
            "group.signed_list",
            "今天的群打卡名单",
            &[tags::GROUP, tags::MEMBER],
            Risk::Read,
            &[opt("group_id", "群号；在群里调用可以不填")],
        ),
        m(
            "group.ai_characters",
            "列出这个群可用的 AI 语音角色：角色 id、名字、试听地址",
            &[tags::GROUP, tags::VOICE],
            Risk::Read,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                num("chat_type", "聊天类型，默认 1"),
            ],
        ),
        m(
            "group.ai_voice",
            "生成一段 AI 语音文件，不直接发送。character 从 group.ai_characters 拿；\
             text 不能为空。要发到群里用 group.ai_voice_send",
            &[tags::GROUP, tags::VOICE, tags::MEDIA],
            Risk::Write,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("character", "AI 语音角色 id"),
                req("text", "要合成的文字，不能为空"),
                num("chat_type", "聊天类型，默认 1"),
            ],
        ),
        m(
            "group.ai_voice_send",
            "生成并发送 AI 语音到群里。character 从 group.ai_characters 拿；\
             text 不能为空。这是一条会进流水的语音消息",
            &[tags::GROUP, tags::VOICE, tags::SEND, tags::MEDIA],
            Risk::Write,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("character", "AI 语音角色 id"),
                req("text", "要合成并发送的文字，不能为空"),
                num("chat_type", "聊天类型，默认 1"),
            ],
        ),
        cm(
            "group.ignored_requests",
            "被平台过滤/忽略的加群请求。列出来只读，处理要走 group.approve",
            &[tags::GROUP, tags::APPROVE],
            Risk::Read,
            &[],
        ),
        // ── 群：写 ──
        m(
            "group.notice.send",
            "发群公告。全群都会看到，属于管理员操作",
            &[tags::GROUP, tags::SEND, tags::REQUIRES_ADMIN],
            Risk::Destructive,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("content", "公告正文"),
            ],
        ),
        m(
            "member.card",
            "改群名片。不填 user_id 就是改自己的；改别人的要管理员",
            &[tags::GROUP, tags::MEMBER],
            Risk::Write,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                opt("user_id", "改谁的；不填改自己"),
                opt("card", "新名片；空字符串 = 清空"),
            ],
        ),
        m(
            "group.remark",
            "给群设一个只有自己看得见的备注名",
            &[tags::GROUP],
            Risk::Write,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("remark", "备注名"),
            ],
        ),
        m(
            "group.file.upload",
            "往群文件里上传一个文件。file 可以是本地路径、http 地址或 base64://",
            &[tags::GROUP, tags::FILE, tags::UPLOAD],
            Risk::Write,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("file", "本地路径 / http 地址 / base64://"),
                req("name", "在群里显示的文件名"),
                opt("folder", "放进哪个文件夹（文件夹 id）"),
            ],
        ),
        m(
            "group.essence.set",
            "把一条消息设成群精华",
            &[tags::GROUP, tags::MESSAGE],
            Risk::Write,
            &[req("message_id", "要设成精华的消息 id")],
        ),
        m(
            "group.essence.delete",
            "取消一条消息的群精华",
            &[tags::GROUP, tags::MESSAGE],
            Risk::Write,
            &[req("message_id", "要取消精华的消息 id")],
        ),
        m(
            "group.sign",
            "群签到（有些群开了这个玩法）",
            &[tags::GROUP],
            Risk::Write,
            &[opt("group_id", "群号；在群里调用可以不填")],
        ),
        m(
            "group.approve",
            "同意或拒绝一个加群/邀请请求。flag 和 sub_type 从 group.system_msg 或入群请求事件里拿",
            &[tags::GROUP, tags::APPROVE],
            Risk::Write,
            &[
                req("flag", "请求的 flag"),
                opt("sub_type", "add 或 invite，来自请求事件"),
                ParamSpec::optional("approve", ParamType::Bool, "true 同意，false 拒绝"),
                opt("reason", "拒绝理由"),
            ],
        ),
        // ── 群：管理（破坏性） ──
        m(
            "member.kick",
            "把一个人踢出群。他会被移出，之后能不能再加看 reject_add_request",
            &[tags::GROUP, tags::MEMBER, tags::REQUIRES_ADMIN],
            Risk::Destructive,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("user_id", "踢谁"),
                flag("reject_add_request", "true = 顺便拒绝他再次加群"),
            ],
        ),
        m(
            "group.mute",
            "禁言一个人。duration 是秒，0 = 解除禁言",
            &[tags::GROUP, tags::MEMBER, tags::MUTE, tags::REQUIRES_ADMIN],
            Risk::Destructive,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("user_id", "禁言谁"),
                req("duration", "秒；0 = 解除"),
            ],
        ),
        m(
            "group.mute_all",
            "全员禁言开关。开了之后只有管理员能说话",
            &[tags::GROUP, tags::MUTE, tags::REQUIRES_ADMIN],
            Risk::Destructive,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                ParamSpec::optional("enable", ParamType::Bool, "true 开，false 关"),
            ],
        ),
        m(
            "member.admin",
            "设/撤群管理员",
            &[tags::GROUP, tags::MEMBER, tags::REQUIRES_ADMIN],
            Risk::Destructive,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("user_id", "谁"),
                ParamSpec::optional("enable", ParamType::Bool, "true 设，false 撤"),
            ],
        ),
        m(
            "group.name",
            "改群名。全群可见",
            &[tags::GROUP, tags::REQUIRES_ADMIN],
            Risk::Destructive,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("group_name", "新群名"),
            ],
        ),
        m(
            "member.title",
            "设/撤群成员的专属头衔（群主才能设）",
            &[tags::GROUP, tags::MEMBER, tags::REQUIRES_ADMIN],
            Risk::Destructive,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("user_id", "谁"),
                opt("special_title", "头衔；空 = 撤销"),
            ],
        ),
        m(
            "group.avatar",
            "换群头像。file 可以是本地路径、http 地址或 base64://",
            &[tags::GROUP, tags::REQUIRES_ADMIN, tags::IMAGE],
            Risk::Destructive,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("file", "图片：本地路径 / http 地址 / base64://"),
            ],
        ),
        m(
            "group.leave",
            "退出群。is_dismiss 只有群主能用来解散",
            &[tags::GROUP, tags::REQUIRES_ADMIN],
            Risk::Destructive,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                flag("is_dismiss", "true = 解散（仅群主）"),
            ],
        ),
        m(
            "group.file.delete",
            "删群文件。搞不好是别人传的，删了就没了",
            &[tags::GROUP, tags::FILE],
            Risk::Destructive,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("file_id", "文件 id"),
                num("busid", "业务 id，通常 102"),
            ],
        ),
        m(
            "group.file.folder_create",
            "在群文件里建一个文件夹。name 不能为空；parent_id 默认根目录 \"/\"",
            &[tags::GROUP, tags::FILE],
            Risk::Write,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("name", "文件夹名，不能为空"),
                opt("parent_id", "父文件夹 id，默认 \"/\""),
            ],
        ),
        m(
            "group.file.folder_delete",
            "删除一个群文件夹。folder_id 必须存在；不存在或平台拒绝时返回错误，不会静默成功。\
             非空文件夹是否允许删除由平台决定，core 不预判",
            &[tags::GROUP, tags::FILE],
            Risk::Destructive,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("folder_id", "要删的文件夹 id"),
            ],
        ),
        m(
            "group.file.move",
            "把一个群文件移到另一个文件夹。file_id 必须存在于 parent_directory，\
             target_directory 必须已存在；移动失败会返回错误，不静默成功",
            &[tags::GROUP, tags::FILE],
            Risk::Write,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("file_id", "文件 id"),
                req("parent_directory", "文件当前所在目录"),
                req("target_directory", "目标目录"),
            ],
        ),
        m(
            "group.file.rename",
            "重命名一个群文件。file_id 必须存在于 current_parent_directory；失败会返回错误",
            &[tags::GROUP, tags::FILE],
            Risk::Write,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("file_id", "文件 id"),
                opt("current_parent_directory", "文件当前目录，默认 \"/\""),
                req("new_name", "新文件名"),
            ],
        ),
        m(
            "group.member_permissions",
            "设置普通群成员的权限：上传群相册、发起临时会话、建群。仅群主",
            &[tags::GROUP, tags::MEMBER, tags::REQUIRES_ADMIN],
            Risk::Destructive,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                flag("allow_member_upload_album", "成员能否上传群相册"),
                flag("allow_member_temporary_session", "成员能否发起临时会话"),
                flag("allow_member_create_group", "成员能否建群"),
            ],
        ),
        m(
            "group.invite_policy",
            "设置谁能邀请新成员入群",
            &[tags::GROUP, tags::MEMBER, tags::REQUIRES_ADMIN],
            Risk::Destructive,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                choices(
                    "policy",
                    "邀请策略",
                    &[
                        "disabled",
                        "require_approval",
                        "no_approval",
                        "no_approval_under_100",
                    ],
                ),
            ],
        ),
        m(
            "group.history_visibility",
            "设置新成员能否看到入群前的历史消息",
            &[tags::GROUP, tags::REQUIRES_ADMIN],
            Risk::Destructive,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                require(ParamSpec::optional(
                    "visible",
                    ParamType::Bool,
                    "true 可见，false 不可见",
                )),
            ],
        ),
        m(
            "member.kick_many",
            "一次踢掉多个群成员。一次最多 20 个，适合清机器人/清广告",
            &[tags::GROUP, tags::MEMBER, tags::REQUIRES_ADMIN],
            Risk::Destructive,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                any("user_ids", "QQ 号数组，一次最多 20 个，如 [10001, 10002]"),
                flag("reject_add_request", "true = 顺便拒绝他们再次加群"),
            ],
        ),
        m(
            "group.notice.delete",
            "删除一条群公告。fid 和 notice_id 至少给一个",
            &[tags::GROUP, tags::MESSAGE, tags::REQUIRES_ADMIN],
            Risk::Destructive,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                opt("fid", "公告 fid，来自 group.notice.get"),
                opt("notice_id", "公告 id，来自 group.notice.get"),
            ],
        ),
        // ── 消息 ──
        m(
            "msg.send",
            "往当前房间发消息（群聊或私聊由 room 决定）。message 是消息段数组，文本、图片、语音、视频、文件、@ 都走这一个；\
         例：[{\"type\":\"text\",\"text\":\"看这个\"},{\"type\":\"image\",\"file\":\"https://…\"}]。\
         配合 msg.send_forward 还能发合并转发",
            &[
                tags::SEND,
                tags::MESSAGE,
                tags::MEDIA,
                tags::IMAGE,
                tags::VOICE,
                tags::VIDEO,
                tags::FILE,
            ],
            Risk::Write,
            &[any(
                "message",
                "消息段数组，如 [{\"type\":\"text\",\"text\":\"hi\"}]",
            )],
        ),
        m(
            "msg.face",
            "发送当前账号收藏里的自定义表情。emoji_id 是完整表情 id 或 32 位 MD5",
            &[tags::SEND, tags::MESSAGE, tags::IMAGE],
            Risk::Write,
            &[
                req("emoji_id", "表情 id：完整 id 或 32 位 MD5"),
                opt("reply_to", "引用哪条消息（消息 id）；不填 = 不引用"),
            ],
        ),
        m(
            "msg.send_forward",
            "发合并转发（那种「聊天记录」卡片）。messages 是节点数组：\
         [{\"type\":\"node\",\"data\":{\"name\":\"昵称\",\"uin\":\"QQ号\",\"content\":[消息段]}}]",
            &[tags::SEND, tags::MESSAGE, tags::FORWARD],
            Risk::Write,
            &[any("messages", "转发节点数组")],
        ),
        m(
            "msg.get",
            "按消息 id 取一条消息的原始内容（含发送者、消息段）",
            &[tags::MESSAGE, tags::HISTORY],
            Risk::Read,
            &[req(
                "message_id",
                "平台消息 id，就是消息里 quote 段的 message_id",
            )],
        ),
        m(
            "msg.recall",
            "撤回一条消息。撤别人的要管理员，撤自己的随时可以",
            &[tags::MESSAGE, tags::RECALL],
            Risk::Destructive,
            &[req("message_id", "要撤回的消息 id")],
        ),
        m(
            "msg.history",
            "翻当前房间的消息历史。message_seq 是起点序号，往前取 count 条",
            &[tags::MESSAGE, tags::HISTORY],
            Risk::Read,
            &[
                num("message_seq", "从这条的序号开始往前"),
                num("count", "取多少条，默认 20"),
            ],
        ),
        m(
            "msg.forward",
            "读一条合并转发的正文，一次给到最里层。返回摊平的节点表：每条的 path（如 1.0 表示\
         第二条第 0 个回复）、depth（第几层）、sender、text；最外层还有 leaf_text 一句话\
         概括全部内容。默认展开 3 层，最多 5 层。节点多到上限时 has_more 为真，\
         把 depth 调大或按某个节点的 forward_id 再调一次即可",
            &[tags::FORWARD, tags::MESSAGE],
            Risk::Read,
            &[
                req("id", "转发 id，就是消息里 forward 段的 data.id"),
                num("depth", "展开几层，默认 3、最多 5。只想要外层就给 1"),
                num("max_nodes", "最多摊多少条节点，默认 200、最多 500"),
            ],
        ),
        m(
            "msg.image",
            "拿到一个图片段的真实文件：本地路径或下载地址",
            &[tags::IMAGE, tags::MEDIA, tags::DOWNLOAD],
            Risk::Read,
            &[req("file", "图片段的 file 字段")],
        ),
        m(
            "msg.voice",
            "把语音转成能用的格式，拿到文件路径。out_format 可填 mp3/amr/wav/silk",
            &[tags::VOICE, tags::MEDIA, tags::DOWNLOAD],
            Risk::Read,
            &[
                req("file", "语音段的 file 字段"),
                opt("out_format", "输出格式，默认 mp3"),
            ],
        ),
        m(
            "msg.voice_text",
            "把一条语音消息转成文字。message_id 必须是一条语音消息；转不出来时平台会报错",
            &[tags::VOICE, tags::MEDIA, tags::MESSAGE],
            Risk::Read,
            &[req("message_id", "语音消息 id")],
        ),
        m(
            "msg.ocr",
            "识别图片里的文字。image 给图片 URL 或已缓存图片的 file_id；识别不了时平台会报错",
            &[tags::IMAGE, tags::MEDIA],
            Risk::Read,
            &[req("image", "图片 URL 或缓存 file_id")],
        ),
        m(
            "msg.react",
            "给一条消息加表情回应（那个小表情）。emoji_id 是平台的表情 id",
            &[tags::MESSAGE, tags::IMAGE],
            Risk::Write,
            &[
                req("message_id", "哪条消息"),
                req("emoji_id", "表情 id"),
                flag("set", "true 加，false 取消"),
            ],
        ),
        m(
            "msg.reactions",
            "读一条消息上的表情回应汇总：每个表情的 id、类型、数量、最后回应时间",
            &[tags::MESSAGE, tags::IMAGE],
            Risk::Read,
            &[req("message_id", "消息 id")],
        ),
        m(
            "msg.reaction_users",
            "读某个表情回应下的用户列表。emojiId 必须在该消息的回应里，先用 msg.reactions 查；\
             用 count / cookie 翻页",
            &[tags::MESSAGE, tags::IMAGE],
            Risk::Read,
            &[
                req("message_id", "消息 id"),
                req("emojiId", "表情 id（参数名就是 emojiId）"),
                num("count", "取多少个用户，默认 10"),
                opt("cookie", "翻页 cookie，从上一次结果拿"),
            ],
        ),
        m(
            "group.reaction",
            "给群里的一条消息加/取消表情回应。message_id 必须存在；code 是平台定义的表情 code",
            &[tags::GROUP, tags::MESSAGE, tags::IMAGE],
            Risk::Write,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("message_id", "消息 id"),
                req("code", "平台表情 code"),
                flag("is_set", "true 加，false 取消，默认 true"),
            ],
        ),
        m(
            "msg.mark_read",
            "把消息标成已读",
            &[tags::MESSAGE],
            Risk::Write,
            &[req("message_id", "标到哪条")],
        ),
        // ── 互动 ──
        m(
            "msg.poke",
            "拍一拍。群里要指定拍谁；私聊里不填就是拍对方",
            &[tags::POKE, tags::SEND, tags::MEMBER],
            Risk::Write,
            &[
                opt("user_id", "拍谁；群里必须给，私聊里不填就是拍对方"),
                opt("group_id", "在哪个群拍；在群里调用可以不填"),
            ],
        ),
        cm(
            "like.send",
            "给一个人的资料卡点赞（那个「赞」）。times 最多 10",
            &[tags::MEMBER, tags::SEND],
            Risk::Write,
            &[
                req("user_id", "给谁点赞"),
                num("times", "点几次，默认 1，最多 10"),
            ],
        ),
        // ── 其它 ──
        cm(
            "system.check_url",
            "问平台一个链接安不安全（腾讯自家的安全接口）。拿不准某个链接时先问一下",
            &[tags::SYSTEM],
            Risk::Read,
            &[req("url", "要检查的链接")],
        ),
    ]
}

// ── 路由 ──────────────────────────────────────────────────────────────────
//
// 方法名 → OneBot 动作。`FROM_GROUP` / `FROM_PRIVATE` 说这个参数能从房间推出来：
// 在群里调就不用重复填群号，在私聊里调就不用填对方 QQ。

/// 调哪个动作。
pub enum Target {
    /// 一个方法对一个动作。
    Fixed(&'static str),
    /// 群聊一个、私聊一个（拍一拍、发消息这两类平台分成两个接口）。
    ByRoom {
        group: &'static str,
        private: &'static str,
    },
}

/// 一条路由：方法名 → 平台动作。
pub struct Route {
    pub method: &'static str,
    pub target: Target,
    /// 在群房间里，这个参数缺了就用房间的群号补。
    pub from_group: Option<&'static str>,
    /// 在私聊房间里，这个参数缺了就用房间的对方号补。
    pub from_private: Option<&'static str>,
}

const fn route(method: &'static str, action: &'static str) -> Route {
    Route {
        method,
        target: Target::Fixed(action),
        from_group: None,
        from_private: None,
    }
}

/// 房间目标参数的声明。core 用它做“不许在 params 里换房间”的兜底校验。
const GROUP_ROOM: &[TargetSpec] = &[TargetSpec::room("group_id", "group")];
const PRIVATE_ROOM: &[TargetSpec] = &[TargetSpec::room("user_id", "private")];
const BOTH_ROOMS: &[TargetSpec] = &[
    TargetSpec::room("group_id", "group"),
    TargetSpec::room("user_id", "private"),
];
/// poke 在群里 `user_id` 是“拍谁”（成员目标），`group_id` 才是房间目标；
/// 在私聊里 `user_id` 又变成房间目标。
const POKE_TARGETS: &[TargetSpec] = &[
    TargetSpec::room("group_id", "group"),
    TargetSpec::member("user_id", "group"),
    TargetSpec::room("user_id", "private"),
];

/// 群参数 `group_id` 由房间补上。
const fn group_route(method: &'static str, action: &'static str) -> Route {
    Route {
        method,
        target: Target::Fixed(action),
        from_group: Some("group_id"),
        from_private: None,
    }
}

/// 群聊/私聊同一个动作、两边都可能从房间推导目标参数（如合并转发）。
const fn both_route(method: &'static str, action: &'static str) -> Route {
    Route {
        method,
        target: Target::Fixed(action),
        from_group: Some("group_id"),
        from_private: Some("user_id"),
    }
}

/// 群聊/私聊两个动作，目标参数从房间推导（发消息、历史、拍一拍）。
const fn room_route(method: &'static str, group: &'static str, private: &'static str) -> Route {
    Route {
        method,
        target: Target::ByRoom { group, private },
        from_group: Some("group_id"),
        from_private: Some("user_id"),
    }
}

pub static ROUTES: &[Route] = &[
    route("account.login_info", "get_login_info"),
    route("account.status", "get_status"),
    route("account.version", "get_version_info"),
    route("friend.list", "get_friend_list"),
    route("friend.get_info", "get_stranger_info"),
    route("friend.approve", "set_friend_add_request"),
    route("friend.delete", "delete_friend"),
    route("friend.remark", "set_friend_remark"),
    route("friend.categories", "get_friends_with_category"),
    route("friend.set_category", "set_friends_category"),
    route("friend.one_way", "get_unidirectional_friend_list"),
    route("friend.doubt_list", "get_doubt_friends_add_request"),
    route("friend.doubt_approve", "set_doubt_friends_add_request"),
    route("face.custom_list", "fetch_custom_face"),
    route("face.custom_detail", "fetch_custom_face_detail"),
    route("face.custom_delete", "delete_custom_face"),
    route("face.custom_modify", "modify_custom_face"),
    route("face.system_list", "fetch_sys_faces"),
    route("face.system_search", "search_sys_faces"),
    route("face.entity", "fetch_face_entity"),
    route("group.list", "get_group_list"),
    group_route("group.info", "get_group_info"),
    group_route("member.info", "get_group_member_info"),
    group_route("member.list", "get_group_member_list"),
    group_route("group.honor", "get_group_honor_info"),
    route("group.system_msg", "get_group_system_msg"),
    route("group.ignored_requests", "get_group_ignored_notifies"),
    group_route("group.notice.get", "_get_group_notice"),
    group_route("group.admin_settings", "get_group_admin_settings"),
    group_route("group.share_link", "get_group_share_link"),
    group_route("group.detail", "get_group_detail_info"),
    group_route("group.shut_list", "get_group_shut_list"),
    group_route("group.signed_list", "get_group_signed_list"),
    group_route("group.ai_characters", "get_ai_characters"),
    group_route("group.ai_voice", "get_ai_record"),
    group_route("group.ai_voice_send", "send_group_ai_record"),
    group_route("group.notice.delete", "_del_group_notice"),
    group_route("group.file.list", "get_group_root_files"),
    group_route("group.file.folder", "get_group_files_by_folder"),
    group_route("group.file.url", "get_group_file_url"),
    group_route("group.file.info", "get_group_file_system_info"),
    route("file.private_url", "get_private_file_url"),
    route("file.info", "get_file"),
    group_route("group.essence.list", "get_essence_msg_list"),
    group_route("group.at_all_remain", "get_group_at_all_remain"),
    group_route("group.notice.send", "_send_group_notice"),
    group_route("member.card", "set_group_card"),
    group_route("group.remark", "set_group_remark"),
    group_route("group.file.upload", "upload_group_file"),
    route("group.essence.set", "set_essence_msg"),
    route("group.essence.delete", "delete_essence_msg"),
    group_route("group.sign", "set_group_sign"),
    route("group.approve", "set_group_add_request"),
    group_route("member.kick", "set_group_kick"),
    group_route("member.kick_many", "set_group_kick_members"),
    group_route("group.mute", "set_group_ban"),
    group_route("group.mute_all", "set_group_whole_ban"),
    group_route("member.admin", "set_group_admin"),
    group_route("group.member_permissions", "set_group_member_permissions"),
    group_route("group.invite_policy", "set_group_member_invite_policy"),
    group_route(
        "group.history_visibility",
        "set_group_new_member_history_visibility",
    ),
    group_route("group.name", "set_group_name"),
    group_route("member.title", "set_group_special_title"),
    group_route("group.avatar", "set_group_portrait"),
    group_route("group.leave", "set_group_leave"),
    group_route("group.file.delete", "delete_group_file"),
    group_route("group.file.folder_create", "create_group_file_folder"),
    group_route("group.file.folder_delete", "delete_group_file_folder"),
    group_route("group.file.move", "move_group_file"),
    group_route("group.file.rename", "rename_group_file"),
    room_route("msg.send", "send_group_msg", "send_private_msg"),
    both_route("msg.face", "send_custom_face"),
    both_route("msg.send_forward", "send_forward_msg"),
    route("msg.get", "get_msg"),
    route("msg.recall", "delete_msg"),
    room_route(
        "msg.history",
        "get_group_msg_history",
        "get_friend_msg_history",
    ),
    route("msg.forward", "get_forward_msg"),
    route("msg.image", "get_image"),
    route("msg.voice", "get_record"),
    route("msg.voice_text", "fetch_ptt_text"),
    route("msg.ocr", "ocr_image"),
    route("msg.react", "set_msg_emoji_like"),
    route("msg.reactions", "get_msg_emoji_likes"),
    route("msg.reaction_users", "fetch_emoji_like"),
    group_route("group.reaction", "set_group_reaction"),
    route("msg.mark_read", "mark_msg_as_read"),
    room_route("msg.poke", "group_poke", "friend_poke"),
    route("like.send", "send_like"),
    route("system.check_url", "check_url_safely"),
];

/// 方法表 + 从路由推导出的目标参数声明。
///
/// 方法表在第一次访问时构造并缓存成 owned 的 `Arc<[MethodSpec]>`；目标声明、
/// effect / record 从路由和风险推导，路由表仍然是唯一真相。
pub fn catalog() -> std::sync::Arc<[MethodSpec]> {
    static CATALOG: OnceLock<std::sync::Arc<[MethodSpec]>> = OnceLock::new();
    CATALOG
        .get_or_init(|| {
            methods()
                .into_iter()
                .map(|method| {
                    let effect = effect_of(&method);
                    MethodSpec {
                        tags: tags_for(&method).into(),
                        targets: targets_of(method.name.as_ref()).into(),
                        effect,
                        record: record_of(method.name.as_ref()),
                        ..method
                    }
                })
                .collect::<Vec<_>>()
                .into()
        })
        .clone()
}

/// 常用 op：清单里优先展示，模型最常走的几条路径。
const COMMON_OPS: &[&str] = &[
    "account.login_info",
    "friend.list",
    "group.list",
    "group.info",
    "member.list",
    "member.info",
    "msg.send",
    "msg.history",
    "msg.get",
    "msg.recall",
    "msg.image",
    "msg.voice",
    "msg.react",
];

/// 实现特有、其它 OneBot 实现不保证的 op。
const SNOWLUMA_ONLY_OPS: &[&str] = &["group.sign", "like.send", "system.check_url"];

fn push_tag(tags: &mut Vec<PlatformTag>, tag: PlatformTag) {
    if !tags.contains(&tag) {
        tags.push(tag);
    }
}

/// 标签由「作者声明的用途」+「风险/作用范围自动标签」组成。
/// 风险与作用范围始终来自 `MethodSpec.risk` / `scope`，不是从标签反推的。
fn tags_for(method: &MethodSpec) -> Vec<PlatformTag> {
    let mut tags = method.tags.to_vec();
    push_tag(
        &mut tags,
        match method.risk {
            Risk::Read => tags::READ,
            Risk::Write => tags::WRITE,
            Risk::Destructive => tags::DESTRUCTIVE,
        },
    );
    push_tag(
        &mut tags,
        match method.scope {
            Scope::Room => tags::ROOM,
            Scope::Connection => tags::CONNECTION,
        },
    );
    if method.risk == Risk::Read {
        push_tag(&mut tags, tags::IDEMPOTENT);
    }
    if (method.risk == Risk::Destructive && matches!(method.group(), "group" | "member"))
        || method.name.as_ref() == "msg.recall"
    {
        push_tag(&mut tags, tags::REQUIRES_ADMIN);
    }
    if COMMON_OPS.contains(&method.name.as_ref()) {
        push_tag(&mut tags, tags::COMMON);
    }
    if SNOWLUMA_ONLY_OPS.contains(&method.name.as_ref()) {
        push_tag(&mut tags, tags::SNOWLUMA_ONLY);
    }
    tags
}

/// 这个方法做什么。发送/撤回/删除按语义认，其余按风险认。
fn effect_of(method: &MethodSpec) -> Effect {
    if method.has_tag(tags::RECALL) || DELETE_METHODS.contains(&method.name.as_ref()) {
        Effect::Delete
    } else if method.has_tag(tags::SEND) {
        Effect::Send
    } else {
        match method.risk {
            Risk::Read => Effect::Read,
            Risk::Write | Risk::Destructive => Effect::Write,
        }
    }
}

/// 名字里带 delete 但没挂 `recall` 标签的几个。
const DELETE_METHODS: &[&str] = &[
    "friend.delete",
    "group.file.delete",
    "group.file.folder_delete",
    "group.essence.delete",
    "group.notice.delete",
    "face.custom_delete",
];

/// 调用成功后要写进流水的那几个。core 靠它判断“这次调用会不会改变房间”。
fn record_of(method: &str) -> RecordMode {
    match method {
        "msg.send" | "msg.face" | "group.ai_voice_send" => RecordMode::Message,
        "msg.send_forward" | "group.notice.send" => RecordMode::Event,
        _ => RecordMode::None,
    }
}

fn targets_of(method: &str) -> &'static [TargetSpec] {
    if method == "msg.poke" {
        return POKE_TARGETS;
    }
    let Some(route) = ROUTES.iter().find(|route| route.method == method) else {
        return &[];
    };
    match (route.from_group, route.from_private) {
        (Some(_), Some(_)) => BOTH_ROOMS,
        (Some(_), None) => GROUP_ROOM,
        (None, Some(_)) => PRIVATE_ROOM,
        (None, None) => &[],
    }
}
