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
    Verification, outbound::tags,
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

/// 形状不固定、可选的参数（转发节点数组）。
fn maybe_any(name: &'static str, desc: &'static str) -> ParamSpec {
    let param = ParamSpec::maybe_any(name, desc);
    match name {
        "messages" => param.with_example(serde_json::json!([{
            "type": "node",
            "data": {
                "name": "某人",
                "uin": "10000",
                "content": [{ "type": "text", "text": "你好" }],
            }
        }])),
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

/// 可选枚举字符串。
fn pick(name: &'static str, desc: &'static str, values: &'static [&'static str]) -> ParamSpec {
    let mut param = choices(name, desc, values);
    param.required = false;
    param
}

/// 必填整数。
fn int_req(name: &'static str, desc: &'static str) -> ParamSpec {
    let mut param = ParamSpec::optional(name, ParamType::Integer, desc);
    param.required = true;
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
        // 出处由 `catalog()` 按方法名补齐；这里只是构造期必填的占位。
        verification: Verification::verified(DEFAULT_SOURCE),
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
        // 出处由 `catalog()` 按方法名补齐；这里只是构造期必填的占位。
        verification: Verification::verified(DEFAULT_SOURCE),
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
            "face.custom_add",
            "把一张图片加进收藏表情，返回 emoji_id。file 接受服务端本地路径、\
             file://、http(s)://、base64://、data:...;base64,...；裸 base64 不支持。\
             两段式：先加进收藏，再用返回的 emoji_id 调 msg.face 发送",
            &[tags::IMAGE, tags::MEDIA, tags::ACCOUNT, tags::UPLOAD],
            Risk::Write,
            &[req(
                "file",
                "图片源：路径 / file:// / http(s):// / base64:// / data URI",
            )],
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
        cm(
            "file.share_link",
            "拿一个闪传文件集（fileset）的分享链接。只接受 fileset_id，不是群 file_id；\
             fileset_id 从 create_flash_task / list_filesets / get_fileset_info / \
             get_fileset_id 获取。平台可能返回空字符串",
            &[tags::FILE],
            Risk::Read,
            &[req("fileset_id", "闪传文件集 id")],
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
        // 群分类标签是分类级（group_class）不是群级：协议请求体里没有 group_id，
        // 不能从群号推导。三个 op 都要求显式 group_class。
        cm(
            "group.tag_list",
            "按分类 ID 列出群分类标签目录。group_class 是分类 ID（0 也合法），\
             不是群号；平台没有 群号 → 分类 的查询接口",
            &[tags::GROUP, tags::SYSTEM],
            Risk::Read,
            &[int_req("group_class", "分类 ID（uint32，0 也是合法值）")],
        ),
        cm(
            "group.tag_add",
            "往一个群分类目录里加标签。group_class 是分类 ID，不是群号；\
             name 是标签名。是否影响具体群的资料需按实验确认",
            &[tags::GROUP, tags::SYSTEM],
            Risk::Write,
            &[
                int_req("group_class", "分类 ID（uint32）"),
                req("name", "标签名，不能为空"),
            ],
        ),
        cm(
            "group.tag_delete",
            "从一个群分类目录里删标签。group_class 是分类 ID，不是群号",
            &[tags::GROUP, tags::SYSTEM],
            Risk::Destructive,
            &[
                int_req("group_class", "分类 ID（uint32）"),
                req("name", "要删的标签名，不能为空"),
            ],
        ),
        m(
            "group.add_option",
            "设置加群方式。add_type 必须显式给：0 空设置、1 任何人都能进、\
             2 需要管理员批准、3 不允许任何人、4 问题+答案、5 问题+管理员批准；\
             55 是 5 的别名（适配器归一化为 5）。只有 4/5 会带 question/answer",
            &[tags::GROUP, tags::SYSTEM, tags::REQUIRES_ADMIN],
            Risk::Write,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                int_req("add_type", "0-5（55 视为 5）；不允许省略"),
                opt("question", "问题进入（add_type=4/5）的问题正文"),
                opt("answer", "答案进入（add_type=4）的答案"),
            ],
        ),
        m(
            "group.search",
            "设置群的搜索开关。name_search / code_search 用 open / close 枚举；\
             只传一个也可以，不传的字段保持不变",
            &[tags::GROUP, tags::SYSTEM, tags::REQUIRES_ADMIN],
            Risk::Write,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                pick(
                    "name_search",
                    "按群名/关键词搜索：open 开启，close 关闭",
                    &["open", "close"],
                ),
                pick(
                    "code_search",
                    "按群号搜索：open 开启，close 关闭",
                    &["open", "close"],
                ),
            ],
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
            "group.album.list",
            "列出群相册：相册 id、名字、照片数、封面、最后上传时间",
            &[tags::GROUP, tags::IMAGE],
            Risk::Read,
            &[opt("group_id", "群号；在群里调用可以不填")],
        ),
        m(
            "group.album.media",
            "列出群相册里的照片/视频。album_id 必须存在；attach_info 用来翻页，\
             把上一次返回的 nextAttachInfo 原样带回来",
            &[tags::GROUP, tags::IMAGE, tags::MEDIA],
            Risk::Read,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("album_id", "相册 id，从 group.album.list 拿"),
                opt("attach_info", "翻页游标，原样带回上一次的 nextAttachInfo"),
            ],
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
            "group.file.folder_rename",
            "重命名一个群文件夹。folder_id 必填；new_folder_name 优先，name 是平台\
             兼容别名（前者为空时回退到 name），两者都不给会被本地拒绝",
            &[tags::GROUP, tags::FILE],
            Risk::Write,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("folder_id", "要改名的文件夹 id"),
                opt("new_folder_name", "新文件夹名（优先）"),
                opt("name", "新文件夹名（new_folder_name 为空时的兼容别名）"),
            ],
        ),
        m(
            "group.file.trans",
            "把群临时文件转存到同群的永久文件存储。只有 group_id + file_id，\
             没有目标群参数；语义不是跨群转存",
            &[tags::GROUP, tags::FILE],
            Risk::Write,
            &[
                opt("group_id", "群号；在群里调用可以不填"),
                req("file_id", "临时文件 id"),
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
        // ── 账号凭据 / 进程运维（critical：只能走票据 + 审批） ──
        cm(
            "account.credentials",
            "获取账号 Cookie、token、csrf_token。凭据类结果只走票据查询，不进流水；\
             必须由 access_request 批准后执行",
            &[tags::ACCOUNT, tags::SYSTEM],
            Risk::Critical,
            &[opt(
                "domain",
                "Cookie 所属域名；不填由平台默认（qun.qq.com）",
            )],
        ),
        cm(
            "account.cookies",
            "获取指定域名的 Cookie 字符串。凭据类结果只走票据查询，不进流水；\
             必须由 access_request 批准后执行",
            &[tags::ACCOUNT, tags::SYSTEM],
            Risk::Critical,
            &[opt(
                "domain",
                "Cookie 所属域名；不填由平台默认（qun.qq.com）",
            )],
        ),
        cm(
            "account.csrf_token",
            "获取指定域名的 CSRF 令牌。凭据类结果只走票据查询，不进流水；\
             必须由 access_request 批准后执行",
            &[tags::ACCOUNT, tags::SYSTEM],
            Risk::Critical,
            &[opt(
                "domain",
                "Cookie 所属域名；不填由平台默认（qun.qq.com）",
            )],
        ),
        cm(
            "account.clientkey",
            "获取 clientkey、过期时间与索引。凭据类结果只走票据查询，不进流水；\
             必须由 access_request 批准后执行",
            &[tags::ACCOUNT, tags::SYSTEM],
            Risk::Critical,
            &[],
        ),
        cm(
            "account.decrypt_key",
            "请求本地数据库解密密钥。密钥只走票据查询，不进流水；\
             必须由 access_request 批准后执行",
            &[tags::ACCOUNT, tags::SYSTEM],
            Risk::Critical,
            &[req("db_path", "要解密的数据库文件路径")],
        ),
        cm(
            "system.send_packet",
            "发送原始 SSO 包（cmd + hex data）。绕过 OneBot 语义，结果只走票据查询；\
             必须由 access_request 批准后执行",
            &[tags::SYSTEM],
            Risk::Critical,
            &[
                req("cmd", "SSO 命令名"),
                opt("data", "hex 编码的包体；不填为空包"),
                flag("rsp", "是否等待平台回包，默认 true"),
            ],
        ),
        cm(
            "system.bot_exit",
            "让平台进程退出。会中断这条连接；必须由 access_request 批准后执行",
            &[tags::SYSTEM],
            Risk::Critical,
            &[],
        ),
        cm(
            "system.restart",
            "请求平台重启。SnowLuma 文档标注该动作不支持；必须由 access_request 批准后执行",
            &[tags::SYSTEM],
            Risk::Critical,
            &[],
        ),
        cm(
            "system.clean_cache",
            "清理平台缓存。可能影响平台侧媒体缓存；必须由 access_request 批准后执行",
            &[tags::SYSTEM],
            Risk::Critical,
            &[],
        ),
        // ── 文件下载 / 流式 ──
        cm(
            "file.download",
            "把 http(s) URL 或 base64 下载到平台侧 data/downloads，只返回平台路径与元信息，\
             不回传文件字节。url 必须是 http/https；max_bytes 默认 8 MiB（仅对 base64 和\
             本地上限生效；平台是否跟随重定向不可控）",
            &[tags::FILE, tags::DOWNLOAD],
            Risk::Destructive,
            &[
                opt("url", "http/https 下载地址"),
                opt("base64", "base64 内容；与 url 二选一"),
                opt("name", "保存文件名；不填由平台推断"),
                num("max_bytes", "ACB 侧大小上限（字节），默认 8 MiB"),
            ],
        ),
        cm(
            "stream.upload_file",
            "流式分块上传：每次调用发一块并等一块确认（type=stream），最后一块带 \
             is_complete=true 得到 type=response。协议无逐请求取消，未完成的分块由平台\
             空闲超时清理；ACB 限制单块 1 MiB、总大小 256 MiB",
            &[tags::FILE, tags::UPLOAD],
            Risk::Write,
            &[
                req("stream_id", "客户端生成的流 ID，限 [A-Za-z0-9_-]"),
                opt("chunk_data", "本块 base64 数据"),
                num("chunk_index", "分块索引，从 0 开始"),
                num("total_chunks", "总分块数；新流必填"),
                num("file_size", "文件总大小（字节）"),
                opt("expected_sha256", "期望的整文件 SHA256"),
                flag("is_complete", "是否最后一块 / 触发合并"),
                opt("filename", "文件名"),
                flag("reset", "丢弃该流"),
                flag("verify_only", "只查当前流状态"),
                num("file_retention", "合并文件保留毫秒；0 = 不回收"),
            ],
        ),
        cm(
            "stream.download_image",
            "以流式方式下载图片。首帧 file_info 会返回，后续 file_chunk 在当前 WS 单响应\
             语义下不会进入 core；要完整文件请用 HTTP/SDK。无逐请求取消、WS 无背压",
            &[tags::FILE, tags::DOWNLOAD, tags::IMAGE, tags::MEDIA],
            Risk::Read,
            &[
                opt("file", "stream 临时目录路径或 http(s) URL"),
                opt("file_id", "缓存的图片 id"),
                num("chunk_size", "分块字节数，默认 64 KiB"),
            ],
        ),
        cm(
            "stream.download_record",
            "以流式方式下载语音。首帧 file_info 会返回，后续 file_chunk 在当前 WS 单响应\
             语义下不会进入 core；要完整文件请用 HTTP/SDK。无逐请求取消、WS 无背压",
            &[tags::FILE, tags::DOWNLOAD, tags::VOICE, tags::MEDIA],
            Risk::Read,
            &[
                opt("file", "stream 临时目录路径或 http(s) URL"),
                opt("file_id", "缓存的语音 id"),
                num("chunk_size", "分块字节数，默认 64 KiB"),
            ],
        ),
        cm(
            "stream.download_file",
            "以流式方式下载任意文件。首帧 file_info 会返回，后续 file_chunk 在当前 WS \
             单响应语义下不会进入 core；要完整文件请用 HTTP/SDK。无逐请求取消、WS 无背压",
            &[tags::FILE, tags::DOWNLOAD, tags::MEDIA],
            Risk::Read,
            &[
                opt("file", "stream 临时目录路径或 http(s) URL"),
                opt("file_id", "缓存的媒体 id"),
                num("chunk_size", "分块字节数，默认 64 KiB"),
            ],
        ),
        cm(
            "stream.test_download",
            "测试下载流：平台推 10 个数据帧，不触达 QQ。当前 WS 单响应语义只拿到首帧；\
             完整多帧需要 HTTP/SDK",
            &[tags::SYSTEM, tags::FILE],
            Risk::Read,
            &[flag("error", "true 时用错误帧结束，默认 false")],
        ),
        cm(
            "stream.clean",
            "清理流式上传/下载临时目录，返回 removed / freed_bytes / skipped_active。\
             只删 stream 目录，不碰其它缓存",
            &[tags::FILE, tags::SYSTEM],
            Risk::Destructive,
            &[],
        ),
        // ── 合并转发预上传 / 专用发送别名 ──
        m(
            "msg.forward_upload",
            "把转发节点上传成 res_id，供后续发送引用。载荷上限 256 KiB / 100 个节点，\
             不让 core 变成大文件搬运通道",
            &[tags::FORWARD, tags::MESSAGE, tags::UPLOAD],
            Risk::Write,
            &[
                maybe_any("messages", "转发节点数组"),
                maybe_any("message", "转发节点数组（与 messages 二选一）"),
            ],
        ),
        m(
            "msg.forward_upload_alias",
            "upload_forward_msg 的拼写别名（平台同时提供 upload_foward_msg）。\
             与 msg.forward_upload 相同的载荷上限",
            &[tags::FORWARD, tags::MESSAGE, tags::UPLOAD],
            Risk::Write,
            &[
                maybe_any("messages", "转发节点数组"),
                maybe_any("message", "转发节点数组（与 messages 二选一）"),
            ],
        ),
        m(
            "msg.send_group_forward",
            "发送群合并转发（send_group_forward_msg）。与 msg.send_forward 的差别是\
             专用群接口；载荷上限 256 KiB / 100 个节点",
            &[tags::FORWARD, tags::MESSAGE, tags::SEND],
            Risk::Write,
            &[
                maybe_any("messages", "转发节点数组"),
                maybe_any("message", "转发节点数组（与 messages 二选一）"),
                opt("source", "转发来源标识"),
                opt("summary", "转发摘要"),
                opt("prompt", "转发提示"),
                maybe_any("news", "新闻/卡片节点"),
            ],
        ),
        m(
            "msg.send_private_forward",
            "发送私聊合并转发（send_private_forward_msg）。与 msg.send_forward 的差别是\
             专用私聊接口；载荷上限 256 KiB / 100 个节点",
            &[tags::FORWARD, tags::MESSAGE, tags::SEND],
            Risk::Write,
            &[
                maybe_any("messages", "转发节点数组"),
                maybe_any("message", "转发节点数组（与 messages 二选一）"),
                opt("source", "转发来源标识"),
                opt("summary", "转发摘要"),
                opt("prompt", "转发提示"),
                maybe_any("news", "新闻/卡片节点"),
            ],
        ),
        // ── 群相册写操作 ──
        m(
            "group.album.qun_list",
            "NapCat 风格相册列表封套：album_list / attach_info / has_more。只读",
            &[tags::GROUP, tags::IMAGE, tags::READ],
            Risk::Read,
            &[opt("attach_info", "上一页返回的分页游标")],
        ),
        m(
            "group.album.upload_image",
            "上传图片到群相册。file 是平台侧路径或 http(s) URL，ACB 不搬运文件字节；\
             写操作，发布后可见",
            &[tags::GROUP, tags::IMAGE, tags::UPLOAD],
            Risk::Write,
            &[
                req("album_id", "相册 id"),
                req("album_name", "相册名称"),
                req("file", "平台侧文件路径或 http(s) URL"),
            ],
        ),
        m(
            "group.album.upload_video",
            "上传视频到群相册。file 是平台侧路径或 http(s) URL，ACB 不搬运文件字节；\
             写操作，发布后可见",
            &[tags::GROUP, tags::VIDEO, tags::UPLOAD],
            Risk::Write,
            &[
                req("album_id", "相册 id"),
                req("album_name", "相册名称"),
                req("file", "平台侧文件路径或 http(s) URL"),
            ],
        ),
        m(
            "group.album.delete_media",
            "删除群相册里的图片或视频。lloc 是图片长定位标识或视频 id；不可恢复",
            &[tags::GROUP, tags::IMAGE, tags::VIDEO],
            Risk::Destructive,
            &[
                req("album_id", "相册 id"),
                req("lloc", "图片长定位标识或视频 id"),
            ],
        ),
        m(
            "group.album.comment",
            "在群相册图片/视频下发表评论。content 非空，最多 4096 字符",
            &[tags::GROUP, tags::TEXT],
            Risk::Write,
            &[
                req("album_id", "相册 id"),
                req("lloc", "图片长定位标识或视频 id"),
                req("content", "评论内容"),
            ],
        ),
        m(
            "group.album.like",
            "给群相册图片/视频点赞。batch_id 必填，lloc 可选",
            &[tags::GROUP, tags::IMAGE, tags::VIDEO],
            Risk::Write,
            &[
                req("album_id", "相册 id"),
                req("batch_id", "相册媒体批次 id"),
                opt("lloc", "图片长定位标识或视频 id"),
            ],
        ),
        m(
            "group.album.unlike",
            "取消对群相册图片/视频的点赞。batch_id 必填，lloc 可选",
            &[tags::GROUP, tags::IMAGE, tags::VIDEO],
            Risk::Write,
            &[
                req("album_id", "相册 id"),
                req("batch_id", "相册媒体批次 id"),
                opt("lloc", "图片长定位标识或视频 id"),
            ],
        ),
        // ── 批次 9：把剩余未排期动作全部暴露 ──
        //
        // 这批 op 覆盖覆盖矩阵里的 62 个未排期动作。除 7 个别名 op 有明确出处外，
        // 其余语义只有 SnowLuma 动作目录这一层来源，一律标 `Unverified`（门槛等同
        // critical）：直调被拒，只能走票据 + 审批，实验完成后再改 `Verified`。
        //
        // 风险分级按动作本身，不为了「看起来干净」放宽：账号资料/在线状态/群待办
        // 是 Write，删除/取消是 Destructive，rkey 与文件集下载是 Critical，
        // QQ 空间里代表账号对外发布的写操作至少 Destructive。

        // ── 账号资料 / 在线状态 / 账号能力探针 ──
        cm(
            "friend.dress",
            "查好友的装扮/头像挂件（_get_friend_dress）。只读",
            &[tags::FRIEND, tags::ACCOUNT],
            Risk::Read,
            &[req("user_id", "好友 QQ 号")],
        ),
        cm(
            "msg.mark_all_read",
            "把全部会话标为已读（_mark_all_as_read）。会改变账号未读状态，不是查询",
            &[tags::MESSAGE, tags::ACCOUNT],
            Risk::Write,
            &[],
        ),
        cm(
            "account.online_clients",
            "列出当前在线的设备/客户端（get_online_clients）。只读",
            &[tags::ACCOUNT, tags::SYSTEM],
            Risk::Read,
            &[],
        ),
        cm(
            "account.profile_likes",
            "查账号收到的资料点赞（get_profile_like）。只读",
            &[tags::ACCOUNT],
            Risk::Read,
            &[
                opt("user_id", "只看某个 QQ 号；不填=自己"),
                num("count", "取多少条"),
            ],
        ),
        cm(
            "account.user_status",
            "查某个用户的在线/输入状态（nc_get_user_status）。只读",
            &[tags::ACCOUNT, tags::SYSTEM],
            Risk::Read,
            &[req("user_id", "QQ 号")],
        ),
        cm(
            "account.diy_status",
            "设置自定义在线状态（set_diy_online_status）。会改变别人看到的账号状态",
            &[tags::ACCOUNT, tags::SYSTEM],
            Risk::Write,
            &[
                opt("status", "状态值，平台定义"),
                opt("text", "自定义状态文案"),
            ],
        ),
        cm(
            "account.input_status",
            "设置输入状态（set_input_status，如正在输入）。会向对方广播",
            &[tags::ACCOUNT, tags::SYSTEM],
            Risk::Write,
            &[req("status", "输入状态值，平台定义")],
        ),
        cm(
            "account.online_status",
            "设置账号在线状态（set_online_status）。会改变别人看到的在线状态",
            &[tags::ACCOUNT, tags::SYSTEM],
            Risk::Write,
            &[req("status", "在线状态值，平台定义")],
        ),
        cm(
            "account.avatar",
            "设置账号头像（set_qq_avatar）。file 支持路径 / http(s) URL / base64://",
            &[tags::ACCOUNT, tags::IMAGE],
            Risk::Write,
            &[req("file", "图片路径或 http(s) URL")],
        ),
        cm(
            "account.profile",
            "设置账号资料（set_qq_profile）：昵称、邮箱、公司、签名。缺省字段不动",
            &[tags::ACCOUNT],
            Risk::Write,
            &[
                opt("nickname", "昵称"),
                opt("email", "邮箱"),
                opt("company", "公司"),
                opt("personal_note", "个性签名"),
            ],
        ),
        cm(
            "account.long_nick",
            "设置账号长昵称（set_self_longnick）。会改变群名片之外的账号展示名",
            &[tags::ACCOUNT],
            Risk::Write,
            &[req("long_nick", "长昵称")],
        ),
        cm(
            "account.can_send_image",
            "探针：这台号此刻能不能发图片（can_send_image）。只读",
            &[tags::ACCOUNT, tags::IMAGE],
            Risk::Read,
            &[],
        ),
        cm(
            "account.can_send_record",
            "探针：这台号此刻能不能发语音（can_send_record）。只读",
            &[tags::ACCOUNT, tags::VOICE],
            Risk::Read,
            &[],
        ),
        cm(
            "account.rkey",
            "下载账号 rkey（get_rkey）。这是解密媒体所需的密钥材料，按账号凭据管理",
            &[tags::ACCOUNT, tags::SYSTEM],
            Risk::Critical,
            &[opt("user_id", "目标 QQ 号；不填=自己")],
        ),
        cm(
            "account.rkey_server",
            "从服务端拉取 rkey（get_rkey_server）。这是解密媒体所需的密钥材料",
            &[tags::ACCOUNT, tags::SYSTEM],
            Risk::Critical,
            &[opt("user_id", "目标 QQ 号；不填=自己")],
        ),
        // ── 群待办 / 内联键盘 / 机器人加群选项 ──
        m(
            "group.todo.list",
            "列群待办（get_group_todo_list）。只读",
            &[tags::GROUP],
            Risk::Read,
            &[],
        ),
        m(
            "group.todo.set",
            "把一条群消息设成群待办（set_group_todo）。会出现在群成员的待办列表里",
            &[tags::GROUP],
            Risk::Write,
            &[req("message_id", "要做成待办的消息 id")],
        ),
        m(
            "group.todo.complete",
            "完成一条群待办（complete_group_todo）。完成状态对所有成员可见",
            &[tags::GROUP],
            Risk::Write,
            &[req("todo_id", "群待办 id")],
        ),
        m(
            "group.todo.cancel",
            "取消一条群待办（cancel_group_todo）。待办会被移除，不可恢复",
            &[tags::GROUP],
            Risk::Destructive,
            &[req("todo_id", "群待办 id")],
        ),
        m(
            "msg.inline_button",
            "点击消息里的内联键盘按钮（click_inline_keyboard_button）。可能触发平台侧动作",
            &[tags::MESSAGE],
            Risk::Write,
            &[
                req("message_id", "带键盘的消息 id"),
                req("button_id", "按钮 id"),
                opt("callback_data", "按钮回调数据"),
            ],
        ),
        m(
            "group.robot_add_option",
            "设置群机器人的加群选项（set_group_robot_add_option）。会改变谁能拉机器人进群",
            &[tags::GROUP, tags::SYSTEM],
            Risk::Write,
            &[req("option", "加群选项值，平台定义")],
        ),
        // ── 收藏 / 闪传 / 文件集 ──
        cm(
            "file.collection_list",
            "列收藏列表（get_collection_list）。只读",
            &[tags::FILE, tags::MEDIA],
            Risk::Read,
            &[],
        ),
        cm(
            "file.flash_task_create",
            "创建闪传任务（create_flash_task）。任务会出现在账号的闪传列表里",
            &[tags::FILE, tags::UPLOAD],
            Risk::Write,
            &[opt("file_name", "文件名"), opt("file_url", "文件地址")],
        ),
        cm(
            "file.flash_file_list",
            "列闪传文件（get_flash_file_list）。只读",
            &[tags::FILE],
            Risk::Read,
            &[],
        ),
        cm(
            "file.flash_file_url",
            "取闪传文件的下载地址（get_flash_file_url）。地址可直接下载，按下载入口管理",
            &[tags::FILE, tags::DOWNLOAD],
            Risk::Critical,
            &[req("file_id", "闪传文件 id")],
        ),
        cm(
            "file.flash_file_delete",
            "删除闪传文件（delete_flash_file）。不可恢复",
            &[tags::FILE],
            Risk::Destructive,
            &[req("file_id", "闪传文件 id")],
        ),
        cm(
            "file.flash_file_rename",
            "重命名闪传文件（rename_flash_file）。会改变账号里的文件名",
            &[tags::FILE],
            Risk::Write,
            &[req("file_id", "闪传文件 id"), req("new_name", "新文件名")],
        ),
        cm(
            "file.flash_msg_send",
            "把闪传文件作为消息发出去（send_flash_msg）",
            &[tags::FILE, tags::SEND, tags::MESSAGE],
            Risk::Write,
            &[
                req("file_id", "闪传文件 id"),
                opt("user_id", "收件人 QQ 号"),
                opt("group_id", "目标群号"),
            ],
        ),
        cm(
            "file.fileset_list",
            "列文件集（list_filesets）。只读",
            &[tags::FILE],
            Risk::Read,
            &[],
        ),
        cm(
            "file.fileset_id",
            "按文件查它所属的文件集 id（get_fileset_id）。只读",
            &[tags::FILE],
            Risk::Read,
            &[opt("file_id", "文件 id")],
        ),
        cm(
            "file.fileset_info",
            "查文件集详情（get_fileset_info）。只读",
            &[tags::FILE],
            Risk::Read,
            &[req("fileset_id", "文件集 id")],
        ),
        cm(
            "file.fileset_download",
            "下载文件集（download_fileset）。会按平台返回的地址取回文件，按任意下载入口管理",
            &[tags::FILE, tags::DOWNLOAD],
            Risk::Critical,
            &[req("fileset_id", "文件集 id")],
        ),
        cm(
            "face.custom_move_front",
            "把收藏表情移到列表最前（move_custom_face_to_front）。会改变账号表情顺序",
            &[tags::IMAGE, tags::ACCOUNT],
            Risk::Write,
            &[req("emoji_id", "收藏表情 id")],
        ),
        // ── 单条转发 / Ark 卡片 / 分享 / 翻译 ──
        m(
            "msg.forward_single_friend",
            "把单条消息转发给好友（forward_friend_single_msg）",
            &[tags::FORWARD, tags::MESSAGE, tags::SEND],
            Risk::Write,
            &[req("message_id", "要转发的消息 id")],
        ),
        m(
            "msg.forward_single_group",
            "把单条消息转发到群（forward_group_single_msg）",
            &[tags::FORWARD, tags::MESSAGE, tags::SEND],
            Risk::Write,
            &[req("message_id", "要转发的消息 id")],
        ),
        cm(
            "msg.mini_app_ark",
            "取小程序 Ark 卡片载荷（get_mini_app_ark）。只读",
            &[tags::MESSAGE],
            Risk::Read,
            &[opt("app_id", "小程序 id"), opt("app_name", "小程序名称")],
        ),
        m(
            "msg.ark_share",
            "给好友发 Ark 分享卡片（send_ark_share）",
            &[tags::MESSAGE, tags::SEND],
            Risk::Write,
            &[any("ark", "Ark 卡片载荷"), opt("user_id", "收件人 QQ 号")],
        ),
        m(
            "msg.group_ark_share",
            "在群里发 Ark 分享卡片（send_group_ark_share）",
            &[tags::MESSAGE, tags::SEND],
            Risk::Write,
            &[any("ark", "Ark 卡片载荷")],
        ),
        m(
            "msg.tuwen_ark",
            "给好友发图文 Ark 卡片（send_tuwen_ark）",
            &[tags::MESSAGE, tags::SEND],
            Risk::Write,
            &[
                req("image", "图片地址"),
                opt("text", "卡片文案"),
                opt("user_id", "收件人 QQ 号"),
            ],
        ),
        m(
            "msg.group_share_ex",
            "把群名片分享给好友（share_group_ex）",
            &[tags::MESSAGE, tags::SEND],
            Risk::Write,
            &[
                req("group_id", "被分享的群号"),
                opt("user_id", "收件人 QQ 号"),
            ],
        ),
        m(
            "msg.peer_share",
            "分享联系人名片（share_peer）",
            &[tags::MESSAGE, tags::SEND],
            Risk::Write,
            &[
                req("user_id", "被分享的 QQ 号"),
                opt("group_id", "目标群号；不填=私聊"),
            ],
        ),
        cm(
            "msg.translate",
            "英译中（translate_en2zh）。只读",
            &[tags::TEXT],
            Risk::Read,
            &[
                req("text", "要翻译的英文"),
                opt("source", "源语言"),
                opt("target", "目标语言"),
            ],
        ),
        cm(
            "face.super_id",
            "判断一个表情能否用超级表情格式（fetch_super_face_id）。只读",
            &[tags::IMAGE, tags::SYSTEM],
            Risk::Read,
            &[req("face_id", "表情 id")],
        ),
        // ── 群信息扩展 / 别名 / 已读 / 拍一拍 ──
        m(
            "group.info_ex",
            "群信息扩展（get_group_info_ex）。只读",
            &[tags::GROUP],
            Risk::Read,
            &[],
        ),
        m(
            "msg.send_alias",
            "按 message_type 发送消息（send_msg）。与 msg.send 等价，保留平台原动作名",
            &[tags::MESSAGE, tags::SEND],
            Risk::Write,
            &[
                any("message", "消息内容"),
                opt("message_type", "group / private"),
                opt("group_id", "群号；message_type=group 时填"),
                opt("user_id", "QQ 号；message_type=private 时填"),
            ],
        ),
        m(
            "group.file.folder_delete_alias",
            "删除群文件文件夹（delete_group_folder）。与 group.file.folder_delete 等价",
            &[tags::GROUP, tags::FILE],
            Risk::Destructive,
            &[req("folder_id", "文件夹 id")],
        ),
        cm(
            "group.ignored_requests_alias",
            "查被忽略的加群请求（get_group_ignore_add_request）。与 group.ignored_requests 同义",
            &[tags::GROUP, tags::APPROVE],
            Risk::Read,
            &[opt("count", "取多少条")],
        ),
        cm(
            "msg.emoji_likes_alias",
            "查消息的表情表态（get_emoji_likes）。与 msg.reaction_users 同义",
            &[tags::MESSAGE],
            Risk::Read,
            &[
                req("message_id", "消息 id"),
                opt("emoji_id", "只看某个表情"),
                num("count", "取多少条"),
            ],
        ),
        m(
            "msg.mark_read_group",
            "把群消息标为已读（mark_group_msg_as_read）。只影响账号未读状态",
            &[tags::MESSAGE],
            Risk::Write,
            &[opt("message_id", "标到哪条消息；不填=全部")],
        ),
        m(
            "msg.mark_read_private",
            "把私聊消息标为已读（mark_private_msg_as_read）。只影响账号未读状态",
            &[tags::MESSAGE],
            Risk::Write,
            &[opt("message_id", "标到哪条消息；不填=全部")],
        ),
        m(
            "msg.poke_alias",
            "拍一拍（send_poke）。与 msg.poke 等价，群里 user_id 是拍谁",
            &[tags::POKE, tags::SEND, tags::MEMBER],
            Risk::Write,
            &[
                req("user_id", "拍谁 / 私聊对方 QQ 号"),
                opt("group_id", "群号；群里拍人时填"),
            ],
        ),
        // ── QQ 空间：代表账号对外发布，写操作至少 Destructive ──
        cm(
            "account.qzone.feeds",
            "列 QQ 空间动态（get_qzone_feeds）。只读",
            &[tags::ACCOUNT],
            Risk::Read,
            &[
                opt("user_id", "看谁的动态；不填=好友流"),
                num("count", "取多少条"),
                num("pos", "从第几条开始"),
            ],
        ),
        cm(
            "account.qzone.list",
            "列 QQ 空间说说（get_qzone_msg_list）。只读",
            &[tags::ACCOUNT],
            Risk::Read,
            &[
                opt("user_id", "看谁的说说；不填=自己"),
                num("count", "取多少条"),
                num("pos", "从第几条开始"),
            ],
        ),
        cm(
            "account.qzone.detail",
            "查一条 QQ 空间说说详情（get_qzone_msg_detail）。只读",
            &[tags::ACCOUNT],
            Risk::Read,
            &[req("msg_id", "说说 id")],
        ),
        cm(
            "account.qzone.send",
            "发一条 QQ 空间说说（send_qzone_msg）。代表账号对外发布",
            &[tags::ACCOUNT, tags::MESSAGE],
            Risk::Destructive,
            &[
                any("content", "说说正文"),
                opt("images", "图片地址，逗号分隔"),
            ],
        ),
        cm(
            "account.qzone.comment",
            "评论一条 QQ 空间说说（comment_qzone）。代表账号对外发布",
            &[tags::ACCOUNT, tags::MESSAGE],
            Risk::Destructive,
            &[req("msg_id", "说说 id"), req("content", "评论正文")],
        ),
        cm(
            "account.qzone.delete",
            "删除一条 QQ 空间说说（delete_qzone_msg）。不可恢复",
            &[tags::ACCOUNT],
            Risk::Destructive,
            &[req("msg_id", "说说 id")],
        ),
        cm(
            "account.qzone.like",
            "点赞一条 QQ 空间说说（like_qzone）。代表账号对外表态",
            &[tags::ACCOUNT],
            Risk::Destructive,
            &[req("msg_id", "说说 id")],
        ),
        cm(
            "account.qzone.unlike",
            "取消点赞一条 QQ 空间说说（unlike_qzone）。代表账号对外表态",
            &[tags::ACCOUNT],
            Risk::Destructive,
            &[req("msg_id", "说说 id")],
        ),
        cm(
            "account.qzone.ban",
            "设置 QQ 空间封禁（set_qzone_ban）。会限制账号的发布能力",
            &[tags::ACCOUNT],
            Risk::Destructive,
            &[num("seconds", "封禁时长；平台若按天解释则以此为准")],
        ),
        cm(
            "account.qzone.right",
            "设置 QQ 空间说说可见范围（set_qzone_msg_right）。会改变谁能看到",
            &[tags::ACCOUNT],
            Risk::Destructive,
            &[
                req("msg_id", "说说 id"),
                req("right", "可见范围值，平台定义"),
            ],
        ),
        // ── 私聊文件上传 ──
        m(
            "file.private_upload",
            "上传文件到私聊（upload_private_file）。只能在私聊房间里用",
            &[tags::FILE, tags::UPLOAD],
            Risk::Write,
            &[
                req("file", "平台侧路径或 http(s) URL"),
                opt("name", "显示的文件名"),
            ],
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

/// 私聊参数 `user_id` 由房间补上。
const fn private_route(method: &'static str, action: &'static str) -> Route {
    Route {
        method,
        target: Target::Fixed(action),
        from_group: None,
        from_private: Some("user_id"),
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
    route("face.custom_add", "add_custom_face"),
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
    route("group.tag_list", "get_group_tags"),
    route("group.tag_add", "add_group_tag"),
    route("group.tag_delete", "delete_group_tag"),
    group_route("group.add_option", "set_group_add_option"),
    group_route("group.search", "set_group_search"),
    group_route("group.share_link", "get_group_share_link"),
    group_route("group.detail", "get_group_detail_info"),
    group_route("group.shut_list", "get_group_shut_list"),
    group_route("group.signed_list", "get_group_signed_list"),
    group_route("group.album.list", "get_group_album_list"),
    group_route("group.album.media", "get_group_album_media_list"),
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
    route("file.share_link", "get_share_link"),
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
    group_route("group.file.folder_rename", "rename_group_file_folder"),
    group_route("group.file.trans", "trans_group_file"),
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
    // critical：只能由已批准的票据执行，直接 platform_call 会在 core 拒绝。
    route("account.credentials", "get_credentials"),
    route("account.cookies", "get_cookies"),
    route("account.csrf_token", "get_csrf_token"),
    route("account.clientkey", "get_clientkey"),
    route("account.decrypt_key", "request_decrypt_key"),
    route("system.send_packet", "send_packet"),
    route("system.bot_exit", "bot_exit"),
    route("system.restart", "set_restart"),
    route("system.clean_cache", "clean_cache"),
    // 文件下载 / 流式。
    route("file.download", "download_file"),
    route("stream.upload_file", "upload_file_stream"),
    route("stream.download_image", "download_file_image_stream"),
    route("stream.download_record", "download_file_record_stream"),
    route("stream.download_file", "download_file_stream"),
    route("stream.test_download", "test_download_stream"),
    route("stream.clean", "clean_stream_temp_file"),
    // 转发预上传与专用别名。
    group_route("msg.forward_upload", "upload_forward_msg"),
    group_route("msg.forward_upload_alias", "upload_foward_msg"),
    group_route("msg.send_group_forward", "send_group_forward_msg"),
    private_route("msg.send_private_forward", "send_private_forward_msg"),
    // 群相册写操作。
    group_route("group.album.qun_list", "get_qun_album_list"),
    group_route("group.album.upload_image", "upload_image_to_qun_album"),
    group_route("group.album.upload_video", "upload_video_to_qun_album"),
    group_route("group.album.delete_media", "del_group_album_media"),
    group_route("group.album.comment", "do_group_album_comment"),
    group_route("group.album.like", "set_group_album_media_like"),
    group_route("group.album.unlike", "cancel_group_album_media_like"),
    // 批次 9：账号资料 / 在线状态 / 群待办 / 收藏闪传 / Ark / QQ 空间 / 别名。
    route("friend.dress", "_get_friend_dress"),
    route("msg.mark_all_read", "_mark_all_as_read"),
    route("account.online_clients", "get_online_clients"),
    route("account.profile_likes", "get_profile_like"),
    route("account.user_status", "nc_get_user_status"),
    route("account.diy_status", "set_diy_online_status"),
    route("account.input_status", "set_input_status"),
    route("account.online_status", "set_online_status"),
    route("account.avatar", "set_qq_avatar"),
    route("account.profile", "set_qq_profile"),
    route("account.long_nick", "set_self_longnick"),
    route("account.can_send_image", "can_send_image"),
    route("account.can_send_record", "can_send_record"),
    route("account.rkey", "get_rkey"),
    route("account.rkey_server", "get_rkey_server"),
    group_route("group.todo.list", "get_group_todo_list"),
    group_route("group.todo.set", "set_group_todo"),
    group_route("group.todo.complete", "complete_group_todo"),
    group_route("group.todo.cancel", "cancel_group_todo"),
    route("msg.inline_button", "click_inline_keyboard_button"),
    group_route("group.robot_add_option", "set_group_robot_add_option"),
    route("file.collection_list", "get_collection_list"),
    route("file.flash_task_create", "create_flash_task"),
    route("file.flash_file_list", "get_flash_file_list"),
    route("file.flash_file_url", "get_flash_file_url"),
    route("file.flash_file_delete", "delete_flash_file"),
    route("file.flash_file_rename", "rename_flash_file"),
    route("file.flash_msg_send", "send_flash_msg"),
    route("file.fileset_list", "list_filesets"),
    route("file.fileset_id", "get_fileset_id"),
    route("file.fileset_info", "get_fileset_info"),
    route("file.fileset_download", "download_fileset"),
    route("face.custom_move_front", "move_custom_face_to_front"),
    private_route("msg.forward_single_friend", "forward_friend_single_msg"),
    group_route("msg.forward_single_group", "forward_group_single_msg"),
    route("msg.mini_app_ark", "get_mini_app_ark"),
    private_route("msg.ark_share", "send_ark_share"),
    group_route("msg.group_ark_share", "send_group_ark_share"),
    private_route("msg.tuwen_ark", "send_tuwen_ark"),
    private_route("msg.group_share_ex", "share_group_ex"),
    route("msg.peer_share", "share_peer"),
    route("msg.translate", "translate_en2zh"),
    route("face.super_id", "fetch_super_face_id"),
    group_route("group.info_ex", "get_group_info_ex"),
    route("msg.send_alias", "send_msg"),
    group_route("group.file.folder_delete_alias", "delete_group_folder"),
    route(
        "group.ignored_requests_alias",
        "get_group_ignore_add_request",
    ),
    route("msg.emoji_likes_alias", "get_emoji_likes"),
    group_route("msg.mark_read_group", "mark_group_msg_as_read"),
    private_route("msg.mark_read_private", "mark_private_msg_as_read"),
    route("msg.poke_alias", "send_poke"),
    route("account.qzone.feeds", "get_qzone_feeds"),
    route("account.qzone.list", "get_qzone_msg_list"),
    route("account.qzone.detail", "get_qzone_msg_detail"),
    route("account.qzone.send", "send_qzone_msg"),
    route("account.qzone.comment", "comment_qzone"),
    route("account.qzone.delete", "delete_qzone_msg"),
    route("account.qzone.like", "like_qzone"),
    route("account.qzone.unlike", "unlike_qzone"),
    route("account.qzone.ban", "set_qzone_ban"),
    route("account.qzone.right", "set_qzone_msg_right"),
    private_route("file.private_upload", "upload_private_file"),
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
                        verification: verification_of(method.name.as_ref()),
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
            Risk::Destructive | Risk::Critical => tags::DESTRUCTIVE,
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
    if (matches!(method.risk, Risk::Destructive | Risk::Critical)
        && matches!(method.group(), "group" | "member"))
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
            Risk::Write | Risk::Destructive | Risk::Critical => Effect::Write,
        }
    }
}

/// 名字里带 delete 但没挂 `recall` 标签的几个。
const DELETE_METHODS: &[&str] = &[
    "friend.delete",
    "group.file.delete",
    "group.file.folder_delete",
    "group.file.folder_delete_alias",
    "group.essence.delete",
    "group.notice.delete",
    "face.custom_delete",
    "group.album.delete_media",
    "group.todo.cancel",
    "file.flash_file_delete",
    "account.qzone.delete",
    "stream.clean",
];

/// 调用成功后要写进流水的那几个。core 靠它判断“这次调用会不会改变房间”。
fn record_of(method: &str) -> RecordMode {
    match method {
        "msg.send"
        | "msg.face"
        | "group.ai_voice_send"
        | "msg.send_alias"
        | "msg.ark_share"
        | "msg.group_ark_share"
        | "msg.tuwen_ark"
        | "msg.group_share_ex"
        | "msg.peer_share"
        | "file.flash_msg_send" => RecordMode::Message,
        "msg.send_forward"
        | "msg.send_group_forward"
        | "msg.send_private_forward"
        | "msg.forward_single_friend"
        | "msg.forward_single_group"
        | "group.notice.send" => RecordMode::Event,
        _ => RecordMode::None,
    }
}

/// 默认出处：SnowLuma 中文 API 目录（由 `collectActionDocs()` 生成，等价于
/// `@snowluma/mcp` 的 `CatalogAction`）与 OneBot 11 规范。
const DEFAULT_SOURCE: &str = "SnowLuma v2.4.0 (e44e4ced) action catalog / OneBot 11";

/// 每个 op 的可信度。只有 `research §3` 里需要实机实验的动作才标 `Unverified`；
/// 其余有官方文档/源码出处的按 `Verified` 处理。
fn verification_of(method: &str) -> Verification {
    match method {
        "group.add_option" => Verification::unverified(
            "add_type=0 的真实语义（空设置 / 允许任何人 / 不允许任何人）与 1-3 只有 NapCat \
             注释单一来源，尚未在 SnowLuma v2.4.0 实机确认",
            "research §3.1：先 get_group_admin_settings 记录基线，对 v={0,1,2,3,4,5,55} 逐个 \
             set_group_add_option 并回读 + QQ 客户端截图；再试 6/100 观察平台是否拒绝",
        ),
        "group.tag_list" | "group.tag_add" | "group.tag_delete" => Verification::unverified(
            "group_class 是分类 ID 且请求体没有 group_id；它是否等于 NTQQ GroupClassExt、\
             加/删标签是否影响群资料而不是全局分类目录，尚未实机确认",
            "research §3.3：选两个不同分类的测试群，扫描 get_group_tags(0..30)，加/删一个 \
             测试标签后检查两群资料与标签目录，最后清理",
        ),
        // 批次 9 的 7 个别名：语义就是已核实的同名/同参动作，给出处。
        "group.file.folder_delete_alias" => Verification::verified(
            "SnowLuma action catalog：delete_group_folder 与 delete_group_file_folder 同名同参，\
             等价于已核实的 group.file.folder_delete",
        ),
        "msg.mark_read_group" | "msg.mark_read_private" => Verification::verified(
            "SnowLuma action catalog：mark_group_msg_as_read / mark_private_msg_as_read 是 \
             mark_msg_as_read 的群/私聊专用别名",
        ),
        "msg.poke_alias" => Verification::verified(
            "SnowLuma action catalog：send_poke 是 group_poke / friend_poke 的合体别名，\
             群聊/私聊语义与已核实的 msg.poke 一致",
        ),
        "msg.emoji_likes_alias" => Verification::verified(
            "SnowLuma action catalog：get_emoji_likes 与 fetch_emoji_like 返回同一组表情表态",
        ),
        "group.ignored_requests_alias" => Verification::verified(
            "SnowLuma action catalog：get_group_ignore_add_request 与 get_group_ignored_notifies \
             同名同义",
        ),
        "msg.send_alias" => Verification::verified(
            "SnowLuma action catalog：send_msg 按 message_type 路由到 send_group_msg / \
             send_private_msg，与已核实的 msg.send 等价",
        ),
        // 批次 9 的其余 op：只有动作目录来源，语义/参数/权限未实机确认。
        "friend.dress"
        | "account.online_clients"
        | "account.profile_likes"
        | "account.user_status"
        | "account.diy_status"
        | "account.input_status"
        | "account.online_status"
        | "account.avatar"
        | "account.profile"
        | "account.long_nick"
        | "account.can_send_image"
        | "account.can_send_record"
        | "account.rkey"
        | "account.rkey_server"
        | "msg.mark_all_read" => Verification::unverified(
            "SnowLuma 动作目录只给出动作名与参数表；账号资料、在线状态、能力探针与 rkey 的\
             真实返回字段、生效条件与频率限制没有官方源码或实机出处",
            "research §3.4：用测试号逐项调用并记录返回字段与失败码；rkey 只在隔离号上验证，\
             不落盘、不外传，验证后立即丢弃",
        ),
        "group.todo.list"
        | "group.todo.set"
        | "group.todo.complete"
        | "group.todo.cancel"
        | "msg.inline_button"
        | "group.robot_add_option" => Verification::unverified(
            "群待办 / 内联键盘 / 机器人加群选项的 id 生命周期、权限位与幂等性只有 SnowLuma\
             动作目录来源",
            "research §3.5：建测试群，用群主/管理员/成员三种身份逐项执行并回读；记录失败码\
             与待办列表变化",
        ),
        "file.collection_list"
        | "file.flash_task_create"
        | "file.flash_file_list"
        | "file.flash_file_url"
        | "file.flash_file_delete"
        | "file.flash_file_rename"
        | "file.flash_msg_send"
        | "file.fileset_list"
        | "file.fileset_id"
        | "file.fileset_info"
        | "file.fileset_download"
        | "file.private_upload"
        | "face.custom_move_front" => Verification::unverified(
            "收藏 / 闪传的文件集与 flash task 生命周期、fileset_id 来源及下载 URL 权限只有\
             SnowLuma 动作目录来源",
            "research §3.6：用测试号创建任务、列文件集、下载后校验哈希；确认 \
             get_flash_file_url 是否对任意 fileset 都返回可下载地址",
        ),
        "msg.forward_single_friend"
        | "msg.forward_single_group"
        | "msg.mini_app_ark"
        | "msg.ark_share"
        | "msg.group_ark_share"
        | "msg.tuwen_ark"
        | "msg.group_share_ex"
        | "msg.peer_share"
        | "msg.translate"
        | "face.super_id"
        | "group.info_ex" => Verification::unverified(
            "单条转发、Ark 卡片、分享卡片与翻译的载荷形状只有 SnowLuma 动作目录来源，\
             字段名可能随客户端版本漂移",
            "research §3.7：用测试号逐项发送到测试群/测试好友，记录平台返回与客户端展示；\
             失败时保留原始 retcode",
        ),
        "account.qzone.feeds"
        | "account.qzone.list"
        | "account.qzone.detail"
        | "account.qzone.send"
        | "account.qzone.comment"
        | "account.qzone.delete"
        | "account.qzone.like"
        | "account.qzone.unlike"
        | "account.qzone.ban"
        | "account.qzone.right" => Verification::unverified(
            "QQ 空间动作只出现在 SnowLuma 目录；参数、权限、风控与「代表账号对外发布」的\
             可见范围未实机确认",
            "research §3.8：用隔离测试号发布/点赞/删除一条仅自己可见的说说，逐步放开可见\
             范围，确认失败码与频率限制",
        ),
        "group.search" => Verification::verified(
            "SnowLuma packages/proto-defs/src/oidb.ts:139-142（0=open,1=close）与 \
             packages/onebot/src/actions/group-admin.ts:323-330",
        ),
        "group.file.folder_rename" => Verification::verified(
            "SnowLuma packages/onebot/src/actions/group-file.ts:526-563（new_folder_name 优先、\
             name 回退，两者皆空 retcode 1400）",
        ),
        "file.share_link" => Verification::verified(
            "SnowLuma packages/onebot/src/actions/extended.ts:5032-5083,5143-5160,5362-5392\
             （fileset_id 来源与 GetFilesetDetail.uploadUrlWrap）",
        ),
        "group.file.trans" => Verification::verified(
            "SnowLuma packages/onebot/src/actions/extended.ts:3596-3627 与 \
             packages/protocol/src/oidb-services/group-file/trans-group-file.ts:1-2,22-28\
             （临时群文件转同群永久存储，无目标群参数）",
        ),
        "face.custom_add" => Verification::verified(
            "SnowLuma packages/onebot/src/actions/extended.ts:883-910 与 \
             packages/onebot/src/action-kit.ts:373-393（file 接受路径 / file:// / http(s):// / \
             base64:// / data URI）",
        ),
        "file.download" => Verification::verified(
            "SnowLuma packages/onebot/src/actions/extended.ts:37-42,277-342,4507-4573\
             （平台上限 1 GiB、默认 60 s、无 URL 白名单）",
        ),
        "stream.upload_file" => Verification::verified(
            "SnowLuma packages/onebot/src/streaming.ts:1-31,62-108、actions/stream-file.ts:\
             42-55,239-342（多请求上传 + 单独完成调用 + reset:true）",
        ),
        "stream.download_image" | "stream.download_record" | "stream.download_file" => {
            Verification::verified(
                "SnowLuma packages/onebot/src/actions/stream-download.ts:31-112,568-616,620-643\
                 （file_info → file_chunk* → file_complete；下载无逐请求取消）",
            )
        }
        "stream.test_download" | "stream.clean" => Verification::verified(
            "SnowLuma packages/onebot/src/actions/stream-download.ts:568-616,620-643 与 \
             actions/stream-file.ts:425-442",
        ),
        "msg.send" | "msg.face" | "msg.send_forward" => Verification::verified(
            "SnowLuma packages/onebot/src/helper/cq.ts:3-65 与 event-converter/to-message.ts:\
             80-90,124-144（string 模式按 CQ 码解析）",
        ),
        _ => Verification::verified(DEFAULT_SOURCE),
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
