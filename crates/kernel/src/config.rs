//! 配置：连接、人格、记忆、harness、harness 会话、订阅、三条策略。
//!
//! 五个一等对象，从下往上（ADR-0017 五对象模型）：
//! 1. **连接**：一条通到平台的管道（适配器种类 + 连接信息 + 登录账号）。同一平台可以有
//!    多条，每条一个账号——一个账号一条连接是平台的硬事实，不是我们的选择。
//! 2. **人格**：一张纯文本人设卡（id + name + 正文）。只管「是谁、什么语气、什么不能漏」，
//!    不带记忆、不带订阅、不声明能力（能力由会话配置与 harness/适配器决定）。
//! 3. **记忆**：一份长期知识，抽象接口（id + name + type + 位置参数）。type 可扩展，
//!    当前第一种是 git。可被多个会话共享，最终一致。
//! 4. **harness**：一个接管者（id + name）。桥不主动连它——它反过来连桥报名字 + 令牌。
//!    桥只存「有哪些 harness」用来校验会话的引用，不存它的连接地址。
//! 5. **harness 会话**：一个串行单元（id + name 可空 + harness id + 人格 id + 记忆 ids
//!    可空 + model 可空 + 订阅）。串行键 = harness id + 会话 id；同一会话内严格串行。
//!
//! 配置从 TOML 读进来后**完全校验**：非法配置在这里就死，不会带病启动。

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

#[cfg(all(feature = "test-fixtures", not(debug_assertions)))]
compile_error!(
    "acb-kernel 的 `test-fixtures` feature 仅用于测试/示例，不能在 release 构建启用；\
     生产请使用 Config::parse / Config::load。"
);

use acb_proto::RoomId;
use serde::Deserialize;
use thiserror::Error;

use crate::plugin::ProviderId;

/// 控制台保存专用的最高优先级分片文件名。
pub const CONSOLE_CONFIG_FILENAME: &str = "90-console.toml";

/// 没配「攒多少条」时的默认值。来源：实现默认，可推翻。
pub const DEFAULT_ACCUMULATE: u32 = 10;

/// 没配存储路径时的默认值：工作目录下的 `state/acb.db`。
/// 来源：实现默认。放 `state/` 是因为那里已经在 .gitignore 里，不会被误提交。
pub const DEFAULT_STORAGE_PATH: &str = "state/acb.db";

/// 没配「每个房间留多少条」时的默认值。来源：实现默认。
/// 5000 条对一个群来说大概是几个月的体量，够 `get_history` 往前翻。
pub const DEFAULT_MAX_RECORDS_PER_ROOM: u64 = 5000;

/// 没配「留多少天」时的默认值。来源：实现默认。
pub const DEFAULT_MAX_AGE_DAYS: u64 = 90;

/// 一轮投递最多塞多少条新记录的默认值。来源：实现默认，可推翻。
/// 这是「正文条数」的硬上限，不是未读上限：未读永远以完整编号区间给出。
pub const DEFAULT_DELIVERY_LIMIT: usize = 30;

/// 投递窗口的默认条数：触发前后各 3 条、时钟唤醒投最旧 3 条。来源：实现默认，
/// 可在 `[[sessions]]` 或 `[[sessions.subscriptions]]` 里覆盖。
pub const DEFAULT_CONTEXT_RECORDS: usize = 3;

/// 投递窗口能配的最大条数。上下文是给人看的，不是批量搬运；再大只会挤掉正文预算。
pub const MAX_CONTEXT_RECORDS: usize = 50;

/// 一轮的默认超时：关闭。模型想跑多久跑多久，桥和 harness 都不自动掐；
/// 需要钳制时在配置里写 `turn_timeout_secs = n`。
pub const DEFAULT_TURN_TIMEOUT_SECS: u64 = 0;

/// `[file_relay].dir` 不写时的默认值。跟 [storage].path 一样相对启动目录。
pub const DEFAULT_RELAY_DIR: &str = "state/relay";

/// `[file_relay].max_publish_bytes` 识别的大小单位（大小写不敏感）。
/// `KB`/`MB`/`GB`/`TB` 按十进制，`KiB`/`MiB`/`GiB`/`TiB` 按二进制。
const FILE_RELAY_SIZE_UNITS: &[(&str, u64)] = &[
    ("", 1),
    ("b", 1),
    ("kb", 1000),
    ("kib", 1024),
    ("mb", 1000 * 1000),
    ("mib", 1024 * 1024),
    ("gb", 1000 * 1000 * 1000),
    ("gib", 1024 * 1024 * 1024),
    ("tb", 1000_u64.pow(4)),
    ("tib", 1024_u64.pow(4)),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteSizeParseError {
    Expected,
    TooLarge,
}

fn parse_file_relay_max_publish_bytes(value: Option<toml::Value>) -> Result<u64, ConfigError> {
    let Some(value) = value else {
        return Ok(crate::relay::DEFAULT_MAX_PUBLISH_BYTES);
    };
    match value {
        toml::Value::Integer(bytes) => {
            let bytes = u64::try_from(bytes).map_err(|_| ConfigError::BadFileRelayMaxPublish {
                value: bytes.to_string(),
                detail: acb_i18n::t!("kernel.config.file_relay_size_expected"),
            })?;
            if bytes == 0 {
                return Err(ConfigError::BadFileRelayMaxPublish {
                    value: bytes.to_string(),
                    detail: acb_i18n::t!("kernel.config.file_relay_size_expected"),
                });
            }
            Ok(bytes)
        }
        toml::Value::String(text) => {
            parse_byte_size(&text).map_err(|kind| ConfigError::BadFileRelayMaxPublish {
                value: text.clone(),
                detail: match kind {
                    ByteSizeParseError::Expected => {
                        acb_i18n::t!("kernel.config.file_relay_size_expected")
                    }
                    ByteSizeParseError::TooLarge => {
                        acb_i18n::t!("kernel.config.file_relay_size_too_large")
                    }
                },
            })
        }
        other => Err(ConfigError::BadFileRelayMaxPublish {
            value: other.to_string(),
            detail: acb_i18n::t!("kernel.config.file_relay_size_expected"),
        }),
    }
}

/// 解析 `"4 GiB"` / `"4096 MiB"` / `"1.5 GiB"` / `"4096"` 这类写法。
/// 小数按整数字节向下取整；不接受 0、负数、未知单位和溢出。
fn parse_byte_size(text: &str) -> Result<u64, ByteSizeParseError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ByteSizeParseError::Expected);
    }
    let split_at = text
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split_at);
    let unit = unit.trim().to_ascii_lowercase();
    let multiplier = FILE_RELAY_SIZE_UNITS
        .iter()
        .find(|(candidate, _)| *candidate == unit)
        .map(|(_, multiplier)| *multiplier)
        .ok_or(ByteSizeParseError::Expected)?;

    let number = number.trim();
    let (int_part, frac_part) = number.split_once('.').unwrap_or((number, ""));
    if int_part.is_empty()
        || !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return Err(ByteSizeParseError::Expected);
    }

    let int: u128 = int_part.parse().map_err(|_| ByteSizeParseError::TooLarge)?;
    let frac: u128 = if frac_part.is_empty() {
        0
    } else {
        frac_part
            .parse()
            .map_err(|_| ByteSizeParseError::TooLarge)?
    };
    let scale = 10_u128
        .checked_pow(frac_part.len() as u32)
        .ok_or(ByteSizeParseError::TooLarge)?;
    let scaled = int
        .checked_mul(scale)
        .and_then(|value| value.checked_add(frac))
        .ok_or(ByteSizeParseError::TooLarge)?;
    let bytes = scaled
        .checked_mul(multiplier as u128)
        .ok_or(ByteSizeParseError::TooLarge)?
        / scale;
    if bytes == 0 {
        return Err(ByteSizeParseError::Expected);
    }
    u64::try_from(bytes).map_err(|_| ByteSizeParseError::TooLarge)
}

/// 没配 worktree 根目录时的默认值：工作目录下的 `state/worktrees`。
/// 来源：实现默认。放 `state/` 是因为那里已经在 .gitignore 里。
/// 每个「会话 × 记忆」的 worktree 按 `根/记忆id/会话id` 分层，路径固定才谈得上幂等。
pub const DEFAULT_WORKTREES_ROOT: &str = "state/worktrees";

/// 整份配置。
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// 人格列表。
    pub personas: Vec<Persona>,
    /// 记忆列表。
    pub memories: Vec<Memory>,
    /// harness 列表。一等对象：会话用它引用「归谁接管」。
    pub harnesses: Vec<Harness>,
    /// harness 会话列表。
    pub sessions: Vec<Session>,
    /// 接入点配置。没配就不监听——只跑内存里的桥（测试、嵌入式）。
    pub endpoint: Option<EndpointConfig>,
    /// 控制台配置。没配就不起控制台（ADR-0018）。
    pub console: Option<ConsoleConfig>,
    /// 文件中继配置。没配就不起文件服务，本机路径原样透传（ADR-0025）。
    pub file_relay: Option<FileRelayConfig>,
    /// 平台侧连接。一个账号一条，可以有多条。
    pub connections: Vec<Connection>,
    /// 动态库插件声明。启动时加载这些 `.so` / `.dylib`，manifest 由插件自己报；
    /// 插件可以是平台适配器，也可以是 wake/access 等策略 provider。
    pub plugins: Vec<PluginConfig>,
    /// 外置进程工具插件声明。只承载第三方工具；第一方工具仍走进程内 ToolLoader。
    pub tool_plugins: Vec<ToolPluginConfig>,
    /// 状态存哪。流水、水位、笔记、投递计数都在这里，重启不改。
    pub storage: StorageConfig,
    /// 流水保留：旧记录什么时候可以删。
    pub retention: RetentionConfig,
    /// 入站/路由动作队列容量。两个队列默认都有界；0 在解析期拒绝。
    pub queues: QueueConfig,
    /// 访问授权规则。空配置 = 只有订阅边界。
    pub grants: crate::grants::GrantsConfig,
    /// 一次性票据的申请冷却与审批聚合节奏。
    pub tickets: crate::tickets::TicketsPolicy,
    /// 后台任务的运行策略（完成唤醒、wait 上限）。
    pub tasks: TasksConfig,
    /// 依赖表的失败阈值与观察窗口。
    pub deps: DepsConfig,
    /// 插件 LKG 的失败阈值与观察窗口。
    pub lkg: LkgConfig,
    /// 策略 provider 同步调用的通用超时。
    pub provider_calls: ProviderCallsConfig,
    /// 带外通知（ntfy 兼容 HTTP）与 in-band 失败通知策略。没配整层不加载。
    pub notifications: Option<NotificationsConfig>,
    /// 记忆 worktree 的根目录。每个「会话 × 记忆」checkout 到这下面。
    pub worktrees_root: String,
    /// 会话**没配自己的 `on_turn_error`** 时的默认失败通知动作列表。
    /// 空 = 只记日志。写法同会话级 `on_turn_error`（`log` / `send:<房间>`）。
    pub default_on_turn_error: TurnErrorActions,
    /// 一轮投递最多塞多少条新记录。超过就只投前 `delivery_limit` 条，并在末尾附一句
    /// 「还有 N 条没投，用 fetch_records 抓」。0 或不配 = 不截断（照旧投满 MAX_DELIVERY）。
    pub delivery_limit: usize,
    /// 一轮最多跑多久（秒）。0 = 关闭超时——模型想跑多久跑多久，桥不掐、harness 也不许
    /// 自己掐；n > 0 = 超时后 harness 侧**主动打断**本侧会话（dsh 有队列能打断；不支持
    /// 打断的 harness 按各自兜底），打断算这轮失败、走 `on_turn_error` 通知。
    /// 之前 dsh 驱动里写死 5 分钟、到点干等，长任务必被腰斩（2026-10-31 用户裁定可配）。
    pub turn_timeout_secs: u64,
}

/// 一份实际参与合并的配置文件及其贡献的条目数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSource {
    pub path: String,
    pub connections: usize,
    pub sessions: usize,
    pub rules: usize,
}

/// 加载结果：生效配置、按加载顺序列出各文件的贡献、以及合并后的 TOML 文本。
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    /// 解析后的密钥集合。只暴露 `entries()` / `value(name)`，Debug 输出不含值。
    pub secrets: SecretStore,
    pub sources: Vec<ConfigSource>,
    /// 合并后的 TOML（无分片时就是 base 原文）。供 memory-steward / process-provider
    /// 这类独立解析器复用同一份生效文本。
    pub merged_toml: String,
    /// 是否在不同层里出现了内容相同、但 id 不同的 `[[grants.rules]]` 条目。
    /// 同 id 已在 `GrantsConfig::validate` 里硬失败；这里只给启动摘要提示。
    pub duplicate_rules: bool,
}

/// 配置里对某个 provider 的一次引用：id + 稳定位置串。
///
/// 位置串只用于控制台/错误信息展示，不参与解析；新增 provider-bearing 字段时
/// 必须同步更新 [`Config::provider_references`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderReference {
    pub id: ProviderId,
    pub location: String,
}

impl Config {
    /// 超时换算成毫秒给协议帧。0 = 关闭（帧里带 0，harness 收到就别自己兜底）。
    pub fn turn_timeout_ms(&self) -> u64 {
        self.turn_timeout_secs.saturating_mul(1000)
    }

    /// 把运行时可变的全局值重置为代码默认值。
    /// 配置文件里的同名键自 schema 16 起不再生效，避免「文件 + 库」两个真源。
    pub fn reset_runtime_defaults(&mut self) {
        self.default_on_turn_error = Vec::new();
        self.delivery_limit = DEFAULT_DELIVERY_LIMIT;
        self.turn_timeout_secs = DEFAULT_TURN_TIMEOUT_SECS;
        self.tickets = crate::tickets::TicketsPolicy::default();
        if let Some(notifications) = &mut self.notifications {
            notifications.min_interval_secs = DEFAULT_NOTIFICATION_MIN_INTERVAL_SECS;
            notifications.in_band = InBandNoticeConfig::default();
        }
    }

    /// 用库里的 `settings` 覆盖代码默认值。未知 key 只告警，不让旧库阻止启动。
    pub fn apply_settings(
        &mut self,
        settings: &BTreeMap<String, serde_json::Value>,
    ) -> Result<(), String> {
        for (key, value) in settings {
            match key.as_str() {
                "default_on_turn_error" => {
                    let array = value
                        .as_array()
                        .ok_or_else(|| format!("setting {key} must be an array of strings"))?;
                    let actions: Vec<String> = array
                        .iter()
                        .map(|item| {
                            item.as_str()
                                .map(str::to_owned)
                                .ok_or_else(|| format!("setting {key} must be an array of strings"))
                        })
                        .collect::<Result<_, _>>()?;
                    self.default_on_turn_error = parse_turn_error_actions(&actions)
                        .map_err(|error| format!("setting {key}: {error}"))?;
                }
                "delivery_limit" => {
                    let limit = value
                        .as_u64()
                        .ok_or_else(|| format!("setting {key} must be a positive integer"))?;
                    if limit == 0 {
                        return Err(format!("setting {key} must be greater than zero"));
                    }
                    self.delivery_limit = limit as usize;
                }
                "turn_timeout_secs" => {
                    self.turn_timeout_secs = value
                        .as_u64()
                        .ok_or_else(|| format!("setting {key} must be an integer"))?;
                }
                "tickets.request_cooldown_secs" => {
                    self.tickets.request_cooldown_secs = value
                        .as_u64()
                        .ok_or_else(|| format!("setting {key} must be an integer"))?;
                }
                "tickets.admin_cooldown_secs" => {
                    self.tickets.admin_cooldown_secs = value
                        .as_u64()
                        .ok_or_else(|| format!("setting {key} must be an integer"))?;
                }
                "tickets.auto_expire" => {
                    self.tickets.auto_expire = value
                        .as_bool()
                        .ok_or_else(|| format!("setting {key} must be a boolean"))?;
                }
                "tickets.pending_ttl_secs" => {
                    self.tickets.pending_ttl_secs = value
                        .as_u64()
                        .ok_or_else(|| format!("setting {key} must be an integer"))?;
                }
                "notifications.min_interval_secs" => {
                    if let Some(notifications) = &mut self.notifications {
                        notifications.min_interval_secs = value
                            .as_u64()
                            .ok_or_else(|| format!("setting {key} must be an integer"))?;
                    }
                }
                "notifications.in_band.window_secs" => {
                    if let Some(notifications) = &mut self.notifications {
                        notifications.in_band.window_secs = value
                            .as_u64()
                            .ok_or_else(|| format!("setting {key} must be an integer"))?;
                    }
                }
                "notifications.in_band.room_limit" => {
                    if let Some(notifications) = &mut self.notifications {
                        notifications.in_band.room_limit = value
                            .as_u64()
                            .ok_or_else(|| format!("setting {key} must be an integer"))?;
                    }
                }
                "notifications.in_band.max_entries" => {
                    if let Some(notifications) = &mut self.notifications {
                        notifications.in_band.max_entries = value
                            .as_u64()
                            .ok_or_else(|| format!("setting {key} must be an integer"))?;
                    }
                }
                "notifications.in_band.remind_secs" => {
                    if let Some(notifications) = &mut self.notifications {
                        notifications.in_band.remind_secs = value
                            .as_u64()
                            .ok_or_else(|| format!("setting {key} must be an integer"))?;
                    }
                }
                other => eprintln!("setting {other} is unknown; ignored"),
            }
        }
        Ok(())
    }

    /// 当前配置引用的所有 provider。`queue_unload` / `queue_reload` 用它判断
    /// “被引用就不能热卸载”，避免散落在 engine 里的手写扫描与新增字段遗漏。
    pub fn provider_references(&self) -> Vec<ProviderReference> {
        let mut refs = Vec::new();
        for connection in &self.connections {
            refs.push(ProviderReference {
                id: connection.receive.provider.clone(),
                location: format!("connection:{} receive", connection.id),
            });
            for route in &connection.routes {
                refs.push(ProviderReference {
                    id: route.provider.clone(),
                    location: format!("connection:{} route", connection.id),
                });
            }
        }
        for session in &self.sessions {
            refs.push(ProviderReference {
                id: session.access.provider.clone(),
                location: format!("session:{} access", session.id),
            });
            refs.push(ProviderReference {
                id: session.delivery.provider.clone(),
                location: format!("session:{} delivery", session.id),
            });
            for hook in session
                .outbound
                .before
                .iter()
                .chain(&session.outbound.after)
            {
                refs.push(ProviderReference {
                    id: hook.clone(),
                    location: format!("session:{} outbound", session.id),
                });
            }
            refs.push(ProviderReference {
                id: session.turn_error.provider.clone(),
                location: format!("session:{} turn_error", session.id),
            });
            for id in &session.prompts {
                refs.push(ProviderReference {
                    id: id.clone(),
                    location: format!("session:{} prompts", session.id),
                });
            }
            for subscription in &session.subscriptions {
                refs.push(ProviderReference {
                    id: subscription.wake.provider.clone(),
                    location: format!("session:{} wake {}", session.id, subscription.room),
                });
                refs.push(ProviderReference {
                    id: subscription.clock.provider.clone(),
                    location: format!("session:{} clock {}", session.id, subscription.room),
                });
            }
        }
        refs.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| left.location.cmp(&right.location))
        });
        refs.dedup();
        refs
    }
}

/// 一轮失败后要做的通知动作。
#[derive(Debug, Clone, PartialEq)]
pub enum TurnErrorAction {
    /// 记进桥日志（默认行为，永远可做）。
    Log,
    /// 把失败信息作为一条消息发到指定房间。
    Send { room: RoomId },
}

/// 一轮失败时按顺序执行的「通知动作」列表（挂在**会话**上，不是全局）。
///
/// 每个动作是一条字符串：`log` 记进日志；`send:<房间>` 把失败信息作为一条消息
/// 发到指定房间（房间写法同订阅，如 `send:qq:group/123`）。顺序执行，一个动作
/// 失败不打断后面的。空 = 只记日志（现状）。
pub type TurnErrorActions = Vec<TurnErrorAction>;

/// 流水保留策略。
///
/// 只管**已经读过**的那一段：任何在册订阅者还没读到的记录，一条都不删（见
/// `Bridge::prune`）。两项都是 0 表示不作任何清理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionConfig {
    /// 每个房间最多留多少条。0 = 不按条数清。
    pub max_records_per_room: u64,
    /// 记录最多留多少天。0 = 不按天数清。
    pub max_age_days: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            max_records_per_room: DEFAULT_MAX_RECORDS_PER_ROOM,
            max_age_days: DEFAULT_MAX_AGE_DAYS,
        }
    }
}

impl RetentionConfig {
    /// 两项都为 0 就是完全不清。
    pub fn enabled(self) -> bool {
        self.max_records_per_room > 0 || self.max_age_days > 0
    }
}

/// 状态存储：一个 SQLite 文件。
#[derive(Debug, Clone, PartialEq)]
pub struct StorageConfig {
    /// 库文件路径。相对路径按启动时的工作目录算。
    pub path: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: DEFAULT_STORAGE_PATH.to_owned(),
        }
    }
}

/// 连接方式。协议相同、连接方向不同，就是两种连接。
///
/// 内置只认 `ws-client` / `ws-server` / `process`；外置/动态适配器可以声明自己的
/// transport，由 manifest 在启动时校验，config 层不写死。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// 正向 WebSocket：桥当客户端，主动连平台开出来的 WS 服务端。
    WsClient,
    /// 反向 WebSocket：桥当服务端，等平台连过来。还没实现。
    WsServer,
    /// 外置进程：桥按连接配置启动适配器子进程，走 stdio wire 协议。
    Process,
    /// 适配器自己声明的连接方式。
    Other(String),
}

impl Transport {
    pub fn as_str(&self) -> &str {
        match self {
            Self::WsClient => "ws-client",
            Self::WsServer => "ws-server",
            Self::Process => "process",
            Self::Other(other) => other,
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        Some(match text {
            "ws-client" => Self::WsClient,
            "ws-server" => Self::WsServer,
            "process" => Self::Process,
            other => Self::Other(other.to_owned()),
        })
    }
}

/// 一条连接到平台的管道。
#[derive(Debug, Clone, PartialEq)]
pub struct Connection {
    /// 连接名。房间、订阅都靠它引用；也出现在给模型看的房间串里。
    pub id: String,
    /// 平台名，如 `qq`。给日志、平台特有内容、显示用。
    pub platform: String,
    /// 适配器种类。内置 `onebot11` / `process`；动态适配器用 `namespace.name`。
    pub kind: String,
    /// 连接方向。
    pub transport: Transport,
    /// 连接地址。`ws-client` 必填且必须是 `ws://`；外置进程适配器通常不填，
    /// 用 [`options`](Self::options) 里的启动命令。
    pub url: Option<String>,
    /// 适配器专属配置。形状由适配器自己的 manifest / 文档定义，core 不解释。
    pub options: serde_json::Value,
    /// 访问令牌。OneBot 实现开了 token 就要填，握手时放在 `Authorization` 头里。
    pub token: Option<String>,
    /// 这条连接登录的账号。
    ///
    /// 必填：桥要靠它分辨「这条消息是不是我自己发的」。分不出来就会自己回自己，
    /// 而且水位会被自己的话挡住。连上之后还会跟平台报的号核对一次。
    pub account: String,
    /// 系统管理员的账号列表。发送者号命中其中之一，就标 `SenderRole::SystemAdmin`。
    /// 空 = 没有系统管理员。适配器判身份时先查这个，查不到再去问平台群角色。
    pub admin_qqs: Vec<String>,
    /// 入站策略（ADR-0026）：决定这条连接上的记录收不收。默认全部接收。
    pub receive: ReceivePolicy,
    /// 消息路由：入库前交给代码处理（命令、链接、任务）。默认空 = 全部交给 AI。
    pub routes: Vec<RoutePolicy>,
}

/// 一个动态库插件声明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginConfig {
    /// `.so` / `.dylib` 路径。
    pub path: String,
    /// 期望的适配器 kind。只对平台适配器插件生效；不写就不校验。
    pub kind: Option<String>,
    /// 动态库单次调用超时（毫秒）。省略或 0 用适配器默认值（5 秒）；
    /// 只对平台适配器生效：超时后该连接句柄标死，后续调用立即失败。
    pub timeout_ms: Option<u64>,
    /// 给人和 LKG 看的版本标签。不参与加载校验；未写时 LKG 记 `"unknown"`。
    pub version: Option<String>,
}

/// 依赖表默认失败次数：同一 id 同一 class 在窗口内累计到 3 次才 down。
pub const DEFAULT_DEPS_FAILURE_THRESHOLD: u64 = 3;
/// 依赖表默认观察窗口（秒）。
pub const DEFAULT_DEPS_FAILURE_WINDOW_SECS: u64 = 600;

/// 依赖表失败阈值配置。`[deps]` 省略时用默认值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepsConfig {
    pub failure_threshold: u64,
    pub failure_window_secs: u64,
}

impl Default for DepsConfig {
    fn default() -> Self {
        Self {
            failure_threshold: DEFAULT_DEPS_FAILURE_THRESHOLD,
            failure_window_secs: DEFAULT_DEPS_FAILURE_WINDOW_SECS,
        }
    }
}

/// 插件 LKG 默认失败次数。
pub const DEFAULT_LKG_MAX_FAILURES: u64 = 3;
/// 插件 LKG 默认观察窗口（秒）。
pub const DEFAULT_LKG_WINDOW_SECS: u64 = 600;

/// 插件 LKG 失败阈值配置。`[lkg]` 省略时行为与旧默认完全一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LkgConfig {
    pub max_failures: u64,
    pub window_secs: u64,
}

impl Default for LkgConfig {
    fn default() -> Self {
        Self {
            max_failures: DEFAULT_LKG_MAX_FAILURES,
            window_secs: DEFAULT_LKG_WINDOW_SECS,
        }
    }
}

/// 策略 provider 同步调用的默认超时（毫秒）。0 = 关闭。
pub const DEFAULT_PROVIDER_CALL_TIMEOUT_MS: u64 = 5000;
/// 允许配置的最大 provider 超时，避免误配成几分钟。
pub const MAX_PROVIDER_CALL_TIMEOUT_MS: u64 = 60_000;

/// 策略 provider 同步调用的超时配置。`timeout_ms = 0` 表示不做超时（旧行为）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCallsConfig {
    pub timeout_ms: u64,
}

impl Default for ProviderCallsConfig {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_PROVIDER_CALL_TIMEOUT_MS,
        }
    }
}

/// 带外通知默认去重窗口（秒）。
pub const DEFAULT_NOTIFICATION_MIN_INTERVAL_SECS: u64 = 600;
/// in-band 失败通知默认去重窗口（秒）。
pub const DEFAULT_IN_BAND_WINDOW_SECS: u64 = 600;
/// in-band 失败通知默认每房间上限。
pub const DEFAULT_IN_BAND_ROOM_LIMIT: u64 = 3;
/// in-band 失败通知默认去重表上限。
pub const DEFAULT_IN_BAND_MAX_ENTRIES: u64 = 256;
/// 连续失败时补一条「还在出错」提醒的默认间隔（秒）。
pub const DEFAULT_IN_BAND_REMIND_SECS: u64 = 600;

/// 带外通知（ntfy 兼容 HTTP）。`active()` 为 false 时整层不加载。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationsConfig {
    /// 总开关。默认 true；显式 false 时即使填了 url/topic 也不加载。
    pub enabled: bool,
    /// ntfy 根地址，如 `http://127.0.0.1:8080`；不含 topic。
    pub url: Option<String>,
    /// ntfy topic。
    pub topic: Option<String>,
    /// topic 引用的来源名。只用于启动摘要/审计，不参与发送；值本身不落日志。
    pub topic_source: Option<String>,
    /// 可选访问令牌；填了就发 `Authorization: Bearer <token>`。
    pub token: Option<String>,
    /// 同一依赖边沿/抑制事件的去重窗口（秒）。
    pub min_interval_secs: u64,
    /// in-band turn 失败通知的去重/限流参数。
    pub in_band: InBandNoticeConfig,
}

impl NotificationsConfig {
    /// 只有开关打开且 url/topic 都非空时才认为这一层要加载。
    pub fn active(&self) -> bool {
        self.enabled
            && self.url.as_deref().is_some_and(|url| !url.is_empty())
            && self.topic.as_deref().is_some_and(|topic| !topic.is_empty())
    }
}

/// in-band turn 失败通知的策略。默认值与原硬编码常量一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InBandNoticeConfig {
    /// 同一 (session, room, 故障) 的去重窗口（秒）。
    pub window_secs: u64,
    /// 同一房间在窗口内最多发几条。
    pub room_limit: u64,
    /// 去重表上限。
    pub max_entries: u64,
    /// 被抑制之后，同一房间最多隔多久补一条「还在出错」提醒（秒）。
    ///
    /// 为什么需要它：去重与每房间上限都是为了不刷屏，但两者叠加的结果是
    /// **连续失败时完全静默**——首条之后一句都没有，房间里看到的是「没人回话」，
    /// 与「桥挂了」无法区分（2026-11-09 连续 6 轮 content-blocked 就是这么误判的）。
    pub remind_secs: u64,
}

impl Default for InBandNoticeConfig {
    fn default() -> Self {
        Self {
            window_secs: DEFAULT_IN_BAND_WINDOW_SECS,
            room_limit: DEFAULT_IN_BAND_ROOM_LIMIT,
            max_entries: DEFAULT_IN_BAND_MAX_ENTRIES,
            remind_secs: DEFAULT_IN_BAND_REMIND_SECS,
        }
    }
}

/// 入站队列默认容量。
pub const DEFAULT_INBOUND_QUEUE: usize = 4096;
/// 路由动作队列默认容量。
pub const DEFAULT_ROUTE_QUEUE: usize = 1024;

/// 队列容量配置。`[queues]` 省略时用默认值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueConfig {
    pub inbound: usize,
    pub route: usize,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            inbound: DEFAULT_INBOUND_QUEUE,
            route: DEFAULT_ROUTE_QUEUE,
        }
    }
}

/// 外置进程工具插件的默认单次调用超时（毫秒）。
pub const DEFAULT_TOOL_PLUGIN_TIMEOUT_MS: u64 = 5000;
/// 允许配置的最小/最大超时，避免误配成 0 或几分钟。
pub const MIN_TOOL_PLUGIN_TIMEOUT_MS: u64 = 100;
pub const MAX_TOOL_PLUGIN_TIMEOUT_MS: u64 = 60_000;

/// 一个外置进程工具插件声明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPluginConfig {
    /// ProviderId（必须含 namespace，且不能是 `acb`）。
    pub id: String,
    /// 受信配置给出的可执行文件。
    pub command: String,
    /// 传给可执行文件的参数。
    pub args: Vec<String>,
    /// 单次调用超时；`None` 用默认 5 秒。
    pub timeout_ms: Option<u64>,
    /// 传给子进程的额外环境变量。插件配置以 `ACB_PLUGIN_*` 形式从这里进来。
    pub env: Vec<(String, String)>,
}

impl ToolPluginConfig {
    pub fn timeout(&self) -> u64 {
        self.timeout_ms.unwrap_or(DEFAULT_TOOL_PLUGIN_TIMEOUT_MS)
    }
}

/// 目前认识的记忆存储形式。加新形式时在这里登记。
pub const KNOWN_MEMORY_KINDS: &[&str] = &["git"];

/// 接入点：harness 连进来的地址。
#[derive(Debug, Clone, PartialEq)]
pub struct EndpointConfig {
    /// 监听地址，形如 `127.0.0.1:7700`。
    pub listen: String,
}

/// 控制台：桥侧的统一管理 HTTP 服务（ADR-0018）。
///
/// 可远程访问（监听地址可配到非回环），因此 token 鉴权必填、必须认真校验。
#[derive(Debug, Clone, PartialEq)]
pub struct ConsoleConfig {
    /// 监听地址，形如 `0.0.0.0:3100` 或 `127.0.0.1:3100`。
    pub listen: String,
    /// 鉴权 token。所有请求都要带（`Authorization: Bearer <token>`）。
    pub token: String,
}

/// 文件中继（ADR-0025）。agent 和平台不共用文件系统时，桥把本机文件收下来
/// 换成平台取得到的 URL。没配就不起文件服务，本机路径原样透传。
#[derive(Debug, Clone, PartialEq)]
pub struct FileRelayConfig {
    /// 中继方式。当前唯一实现 `file-server`（桥自带文件服务器）；
    /// 留着这个字段是为了将来加别的收发方式（比如反向推送到平台）时不用改配置结构。
    pub mode: String,
    /// 监听地址，如 `0.0.0.0:3131`。
    pub listen: String,
    /// 平台那边访问桥用的基地址（不含尾斜杠），如 `http://10.89.0.1:3131`。
    /// 平台在容器里时通常写宿主网关地址。
    pub public_base: String,
    /// 收下的文件落盘位置。
    pub dir: String,
    /// 单个文件写入中继目录的上限（字节）。默认 4 GiB，对齐平台的文件上传上限。
    ///
    /// 支持整数字节或人类可读写法（`"4 GiB"`、`"4096 MiB"`、`"1.5 GiB"`）。
    /// 每次发布都会整份复制到 `dir` 并保留 24 小时；调高后要注意并发发布时的磁盘峰值。
    pub max_publish_bytes: u64,
}

/// `FileRelayConfig.mode` 当前唯一合法值。
pub const FILE_RELAY_MODE_FILE_SERVER: &str = "file-server";

/// 后台任务的运行策略。
#[derive(Debug, Clone, PartialEq)]
pub struct TasksConfig {
    /// 任务收工后，是否由桥直接给**发起该任务的会话**排一轮唤醒。
    ///
    /// 默认 false：与旧行为一致——任务只落记录、发合成事件，叫不叫醒由订阅的
    /// wake 策略决定。打开后不走 `lua.wake_policy`，也不叫醒同房间的其它订阅者。
    pub wake_on_completion: bool,
    /// `tasks op=wait` 单次等待的服务端上限（毫秒）。调用方给的 `wait_ms`
    /// 超过它会被截断到它，并在返回里说明；不写死具体值，由现场配置决定。
    pub wait_max_ms: u64,
}

/// `[tasks].wait_max_ms` 的默认上限。刻意低于 opencode 的单次工具调用硬超时（60 秒），
/// 但桥不感知 harness，只提供这个可配天花板。
pub const DEFAULT_TASK_WAIT_MAX_MS: u64 = 45_000;

impl Default for TasksConfig {
    fn default() -> Self {
        Self {
            wake_on_completion: false,
            wait_max_ms: DEFAULT_TASK_WAIT_MAX_MS,
        }
    }
}

/// 一张人格卡：纯文本人设。
#[derive(Debug, Clone, PartialEq)]
pub struct Persona {
    pub id: String,
    /// 人设名，给人看的。
    pub name: String,
    /// 人设正文：「你是谁、什么性格、什么语气」，以及信息边界（什么不能漏）。
    pub text: String,
}

/// 一份记忆：抽象接口。`type` 决定底层存储，当前第一种是 `git`。
#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    pub id: String,
    /// 记忆名，给人看的。
    pub name: String,
    /// 存储形式，如 `git`。可扩展。
    pub kind: String,
    /// 与 `kind` 对应的位置/参数。`git` 下是仓库路径。
    pub location: String,
}

/// 一个 harness：一等对象，会话靠它引用「归谁接管」。
///
/// 桥不主动连 harness——是 harness 反过来连桥、报出名字 + 令牌。所以桥对 harness
/// 只需要知道「有哪些、id 是什么」用来校验会话的引用，不存它的连接地址或内部配置。
#[derive(Debug, Clone, PartialEq)]
pub struct Harness {
    pub id: String,
    /// 显示名，给人看的。
    pub name: String,
}

/// 一个 harness 会话：一个串行单元。
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: String,
    /// 这个会话归哪个 harness 接管，引用 `harnesses` 里的某个 id。
    /// 串行键 = harness 名 + 会话 id。
    pub harness: String,
    /// 会话显示名，给人看的。可空，缺省 = id。
    pub name: String,
    /// 这个会话用哪个模型。可空 = 由 harness 侧自己决定。
    pub model: Option<String>,
    /// dsh 驱动的 MCP 端口。可空 = 旧配置，由 acbctl 按历史下标推导并告警。
    /// 新配置必须显式写，避免「插一个会话把所有人端口平移」。
    pub driver_port: Option<u16>,
    /// 人格卡 id。
    pub persona: String,
    /// 引用的记忆 id 列表。可空（空 = 不存长期记忆）。
    pub memories: Vec<String>,
    /// 接入令牌。harness 握手时报出令牌与 harness 名，桥据此绑定会话。
    pub token: String,
    /// 订阅列表。
    pub subscriptions: Vec<Subscription>,
    /// 访问策略（ADR-0026 Phase 2d）。默认按订阅；可换 provider。
    pub access: AccessPolicy,
    /// 投递批次选择策略（ADR-0026 Phase 4）。默认从最旧开始连续投一段。
    pub delivery: DeliveryPolicy,
    /// 发送前/后钩子（ADR-0026 Phase 4）。按列表顺序执行。
    pub outbound: OutboundPolicy,
    /// 失败通知策略（ADR-0026）：默认按 `on_turn_error` / 全局默认选动作。
    pub turn_error: TurnErrorPolicy,
    /// 上下文注入 provider 列表（按配置顺序）。空 = 不注入。
    pub prompts: Vec<ProviderId>,
    /// 这个会话一轮失败时按顺序执行的「通知动作」列表。空 = 只记日志。
    /// 见 [`TurnErrorAction`]。
    pub on_turn_error: TurnErrorActions,
}

/// 一条订阅：连接 + 房间 + 唤醒策略。
#[derive(Debug, Clone, PartialEq)]
pub struct Subscription {
    /// 房间（连接名 + 路径）。
    pub room: RoomId,
    pub wake: WakePolicy,
    /// 时钟策略（ADR-0011）。默认关闭。
    pub clock: ClockPolicy,
    /// 这个房间的投递窗口。订阅没写的字段继承会话级默认值。
    pub window: DeliveryWindow,
}

/// 投递窗口：正文带多少、区间怎么留。
///
/// 唤醒时只投「触发消息前后 N 条」的完整正文，其余未读只给编号区间；时钟/攒批
/// 唤醒没有触发消息，就投最旧的 `batch_head` 条正文。默认 3/3/3。
///
/// 这是用户 2026-11-10 定稿的投递格式（见 `docs/plan/downstream-changes.md` D7）：
/// 上下文由桥给够，读多少由 agent 自己决定，避免一次把几十条正文塞进上下文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryWindow {
    /// 触发消息之前带几条正文（可能已经确认过）。
    pub context_before: usize,
    /// 触发消息之后带几条正文。
    pub context_after: usize,
    /// 时钟/攒批唤醒投最旧的几条正文。
    pub batch_head: usize,
}

impl Default for DeliveryWindow {
    fn default() -> Self {
        Self {
            context_before: DEFAULT_CONTEXT_RECORDS,
            context_after: DEFAULT_CONTEXT_RECORDS,
            batch_head: DEFAULT_CONTEXT_RECORDS,
        }
    }
}

/// 一个会话的访问策略。
///
/// **只决定「能不能碰这个房间/这条连接」**；风险等级只是操作属性，不参与鉴权。
///
/// 不写 `provider` 时用内置 `[grants]` 评估器；写外部 provider 时由插件负责判定。
#[derive(Debug, Clone, PartialEq)]
pub struct AccessPolicy {
    /// 访问策略的 provider id（ADR-0026）。
    pub provider: ProviderId,
    /// 插件专属配置；内置 access 忽略它。
    pub options: serde_json::Value,
}

impl Default for AccessPolicy {
    fn default() -> Self {
        Self {
            provider: ProviderId::builtin_access(),
            options: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

/// 一个会话的投递批次选择策略（ADR-0026 Phase 4）。
#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryPolicy {
    /// composer 的 provider id。不写就用内置「从旧到新连续投一段」。
    pub provider: ProviderId,
    /// 插件专属配置；内置 composer 忽略它。
    pub options: serde_json::Value,
}

impl Default for DeliveryPolicy {
    fn default() -> Self {
        Self {
            provider: ProviderId::builtin_delivery(),
            options: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

/// 一个会话的发送钩子。按列表顺序执行：`before` 可以改写正文，
/// `after` 只观察回执。空列表 = 不挂任何钩子。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OutboundPolicy {
    pub before: Vec<ProviderId>,
    pub after: Vec<ProviderId>,
}

/// 一轮失败时怎么通知（ADR-0026）。
#[derive(Debug, Clone, PartialEq)]
pub struct TurnErrorPolicy {
    /// 失败通知的 provider id。不写就用内置「会话动作优先、否则全局默认」。
    pub provider: ProviderId,
    /// 插件专属配置；内置 provider 忽略它。
    pub options: serde_json::Value,
}

impl Default for TurnErrorPolicy {
    fn default() -> Self {
        Self {
            provider: ProviderId::builtin_turn_error(),
            options: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

/// 一条订阅的时钟策略（ADR-0011）。默认关闭。
#[derive(Debug, Clone, PartialEq)]
pub struct ClockPolicy {
    pub provider: ProviderId,
    /// 固定间隔（秒）；0 / 不写 = 不看间隔。
    pub interval_secs: Option<u64>,
    /// 房间安静多久（秒）后评估；0 / 不写 = 不看空闲。
    pub idle_secs: Option<u64>,
    /// 插件专属配置；内置时钟忽略它。
    pub options: serde_json::Value,
}

impl Default for ClockPolicy {
    fn default() -> Self {
        Self {
            provider: ProviderId::builtin_clock_off(),
            interval_secs: None,
            idle_secs: None,
            options: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

/// 一条连接的入站策略（ADR-0026）。默认全部接收。
#[derive(Debug, Clone, PartialEq)]
pub struct ReceivePolicy {
    pub provider: ProviderId,
    /// 插件专属配置；内置「全部接收」忽略它。
    pub options: serde_json::Value,
}

/// 一条连接的消息路由策略。按配置顺序执行；前一个 `pass` 才轮到下一个。
#[derive(Debug, Clone, PartialEq)]
pub struct RoutePolicy {
    pub provider: ProviderId,
    /// 插件专属配置；形状由 provider 自己定义。
    pub options: serde_json::Value,
}

impl Default for ReceivePolicy {
    fn default() -> Self {
        Self {
            provider: ProviderId::builtin_receive_all(),
            options: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

/// 唤醒策略的判定输入。只带判定需要的东西，不把整条记录塞进来。
#[derive(Debug, Clone, Copy)]
pub struct WakeInput<'a> {
    /// 发送者 id。
    pub sender_id: &'a str,
    /// 消息纯文本（事件没有文本，传空串）。
    pub text: &'a str,
    /// 是不是冲本账号来的（@ 到我、拍我）。
    pub at_self: bool,
    /// 这条是不是引用了我发的某条消息。
    pub reply_to_self: bool,
    /// 有没有正文。只有 @ / 引用 / 空的纯文本时是 false，图片、文件等算 true。
    /// 跨平台判断「触发消息是否只是一句 @」，策略不用认识平台名。
    pub has_body: bool,
    /// 平台事件类型；普通消息为 `None`。
    pub event: Option<&'a str>,
    /// 这条连接所属平台（`qq` / `discord` …），用于匹配 `qq.poke` 这种限定事件名。
    pub platform: &'a str,
    /// 事件语义标签；普通消息为空。
    pub tags: &'a [&'a str],
    /// 这条进来之后，该房间待处理的未读条数。
    pub unread: u32,
}

/// 判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeDecision {
    Wake,
    Stay,
}

/// 判定结果 + 原因。原因给 agent 看「这次为什么醒」，不参与调度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeVerdict {
    pub decision: WakeDecision,
    pub reason: Option<String>,
    /// 命中后先等多久再真正开轮。0 = 立即。
    ///
    /// 这段时间里同房间的新消息照常入库；到点 `open_turn` 会一起取走。
    /// 给「只有 @、没有正文」的场景留出等后续内容的时间。
    pub delay_ms: u64,
    /// 这次命中是「攒够一批」而不是「某一条消息点到我」。
    ///
    /// 攒批唤醒没有触发消息：投递按 `batch_head` 投最旧的几条，让水位能顺着往前走；
    /// 单条命中则按触发消息前后 N 条投。默认 false（按触发消息算）。
    pub batch: bool,
}

impl WakeVerdict {
    pub fn wake(reason: impl Into<String>) -> Self {
        Self {
            decision: WakeDecision::Wake,
            reason: Some(reason.into()),
            delay_ms: 0,
            batch: false,
        }
    }

    /// 命中后延迟 `delay_ms` 再开轮。
    pub fn wake_after(reason: impl Into<String>, delay_ms: u64) -> Self {
        Self {
            decision: WakeDecision::Wake,
            reason: Some(reason.into()),
            delay_ms,
            batch: false,
        }
    }

    /// 攒够一批才醒：没有触发消息，投递走 `batch_head`。
    pub fn wake_batch(reason: impl Into<String>) -> Self {
        Self {
            decision: WakeDecision::Wake,
            reason: Some(reason.into()),
            delay_ms: 0,
            batch: true,
        }
    }

    pub fn stay() -> Self {
        Self {
            decision: WakeDecision::Stay,
            reason: None,
            delay_ms: 0,
            batch: false,
        }
    }
}

/// 唤醒策略。
///
/// 默认五件套：@ 我就醒、引用我的话也醒、叫昵称就醒、攒够 N 条就醒、被拍就醒
/// （ADR-0002 / 0007）。关键词、事件类型、免打扰名单是可选开关，默认全关。
#[derive(Debug, Clone, PartialEq)]
pub struct WakePolicy {
    /// 唤醒策略的 provider id（ADR-0026）。默认是内置实现。
    pub provider: ProviderId,
    /// @ 到本账号就醒。
    pub at_mention: bool,
    /// 引用我发的消息就醒（QQ 的「回复」就是这种）。
    ///
    /// 和 `at_mention` 分开：有人只想被 @ 叫醒，不想被人引用一条旧消息就醒。
    pub reply: bool,
    /// 文本里出现 `names` 里任意一个就醒。
    pub name_mention: bool,
    pub names: Vec<String>,
    /// 未读攒到这么多条就醒；`None` 表示不看条数。
    pub accumulate: Option<u32>,
    /// 平台事件语义标签，命中就醒（如 `poke`、`reaction`、`member_join`）。
    ///
    /// 这是跨平台维度：适配器把平台事件翻译成标签，core 不需要知道平台名。
    pub event_tags: Vec<String>,
    /// 命中这些标签的事件**不**叫醒（只挡唤醒，不挡记录）。
    ///
    /// 旧配置的 `poke = false` 等价于在这里加 `poke`。
    pub ignore_event_tags: Vec<String>,
    /// 额外的事件类型，命中就醒。可以写 `qq.poke` 这种带平台前缀的名字，
    /// 也可以写 `qq.*` 匹配该平台全部事件。
    pub events: Vec<String>,
    /// 文本命中关键词就醒。默认空。
    pub keywords: Vec<String>,
    /// 这些人说话不叫醒（只挡唤醒，不挡记录）。
    pub ignore_speakers: Vec<String>,
    /// 插件专属配置：TOML 里 `[... .wake.options]` 的自由表，core 不解释，原样交给
    /// provider。内置 wake 忽略它。
    pub options: serde_json::Value,
}

impl Default for WakePolicy {
    fn default() -> Self {
        Self {
            provider: ProviderId::builtin_wake(),
            at_mention: true,
            reply: true,
            name_mention: true,
            names: Vec::new(),
            accumulate: Some(DEFAULT_ACCUMULATE),
            event_tags: Vec::new(),
            ignore_event_tags: Vec::new(),
            events: Vec::new(),
            keywords: Vec::new(),
            ignore_speakers: Vec::new(),
            options: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

impl WakePolicy {
    /// 判定这条要不要叫醒设备。
    ///
    /// 顺序有讲究：免打扰名单最先——被忽略的人怎么叫都不醒，但记录照旧。
    pub fn decide(&self, input: &WakeInput<'_>) -> WakeDecision {
        self.decide_with_reason(input).decision
    }

    /// 同 [`decide`](Self::decide)，但带上命中原因，给 agent 看「这次为什么醒」。
    pub fn decide_with_reason(&self, input: &WakeInput<'_>) -> WakeVerdict {
        if self
            .ignore_speakers
            .iter()
            .any(|speaker| speaker == input.sender_id)
        {
            return WakeVerdict::stay();
        }

        // 语义标签优先：适配器把“拍一拍 / reaction / 入群”翻译成跨平台标签。
        if self
            .ignore_event_tags
            .iter()
            .any(|tag| input.tags.iter().any(|candidate| candidate == tag))
        {
            return WakeVerdict::stay();
        }
        if let Some(tag) = self
            .event_tags
            .iter()
            .find(|tag| input.tags.iter().any(|candidate| candidate == tag))
        {
            return wake_with_backlog(input, format!("命中标签 {tag}"));
        }

        if let Some(event) = input.event {
            let qualified = format!("{}.{}", input.platform, event);
            let wildcard = format!("{}.*", input.platform);
            if self
                .events
                .iter()
                .any(|wanted| wanted == event || wanted == &qualified || wanted == &wildcard)
            {
                return wake_with_backlog(input, format!("命中事件 {event}"));
            }
            // 没点名的事件：只有冲我来的（拍到我）才按 at_mention 醒。
            // 这就是旧 `poke = true` 的语义；`poke = false` 由 ignore_event_tags 表达。
            if self.at_mention && input.at_self {
                return wake_with_backlog(input, format!("事件 {event} 指向我"));
            }
            // 表情回应是对某条消息的回应，和引用同类：被回应的是我说的话就叫醒。
            // 撤销反应（reaction_remove）不算新的互动，只记录不叫醒。
            if self.reply
                && input.reply_to_self
                && !input.tags.iter().any(|tag| *tag == "reaction_remove")
            {
                return wake_with_backlog(input, format!("事件 {event} 回应了我说的话"));
            }
            return WakeVerdict::stay();
        }

        if self.at_mention && input.at_self {
            return wake_with_backlog(input, "@ 了我");
        }
        // 引用我的话等同于对我说：QQ 里「回复」是最自然的追问方式，很多人不会再加 @。
        if self.reply && input.reply_to_self {
            return wake_with_backlog(input, "引用了我说的话");
        }
        if self.name_mention && matches_any(&self.names, input.text) {
            return wake_with_backlog(input, "叫了我的名字");
        }
        if matches_any(&self.keywords, input.text) {
            return wake_with_backlog(input, "命中关键词");
        }
        if let Some(threshold) = self.accumulate {
            if threshold > 0 && input.unread >= threshold {
                // 阈值和实际积压分开写：阈值是配置，实际条数才是这轮要处理的量。
                // 攒批没有「触发消息」：投递要按最旧的一批走，水位才能往前推。
                return WakeVerdict::wake_batch(wake_with_backlog_reason(
                    input,
                    format!("积压达到阈值（{threshold} 条）"),
                ));
            }
        }
        WakeVerdict::stay()
    }
}

/// 命中后统一补一句实际积压量。主因由调用方给，这里只把「涉及多少条」讲清楚。
fn wake_with_backlog(input: &WakeInput<'_>, reason: impl Into<String>) -> WakeVerdict {
    WakeVerdict::wake(wake_with_backlog_reason(input, reason))
}

/// 同上，但只拼文案；攒批唤醒要用 `wake_batch` 包装同一段文案。
fn wake_with_backlog_reason(input: &WakeInput<'_>, reason: impl Into<String>) -> String {
    let mut reason = reason.into();
    if input.unread > 1 {
        reason.push_str(&format!("，这轮共 {} 条未读", input.unread));
    }
    reason
}

/// TOML 的自由表 → JSON，原样交给 provider。core 不解释里面的键。
fn policy_options(
    table: toml::value::Table,
    label: &str,
) -> Result<serde_json::Value, ConfigError> {
    serde_json::to_value(table).map_err(|error| ConfigError::ParseFailed {
        detail: acb_i18n::t!(
            "kernel.config.policy_options_failed",
            label = label,
            error = error
        ),
    })
}

fn normalize_list(values: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    for value in values {
        let value = value.trim().to_owned();
        if !value.is_empty() && !normalized.contains(&value) {
            normalized.push(value);
        }
    }
    normalized
}

fn matches_any(names: &[String], text: &str) -> bool {
    names
        .iter()
        .any(|name| !name.trim().is_empty() && text.contains(name.as_str()))
}

/// 配置错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigError {
    #[error("{}", acb_i18n::t!("kernel.config.parse_failed", detail = detail))]
    ParseFailed { detail: String },
    #[error("{}", acb_i18n::t!("kernel.config.file_read_failed", path = path, detail = detail))]
    FileReadFailed { path: String, detail: String },
    #[error("{}", acb_i18n::t!("kernel.config.dir_read_failed", path = path, detail = detail))]
    DirReadFailed { path: String, detail: String },
    #[error("{}", acb_i18n::t!("kernel.config.file_parse_failed", path = path, detail = detail))]
    FileParseFailed { path: String, detail: String },
    #[error(
        "{}",
        acb_i18n::t!(
            "kernel.config.duplicate_across_files",
            kind = kind,
            id = id,
            first = first,
            second = second
        )
    )]
    DuplicateAcrossFiles {
        kind: String,
        id: String,
        first: String,
        second: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.empty_field", session = session, field = field))]
    EmptyField {
        session: String,
        field: &'static str,
    },
    #[error("{}", acb_i18n::t!("kernel.config.tasks_wait_max_ms", value = value))]
    TasksWaitMaxMs { value: u64 },
    #[error("{}", acb_i18n::t!("kernel.config.duplicate_session", session = session))]
    DuplicateSession { session: String },
    #[error("{}", acb_i18n::t!("kernel.config.duplicate_room", session = session, room = room))]
    DuplicateRoom { session: String, room: String },
    #[error("{}", acb_i18n::t!("kernel.config.bad_room", session = session, room = room, detail = detail))]
    BadRoom {
        session: String,
        room: String,
        detail: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.bad_delivery_window", session = session, room = room, field = field, value = value))]
    BadDeliveryWindow {
        session: String,
        room: String,
        field: String,
        value: usize,
    },
    #[error("{}", acb_i18n::t!("kernel.config.unknown_connection", session = session, room = room, connection = connection))]
    UnknownConnection {
        session: String,
        room: String,
        connection: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.bad_accumulate", session = session, room = room))]
    BadAccumulate { session: String, room: String },
    #[error("{}", acb_i18n::t!("kernel.config.bad_wake_event", session = session, room = room, event = event, platform = platform))]
    BadWakeEvent {
        session: String,
        room: String,
        event: String,
        platform: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.bad_wake_provider", session = session, room = room, provider = provider, detail = detail))]
    BadWakeProvider {
        session: String,
        room: String,
        provider: String,
        detail: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.bad_access_provider", session = session, provider = provider, detail = detail))]
    BadAccessProvider {
        session: String,
        provider: String,
        detail: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.removed_access_provider", session = session, provider = provider))]
    RemovedAccessProvider { session: String, provider: String },
    #[error("{}", acb_i18n::t!("kernel.config.bad_delivery_provider", session = session, provider = provider, detail = detail))]
    BadDeliveryProvider {
        session: String,
        provider: String,
        detail: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.bad_send_hook_provider", session = session, provider = provider, detail = detail))]
    BadSendHookProvider {
        session: String,
        provider: String,
        detail: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.bad_turn_error_provider", session = session, provider = provider, detail = detail))]
    BadTurnErrorProvider {
        session: String,
        provider: String,
        detail: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.bad_prompt_provider", session = session, provider = provider, detail = detail))]
    BadPromptProvider {
        session: String,
        provider: String,
        detail: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.bad_clock_provider", session = session, room = room, provider = provider, detail = detail))]
    BadClockProvider {
        session: String,
        room: String,
        provider: String,
        detail: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.empty_token", session = session))]
    EmptyToken { session: String },
    #[error("{}", acb_i18n::t!("kernel.config.unknown_persona", session = session, persona = persona))]
    UnknownPersona { session: String, persona: String },
    #[error("{}", acb_i18n::t!("kernel.config.unknown_memory", session = session, memory = memory))]
    UnknownMemory { session: String, memory: String },
    #[error("{}", acb_i18n::t!("kernel.config.duplicate_persona", persona = persona))]
    DuplicatePersona { persona: String },
    #[error("{}", acb_i18n::t!("kernel.config.empty_persona_field", persona = persona, field = field))]
    EmptyPersonaField {
        persona: String,
        field: &'static str,
    },
    #[error("{}", acb_i18n::t!("kernel.config.duplicate_memory", memory = memory))]
    DuplicateMemory { memory: String },
    #[error("{}", acb_i18n::t!("kernel.config.empty_memory_field", memory = memory, field = field))]
    EmptyMemoryField { memory: String, field: &'static str },
    #[error("{}", acb_i18n::t!("kernel.config.unknown_memory_kind", memory = memory, kind = kind))]
    UnknownMemoryKind { memory: String, kind: String },
    #[error("{}", acb_i18n::t!("kernel.config.duplicate_harness", harness = harness))]
    DuplicateHarness { harness: String },
    #[error("{}", acb_i18n::t!("kernel.config.empty_harness_field", harness = harness, field = field))]
    EmptyHarnessField {
        harness: String,
        field: &'static str,
    },
    #[error("{}", acb_i18n::t!("kernel.config.unknown_harness", session = session, harness = harness))]
    UnknownHarness { session: String, harness: String },
    /// 入库的业务对象（connections / sessions / personas / memories / harnesses）
    /// 原文有问题。五类对象存的是 TOML 原文，坏文本必须在加载时报出来，
    /// 不能等到用的时候才发现少一条会话。
    #[error("{}", acb_i18n::t!("kernel.config.object_not_array", kind = kind))]
    ObjectNotArray { kind: String },
    #[error("{}", acb_i18n::t!("kernel.config.object_missing_id", kind = kind))]
    ObjectMissingId { kind: String },
    #[error("{}", acb_i18n::t!("kernel.config.object_render_failed", kind = kind, id = id, detail = detail))]
    ObjectRenderFailed {
        kind: String,
        id: String,
        detail: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.object_parse_failed", kind = kind, detail = detail))]
    ObjectParseFailed { kind: String, detail: String },
    #[error("{}", acb_i18n::t!("kernel.config.object_snippet_shape", kind = kind, id = id))]
    ObjectSnippetShape { kind: String, id: String },
    #[error("{}", acb_i18n::t!("kernel.config.bad_listen", address = address, detail = detail))]
    BadListen { address: String, detail: String },
    #[error("{}", acb_i18n::t!("kernel.config.file_relay_max_publish_bytes", value = value, detail = detail))]
    BadFileRelayMaxPublish { value: String, detail: String },
    #[error("{}", acb_i18n::t!("kernel.config.empty_connection_id"))]
    EmptyConnectionId,
    #[error("{}", acb_i18n::t!("kernel.config.duplicate_connection", connection = connection))]
    DuplicateConnection { connection: String },
    #[error("{}", acb_i18n::t!("kernel.config.empty_connection_platform", connection = connection))]
    EmptyConnectionPlatform { connection: String },
    #[error("{}", acb_i18n::t!("kernel.config.empty_connection_kind", connection = connection))]
    EmptyConnectionKind { connection: String },
    #[error("{}", acb_i18n::t!("kernel.config.unknown_transport", connection = connection, transport = transport))]
    UnknownTransport {
        connection: String,
        transport: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.bad_url", connection = connection, url = url))]
    BadUrl { connection: String, url: String },
    #[error("{}", acb_i18n::t!("kernel.config.empty_account", connection = connection))]
    EmptyAccount { connection: String },
    #[error("{}", acb_i18n::t!("kernel.config.empty_plugin_path"))]
    EmptyPluginPath,
    #[error("{}", acb_i18n::t!("kernel.config.duplicate_plugin_path", path = path))]
    DuplicatePluginPath { path: String },
    #[error("{}", acb_i18n::t!("kernel.config.bad_tool_plugin_id", id = id))]
    BadToolPluginId { id: String },
    #[error("{}", acb_i18n::t!("kernel.config.reserved_tool_plugin_id", id = id))]
    ReservedToolPluginId { id: String },
    #[error("{}", acb_i18n::t!("kernel.config.duplicate_tool_plugin_id", id = id))]
    DuplicateToolPluginId { id: String },
    #[error("{}", acb_i18n::t!("kernel.config.empty_tool_plugin_command", id = id))]
    EmptyToolPluginCommand { id: String },
    #[error("{}", acb_i18n::t!("kernel.config.bad_tool_plugin_timeout", id = id, timeout_ms = timeout_ms))]
    BadToolPluginTimeout { id: String, timeout_ms: u64 },
    #[error("{}", acb_i18n::t!("kernel.config.bad_queue_limit", field = field, value = value))]
    BadQueueLimit { field: String, value: u64 },
    #[error("{}", acb_i18n::t!("kernel.config.bad_grant_config", detail = detail))]
    BadGrantConfig { detail: String },
    #[error("{}", acb_i18n::t!("kernel.config.legacy_allow_destructive", connection = connection))]
    LegacyAllowDestructive { connection: String },
    #[error("{}", acb_i18n::t!("kernel.config.legacy_policy_risk", connection = connection))]
    LegacyPolicyRisk { connection: String },
    #[error("{}", acb_i18n::t!("kernel.config.global_policy_unsupported"))]
    GlobalPolicyUnsupported,
    #[error("{}", acb_i18n::t!("kernel.config.bad_deps_config", field = field, value = value))]
    BadDepsConfig { field: String, value: u64 },
    #[error("{}", acb_i18n::t!("kernel.config.bad_lkg_config", field = field, value = value))]
    BadLkgConfig { field: String, value: u64 },
    #[error("{}", acb_i18n::t!("kernel.config.bad_provider_calls_timeout", timeout_ms = timeout_ms, max = MAX_PROVIDER_CALL_TIMEOUT_MS))]
    BadProviderCallsTimeout { timeout_ms: u64 },
    #[error("{}", acb_i18n::t!("kernel.config.bad_notifications_config", field = field, value = value))]
    BadNotificationsConfig { field: String, value: u64 },
    #[error("{}", acb_i18n::t!("kernel.config.bad_notifications_url", url = url))]
    BadNotificationsUrl { url: String },
    #[error("{}", acb_i18n::t!("kernel.config.bad_receive_provider", connection = connection, provider = provider, detail = detail))]
    BadReceiveProvider {
        connection: String,
        provider: String,
        detail: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.bad_route_provider", connection = connection, provider = provider, detail = detail))]
    BadRouteProvider {
        connection: String,
        provider: String,
        detail: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.empty_storage_path", default = DEFAULT_STORAGE_PATH))]
    EmptyStoragePath,
    #[error("{}", acb_i18n::t!("kernel.config.empty_worktrees_root", default = DEFAULT_WORKTREES_ROOT))]
    EmptyWorktreesRoot,
    #[error("{}", acb_i18n::t!("kernel.config.bad_turn_error_action", action = action))]
    BadTurnErrorAction { action: String },
    #[error("{}", acb_i18n::t!("kernel.config.secret_literal", field = field))]
    SecretLiteral { field: String },
    #[error("{}", acb_i18n::t!("kernel.config.secret_unknown", field = field, name = name))]
    UnknownSecret { field: String, name: String },
    #[error("{}", acb_i18n::t!("kernel.config.secret_duplicate", name = name))]
    DuplicateSecret { name: String },
    #[error("{}", acb_i18n::t!("kernel.config.secret_source_bad", name = name, kind = kind, detail = detail))]
    BadSecretSource {
        name: String,
        kind: String,
        detail: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.secret_env_missing", name = name, variable = variable))]
    MissingSecretEnv { name: String, variable: String },
    #[error("{}", acb_i18n::t!("kernel.config.secret_file_read", name = name, path = path, detail = detail))]
    BadSecretFile {
        name: String,
        path: String,
        detail: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.secret_file_insecure", name = name, path = path, detail = detail))]
    InsecureSecretFile {
        name: String,
        path: String,
        detail: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.secret_file_empty", name = name, origin = origin))]
    EmptySecret { name: String, origin: String },
    #[error("{}", acb_i18n::t!("kernel.config.secret_dotenv_bad", name = name, path = path, line = line, detail = detail))]
    BadSecretDotenv {
        name: String,
        path: String,
        line: usize,
        detail: String,
    },
    #[error("{}", acb_i18n::t!("kernel.config.console_secret_source"))]
    ConsoleSecretSource,
}

impl Config {
    /// 从 TOML 文本解析并校验。
    ///
    /// 配置里不再做任何文本插值：密钥字段写 `{ secret = "<来源名>" }`，
    /// 由加载期的 [`SecretStore`] 解析；读不到就报错，不静默变成空串。
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|error| ConfigError::ParseFailed {
            detail: error.to_string(),
        })?;
        Self::from_raw(raw, SecretMode::Strict)
    }

    /// 测试用解析入口：密钥引用未声明时按名字自身解析。
    ///
    /// 只在 `cfg(test)` 或显式启用 `test-fixtures` feature（测试/示例的 dev-dependencies
    /// 会自动开启）时编译；生产构建下这个方法不存在，误用会编译失败。
    /// 字面量密钥仍然和 [`Config::parse`] 一样被硬拒绝。
    #[cfg(any(test, feature = "test-fixtures"))]
    #[doc(hidden)]
    pub fn parse_fixture(text: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|error| ConfigError::ParseFailed {
            detail: error.to_string(),
        })?;
        Self::from_raw(raw, SecretMode::Fixture)
    }

    /// 从 base 文件加载，并按 `config_dir` 合并分片。
    ///
    /// `config_dir` 为 `None` 或目录不存在时等同于未启用分片：行为与 [`Config::parse`]
    /// 完全一致。目录存在时只读取其中的 `*.toml`（按文件名字典序），子目录与其他后缀
    /// 忽略；`90-console.toml` 最后加载并覆盖它包含的顶层键。
    pub fn load(base_path: &Path, config_dir: Option<&Path>) -> Result<LoadedConfig, ConfigError> {
        Self::load_mode(base_path, config_dir, SecretMode::Strict, None, None)
    }

    /// 加载时用库里的业务对象替换文件里的五类数组。
    ///
    /// `objects` 为 `None` 等同于 [`Config::load`]：完全按文件来（首次导入前的行为）。
    /// 传 `Some` 时文件里那几段不再参与——库是唯一源（2026-11-07 用户裁定：
    /// 业务对象只认库，`config.toml` 只当引导参数）。
    pub fn load_with_objects(
        base_path: &Path,
        config_dir: Option<&Path>,
        objects: Option<&crate::config_objects::ConfigObjects>,
    ) -> Result<LoadedConfig, ConfigError> {
        Self::load_mode(base_path, config_dir, SecretMode::Strict, objects, None)
    }

    /// 加载时把库里已解密的密钥当作来源值注入（P2.5 密钥入库）。
    ///
    /// `preset` 里的名字不再读文件/环境变量——文件被删掉也能启动，这正是
    /// 「值以库为准」的验收条件。名字命中时才覆盖，未命中的来源照旧按
    /// `[secrets]` 声明解析（fail-closed）。
    pub fn load_with_secrets(
        base_path: &Path,
        config_dir: Option<&Path>,
        objects: Option<&crate::config_objects::ConfigObjects>,
        preset: Option<&SecretPreset>,
    ) -> Result<LoadedConfig, ConfigError> {
        Self::load_mode(base_path, config_dir, SecretMode::Strict, objects, preset)
    }

    /// 测试用加载入口，语义同 [`Config::parse_fixture`]；同样只在测试构建里存在。
    #[cfg(any(test, feature = "test-fixtures"))]
    #[doc(hidden)]
    pub fn load_fixture(
        base_path: &Path,
        config_dir: Option<&Path>,
    ) -> Result<LoadedConfig, ConfigError> {
        Self::load_mode(base_path, config_dir, SecretMode::Fixture, None, None)
    }

    /// 测试用：带库对象注入的加载入口，语义同 [`Config::load_with_objects`]。
    #[cfg(any(test, feature = "test-fixtures"))]
    #[doc(hidden)]
    pub fn load_fixture_with_objects(
        base_path: &Path,
        config_dir: Option<&Path>,
        objects: Option<&crate::config_objects::ConfigObjects>,
    ) -> Result<LoadedConfig, ConfigError> {
        Self::load_mode(base_path, config_dir, SecretMode::Fixture, objects, None)
    }

    fn load_mode(
        base_path: &Path,
        config_dir: Option<&Path>,
        mode: SecretMode,
        objects: Option<&crate::config_objects::ConfigObjects>,
        preset: Option<&SecretPreset>,
    ) -> Result<LoadedConfig, ConfigError> {
        let base_text = read_config_file(base_path)?;
        let Some(config_dir) = config_dir.filter(|dir| dir.exists()) else {
            return load_single_with_objects(base_path, &base_text, mode, objects, preset);
        };
        if !config_dir.is_dir() {
            return Err(ConfigError::DirReadFailed {
                path: config_dir.display().to_string(),
                detail: acb_i18n::t!("kernel.config.not_a_directory"),
            });
        }
        let fragments = list_config_fragments(config_dir)?;
        let normal: Vec<PathBuf> = fragments
            .iter()
            .filter(|path| path.file_name() != Some(std::ffi::OsStr::new(CONSOLE_CONFIG_FILENAME)))
            .cloned()
            .collect();
        let console = fragments
            .iter()
            .find(|path| path.file_name() == Some(std::ffi::OsStr::new(CONSOLE_CONFIG_FILENAME)))
            .cloned();
        if normal.is_empty() && console.is_none() {
            // 目录存在但没有任何分片：保持旧路径，错误信息和解析行为都不变。
            return load_single_with_objects(base_path, &base_text, mode, objects, preset);
        }

        let mut files = vec![parse_config_file(base_path, &base_text)?];
        for path in &normal {
            let text = read_config_file(path)?;
            files.push(parse_config_file(path, &text)?);
        }
        check_duplicate_ids(&files)?;

        let console_layer = match console {
            Some(path) => {
                let text = read_config_file(&path)?;
                let parsed = parse_config_file(&path, &text)?;
                if parsed.value.get("secrets").is_some() {
                    return Err(ConfigError::ConsoleSecretSource);
                }
                Some(parsed)
            }
            None => None,
        };
        // 90-console 若自带 [grants]，它会整体替换下层 grants，只有它自己的规则
        // 生效，因此不存在跨层重复；否则比较 base + 普通分片。
        let duplicate_rules = if console_layer
            .as_ref()
            .is_some_and(|overlay| overlay.has_grants)
        {
            false
        } else {
            duplicate_rules_across_files(&files)
        };

        let mut merged = files[0].value.clone();
        for file in files.iter().skip(1) {
            merge_document(&mut merged, file.value.clone());
        }
        let mut sources: Vec<ConfigSource> = files.iter().map(|file| file.source.clone()).collect();

        if let Some(overlay) = console_layer {
            check_duplicate_ids(std::slice::from_ref(&overlay))?;
            apply_console_overlay(&mut merged, overlay.value.clone());
            sources.push(overlay.source);
        }
        if let Some(objects) = objects {
            objects.splice_into(&mut merged)?;
        }

        finish_loaded(merged, sources, duplicate_rules, mode, preset)
    }

    /// 用一段控制台文本作为 `90-console.toml` 的覆盖层，合并出最终配置。
    ///
    /// 供控制台在落盘前校验/暂存使用：不会写文件，只把这段文本当作最高优先级层。
    pub fn load_with_console_overlay(
        base_path: &Path,
        config_dir: &Path,
        overlay_text: &str,
        objects: Option<&crate::config_objects::ConfigObjects>,
    ) -> Result<LoadedConfig, ConfigError> {
        let base_text = read_config_file(base_path)?;
        let fragments = if config_dir.exists() {
            list_config_fragments(config_dir)?
        } else {
            Vec::new()
        };
        let normal: Vec<PathBuf> = fragments
            .iter()
            .filter(|path| path.file_name() != Some(std::ffi::OsStr::new(CONSOLE_CONFIG_FILENAME)))
            .cloned()
            .collect();

        let mut files = vec![parse_config_file(base_path, &base_text)?];
        for path in &normal {
            let text = read_config_file(path)?;
            files.push(parse_config_file(path, &text)?);
        }
        check_duplicate_ids(&files)?;

        let overlay_path = config_dir.join(CONSOLE_CONFIG_FILENAME);
        let overlay = parse_config_file(&overlay_path, overlay_text)?;
        if overlay.value.get("secrets").is_some() {
            return Err(ConfigError::ConsoleSecretSource);
        }
        let duplicate_rules = if overlay.has_grants {
            false
        } else {
            duplicate_rules_across_files(&files)
        };

        let mut merged = files[0].value.clone();
        for file in files.iter().skip(1) {
            merge_document(&mut merged, file.value.clone());
        }
        let mut sources: Vec<ConfigSource> = files.iter().map(|file| file.source.clone()).collect();

        check_duplicate_ids(std::slice::from_ref(&overlay))?;
        apply_console_overlay(&mut merged, overlay.value.clone());
        sources.push(overlay.source);
        if let Some(objects) = objects {
            objects.splice_into(&mut merged)?;
        }

        finish_loaded(merged, sources, duplicate_rules, SecretMode::Strict, None)
    }

    fn from_raw(raw: RawConfig, mode: SecretMode) -> Result<Self, ConfigError> {
        Self::from_raw_with_secrets(raw, mode, None).map(|(config, _)| config)
    }

    fn from_raw_with_secrets(
        raw: RawConfig,
        mode: SecretMode,
        preset: Option<&SecretPreset>,
    ) -> Result<(Self, SecretStore), ConfigError> {
        // 显式传入的优先；否则用启动期装好的库内快照（重载路径都会走到这里）。
        let preset: Option<&SecretPreset> = match preset {
            Some(preset) => Some(preset),
            None => crate::secrets_db::current(),
        };
        let secrets = SecretStore::from_raw(raw.secrets, mode, preset)?;
        if raw.policy.is_some() {
            return Err(ConfigError::GlobalPolicyUnsupported);
        }
        let default_on_turn_error = parse_turn_error_actions(&raw.default_on_turn_error)?;

        let storage = match raw.storage {
            Some(raw) => {
                let path = raw.path.trim().to_owned();
                if path.is_empty() {
                    return Err(ConfigError::EmptyStoragePath);
                }
                StorageConfig { path }
            }
            None => StorageConfig::default(),
        };

        let retention = match raw.retention {
            Some(raw) => RetentionConfig {
                max_records_per_room: raw
                    .max_records_per_room
                    .unwrap_or(DEFAULT_MAX_RECORDS_PER_ROOM),
                max_age_days: raw.max_age_days.unwrap_or(DEFAULT_MAX_AGE_DAYS),
            },
            None => RetentionConfig::default(),
        };

        let queues = match raw.queues {
            Some(raw) => {
                let inbound = queue_limit("inbound", raw.inbound, DEFAULT_INBOUND_QUEUE)?;
                let route = queue_limit("route", raw.route, DEFAULT_ROUTE_QUEUE)?;
                QueueConfig { inbound, route }
            }
            None => QueueConfig::default(),
        };

        let grants = match raw.grants {
            Some(grants) => {
                grants
                    .validate()
                    .map_err(|detail| ConfigError::BadGrantConfig { detail })?;
                grants
            }
            None => crate::grants::GrantsConfig::default(),
        };

        let tickets = raw.tickets.unwrap_or_default();

        let deps = match raw.deps {
            Some(raw) => DepsConfig {
                failure_threshold: deps_limit(
                    "failure_threshold",
                    raw.failure_threshold,
                    DEFAULT_DEPS_FAILURE_THRESHOLD,
                )?,
                failure_window_secs: deps_limit(
                    "failure_window_secs",
                    raw.failure_window_secs,
                    DEFAULT_DEPS_FAILURE_WINDOW_SECS,
                )?,
            },
            None => DepsConfig::default(),
        };

        let lkg = match raw.lkg {
            Some(raw) => LkgConfig {
                max_failures: lkg_limit(
                    "max_failures",
                    raw.max_failures,
                    DEFAULT_LKG_MAX_FAILURES,
                )?,
                window_secs: lkg_limit("window_secs", raw.window_secs, DEFAULT_LKG_WINDOW_SECS)?,
            },
            None => LkgConfig::default(),
        };

        let provider_calls = match raw.provider_calls {
            Some(raw) => {
                let timeout_ms = raw.timeout_ms.unwrap_or(DEFAULT_PROVIDER_CALL_TIMEOUT_MS);
                if timeout_ms > MAX_PROVIDER_CALL_TIMEOUT_MS {
                    return Err(ConfigError::BadProviderCallsTimeout { timeout_ms });
                }
                ProviderCallsConfig { timeout_ms }
            }
            None => ProviderCallsConfig::default(),
        };

        let notifications = match raw.notifications {
            Some(raw) => {
                let url = raw
                    .url
                    .map(|url| url.trim().trim_end_matches('/').to_owned())
                    .filter(|url| !url.is_empty());
                let topic_source = match raw.topic.as_ref() {
                    Some(RawSecretValue::Reference(reference)) => Some(reference.secret.clone()),
                    _ => None,
                };
                let topic = secrets
                    .resolve_opt(raw.topic, "notifications.topic")?
                    .map(|topic| topic.trim().trim_start_matches('/').to_owned())
                    .filter(|topic| !topic.is_empty());
                let token = secrets
                    .resolve_opt(raw.token, "notifications.token")?
                    .map(|token| token.trim().to_owned())
                    .filter(|token| !token.is_empty());
                if raw.enabled && url.is_some() && topic.is_some() {
                    let value = url.as_deref().unwrap_or_default();
                    if !value.starts_with("http://") && !value.starts_with("https://") {
                        return Err(ConfigError::BadNotificationsUrl {
                            url: value.to_owned(),
                        });
                    }
                }
                let min_interval_secs = notice_limit(
                    "min_interval_secs",
                    raw.min_interval_secs,
                    DEFAULT_NOTIFICATION_MIN_INTERVAL_SECS,
                )?;
                let in_band = InBandNoticeConfig {
                    window_secs: notice_limit(
                        "in_band.window_secs",
                        raw.in_band.window_secs,
                        DEFAULT_IN_BAND_WINDOW_SECS,
                    )?,
                    room_limit: notice_limit(
                        "in_band.room_limit",
                        raw.in_band.room_limit,
                        DEFAULT_IN_BAND_ROOM_LIMIT,
                    )?,
                    max_entries: notice_limit(
                        "in_band.max_entries",
                        raw.in_band.max_entries,
                        DEFAULT_IN_BAND_MAX_ENTRIES,
                    )?,
                    remind_secs: notice_limit(
                        "in_band.remind_secs",
                        raw.in_band.remind_secs,
                        DEFAULT_IN_BAND_REMIND_SECS,
                    )?,
                };
                Some(NotificationsConfig {
                    enabled: raw.enabled,
                    url,
                    topic,
                    topic_source,
                    token,
                    min_interval_secs,
                    in_band,
                })
            }
            None => None,
        };

        let worktrees_root = match raw.worktrees_root {
            Some(raw) => {
                let root = raw.trim().to_owned();
                if root.is_empty() {
                    return Err(ConfigError::EmptyWorktreesRoot);
                }
                root
            }
            None => DEFAULT_WORKTREES_ROOT.to_owned(),
        };

        let endpoint = match raw.endpoint {
            Some(raw) => {
                let address = raw.listen.trim().to_owned();
                // 解析不了就别启动：监听地址写错了，等到运行时才发现只会更晚。
                if address.parse::<std::net::SocketAddr>().is_err() {
                    return Err(ConfigError::BadListen {
                        address,
                        detail: acb_i18n::t!("kernel.config.listen_host_port"),
                    });
                }
                Some(EndpointConfig { listen: address })
            }
            None => None,
        };

        let console = match raw.console {
            Some(raw) => {
                let address = raw.listen.trim().to_owned();
                if address.parse::<std::net::SocketAddr>().is_err() {
                    return Err(ConfigError::BadListen {
                        address,
                        detail: acb_i18n::t!("kernel.config.console_host_port"),
                    });
                }
                let token = secrets
                    .resolve(&raw.token, "console.token")?
                    .trim()
                    .to_owned();
                if token.is_empty() {
                    return Err(ConfigError::BadListen {
                        address,
                        detail: acb_i18n::t!("kernel.config.console_token_empty"),
                    });
                }
                Some(ConsoleConfig {
                    listen: address,
                    token,
                })
            }
            None => None,
        };

        let file_relay = match raw.file_relay {
            Some(raw) => {
                let mode = raw
                    .mode
                    .as_deref()
                    .map(str::trim)
                    .filter(|mode| !mode.is_empty())
                    .unwrap_or(FILE_RELAY_MODE_FILE_SERVER)
                    .to_owned();
                if mode != FILE_RELAY_MODE_FILE_SERVER {
                    return Err(ConfigError::BadListen {
                        address: mode.clone(),
                        detail: acb_i18n::t!(
                            "kernel.config.file_relay_mode",
                            mode = FILE_RELAY_MODE_FILE_SERVER
                        ),
                    });
                }
                let address = raw.listen.trim().to_owned();
                if address.parse::<std::net::SocketAddr>().is_err() {
                    return Err(ConfigError::BadListen {
                        address,
                        detail: acb_i18n::t!("kernel.config.file_relay_host_port"),
                    });
                }
                let public_base = raw.public_base.trim().trim_end_matches('/').to_owned();
                if !public_base.starts_with("http://") && !public_base.starts_with("https://") {
                    return Err(ConfigError::BadListen {
                        address,
                        detail: acb_i18n::t!("kernel.config.file_relay_public_base"),
                    });
                }
                let max_publish_bytes = parse_file_relay_max_publish_bytes(raw.max_publish_bytes)?;
                Some(FileRelayConfig {
                    mode,
                    listen: address,
                    public_base,
                    dir: raw.dir.unwrap_or_else(|| DEFAULT_RELAY_DIR.to_owned()),
                    max_publish_bytes,
                })
            }
            None => None,
        };

        let mut plugins: Vec<PluginConfig> = Vec::with_capacity(raw.plugins.len());
        for raw_plugin in raw.plugins {
            let path = raw_plugin.path.trim().to_owned();
            if path.is_empty() {
                return Err(ConfigError::EmptyPluginPath);
            }
            if plugins.iter().any(|plugin| plugin.path == path) {
                return Err(ConfigError::DuplicatePluginPath { path });
            }
            let kind = raw_plugin
                .kind
                .map(|kind| kind.trim().to_owned())
                .filter(|kind| !kind.is_empty());
            let version = raw_plugin
                .version
                .map(|version| version.trim().to_owned())
                .filter(|version| !version.is_empty());
            plugins.push(PluginConfig {
                path,
                kind,
                timeout_ms: raw_plugin.timeout_ms,
                version,
            });
        }

        let mut tool_plugins: Vec<ToolPluginConfig> = Vec::with_capacity(raw.tool_plugins.len());
        for raw_plugin in raw.tool_plugins {
            let id = raw_plugin.id.trim().to_owned();
            let parsed = ProviderId::parse(&id)
                .map_err(|_| ConfigError::BadToolPluginId { id: id.clone() })?;
            if parsed.namespace() == "acb" {
                return Err(ConfigError::ReservedToolPluginId { id });
            }
            if tool_plugins.iter().any(|plugin| plugin.id == id) {
                return Err(ConfigError::DuplicateToolPluginId { id });
            }
            let command = raw_plugin.command.trim().to_owned();
            if command.is_empty() {
                return Err(ConfigError::EmptyToolPluginCommand { id });
            }
            if let Some(timeout_ms) = raw_plugin.timeout_ms {
                if !(MIN_TOOL_PLUGIN_TIMEOUT_MS..=MAX_TOOL_PLUGIN_TIMEOUT_MS).contains(&timeout_ms)
                {
                    return Err(ConfigError::BadToolPluginTimeout { id, timeout_ms });
                }
            }
            let args = raw_plugin
                .args
                .into_iter()
                .map(|arg| arg.trim().to_owned())
                .filter(|arg| !arg.is_empty())
                .collect();
            tool_plugins.push(ToolPluginConfig {
                id,
                command,
                args,
                timeout_ms: raw_plugin.timeout_ms,
                env: Vec::new(),
            });
        }

        let mut connections: Vec<Connection> = Vec::with_capacity(raw.connections.len());
        for raw_connection in raw.connections {
            let connection = build_connection(raw_connection, &secrets)?;
            if connections.iter().any(|seen| seen.id == connection.id) {
                return Err(ConfigError::DuplicateConnection {
                    connection: connection.id,
                });
            }
            connections.push(connection);
        }

        // 人格：一张卡一份，id 唯一。
        let mut personas: Vec<Persona> = Vec::with_capacity(raw.personas.len());
        for raw_persona in raw.personas {
            let id = raw_persona.id.trim().to_owned();
            if id.is_empty() {
                return Err(ConfigError::EmptyPersonaField {
                    persona: id,
                    field: "id",
                });
            }
            if personas.iter().any(|seen| seen.id == id) {
                return Err(ConfigError::DuplicatePersona { persona: id });
            }
            let name = raw_persona.name.trim().to_owned();
            if name.is_empty() {
                return Err(ConfigError::EmptyPersonaField {
                    persona: id,
                    field: "name",
                });
            }
            let text = raw_persona.text.trim().to_owned();
            if text.is_empty() {
                return Err(ConfigError::EmptyPersonaField {
                    persona: id,
                    field: "text",
                });
            }
            personas.push(Persona { id, name, text });
        }

        // 记忆：一份一个 id，kind 目前只有 git。
        let mut memories: Vec<Memory> = Vec::with_capacity(raw.memories.len());
        for raw_memory in raw.memories {
            let id = raw_memory.id.trim().to_owned();
            if id.is_empty() {
                return Err(ConfigError::EmptyMemoryField {
                    memory: id,
                    field: "id",
                });
            }
            if memories.iter().any(|seen| seen.id == id) {
                return Err(ConfigError::DuplicateMemory { memory: id });
            }
            let name = raw_memory.name.trim().to_owned();
            if name.is_empty() {
                return Err(ConfigError::EmptyMemoryField {
                    memory: id,
                    field: "name",
                });
            }
            let kind = raw_memory.kind.trim().to_owned();
            if !KNOWN_MEMORY_KINDS.contains(&kind.as_str()) {
                return Err(ConfigError::UnknownMemoryKind { memory: id, kind });
            }
            let location = raw_memory.location.trim().to_owned();
            if location.is_empty() {
                return Err(ConfigError::EmptyMemoryField {
                    memory: id,
                    field: "location",
                });
            }
            memories.push(Memory {
                id,
                name,
                kind,
                location,
            });
        }

        // harness：一等对象，一个 id 一个。会话靠它引用「归谁接管」。
        let mut harnesses: Vec<Harness> = Vec::with_capacity(raw.harnesses.len());
        for raw_harness in raw.harnesses {
            let id = raw_harness.id.trim().to_owned();
            if id.is_empty() {
                return Err(ConfigError::EmptyHarnessField {
                    harness: id,
                    field: "id",
                });
            }
            if harnesses.iter().any(|seen| seen.id == id) {
                return Err(ConfigError::DuplicateHarness { harness: id });
            }
            let name = raw_harness.name.trim().to_owned();
            if name.is_empty() {
                return Err(ConfigError::EmptyHarnessField {
                    harness: id,
                    field: "name",
                });
            }
            harnesses.push(Harness { id, name });
        }

        let mut sessions = Vec::with_capacity(raw.sessions.len());
        let mut seen_sessions: Vec<String> = Vec::new();
        let mut seen_driver_ports: Vec<(u16, String)> = Vec::new();

        for raw_session in raw.sessions {
            let id = raw_session.id;
            if seen_sessions.iter().any(|seen| seen == &id) {
                return Err(ConfigError::DuplicateSession { session: id });
            }
            seen_sessions.push(id.clone());

            let driver_port = raw_session.driver_port;
            if let Some(port) = driver_port {
                if port < 1024 {
                    return Err(ConfigError::ParseFailed {
                        detail: format!(
                            "session {id}: driver_port {port} is below 1024; use an unprivileged port"
                        ),
                    });
                }
                if let Some((_, first)) = seen_driver_ports.iter().find(|(seen, _)| *seen == port) {
                    return Err(ConfigError::ParseFailed {
                        detail: format!(
                            "session {id}: driver_port {port} is already used by session {first}"
                        ),
                    });
                }
                seen_driver_ports.push((port, id.clone()));
            }

            check_not_blank(&id, "id").map_err(|field| ConfigError::EmptyField {
                session: id.clone(),
                field,
            })?;
            check_not_blank(&raw_session.harness, "harness").map_err(|field| {
                ConfigError::EmptyField {
                    session: id.clone(),
                    field,
                }
            })?;
            check_not_blank(&raw_session.persona, "persona").map_err(|field| {
                ConfigError::EmptyField {
                    session: id.clone(),
                    field,
                }
            })?;
            let token = secrets
                .resolve(&raw_session.token, &format!("sessions[{id}].token"))?
                .trim()
                .to_owned();
            check_not_blank(&token, "token").map_err(|_| ConfigError::EmptyToken {
                session: id.clone(),
            })?;

            // 引用的 harness 必须真的存在。
            let harness = raw_session.harness.trim().to_owned();
            if !harnesses.iter().any(|h| h.id == harness) {
                return Err(ConfigError::UnknownHarness {
                    session: id.clone(),
                    harness,
                });
            }

            // 引用的人格与记忆必须真的存在。
            if !personas
                .iter()
                .any(|persona| persona.id == raw_session.persona)
            {
                return Err(ConfigError::UnknownPersona {
                    session: id.clone(),
                    persona: raw_session.persona.clone(),
                });
            }
            for memory in &raw_session.memories {
                if !memories.iter().any(|m| &m.id == memory) {
                    return Err(ConfigError::UnknownMemory {
                        session: id.clone(),
                        memory: memory.clone(),
                    });
                }
            }

            // 会话级投递窗口默认值：订阅没写的字段用它，再没写就用 3/3/3。
            let session_window = DeliveryWindow {
                context_before: raw_session
                    .context_before
                    .unwrap_or(DEFAULT_CONTEXT_RECORDS),
                context_after: raw_session.context_after.unwrap_or(DEFAULT_CONTEXT_RECORDS),
                batch_head: raw_session.batch_head.unwrap_or(DEFAULT_CONTEXT_RECORDS),
            };
            validate_window(&id, "", &session_window)?;

            let mut subscriptions = Vec::with_capacity(raw_session.subscriptions.len());
            let mut seen_rooms: Vec<RoomId> = Vec::new();
            for raw_subscription in raw_session.subscriptions {
                let subscription = build_subscription(&id, session_window, raw_subscription)?;
                // 订阅连的连接必须真的存在，否则这个房间永远发不出去。
                let Some(connection) = connections
                    .iter()
                    .find(|connection| connection.id == subscription.room.connection)
                else {
                    return Err(ConfigError::UnknownConnection {
                        session: id.clone(),
                        room: subscription.room.to_string(),
                        connection: subscription.room.connection.clone(),
                    });
                };
                // 唤醒事件里带平台前缀的，必须属于这个房间所属的平台。
                validate_wake_events(
                    &id,
                    &subscription.room,
                    &subscription.wake,
                    &connection.platform,
                )?;
                if seen_rooms.iter().any(|room| room == &subscription.room) {
                    return Err(ConfigError::DuplicateRoom {
                        session: id.clone(),
                        room: subscription.room.to_string(),
                    });
                }
                seen_rooms.push(subscription.room.clone());
                subscriptions.push(subscription);
            }

            // name 可空：缺省 = id。model 可空：由 harness 侧决定。
            let name = {
                let trimmed = raw_session.name.trim().to_owned();
                if trimmed.is_empty() {
                    id.clone()
                } else {
                    trimmed
                }
            };
            let model = raw_session.model.and_then(|m| {
                let trimmed = m.trim().to_owned();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            });

            let on_turn_error = parse_turn_error_actions(&raw_session.on_turn_error)?;
            let access = build_access(&id, raw_session.access)?;
            let delivery = build_delivery(&id, raw_session.delivery)?;
            let outbound = build_outbound(&id, raw_session.outbound)?;
            let turn_error = build_turn_error(&id, raw_session.turn_error)?;
            let prompts = build_prompts(&id, raw_session.prompts)?;

            sessions.push(Session {
                id,
                harness,
                name,
                model,
                driver_port,
                persona: raw_session.persona,
                memories: raw_session.memories,
                token,
                subscriptions,
                access,
                delivery,
                outbound,
                turn_error,
                on_turn_error,
                prompts,
            });
        }

        let tasks = match raw.tasks {
            Some(raw) => {
                let wait_max_ms = raw.wait_max_ms.unwrap_or(DEFAULT_TASK_WAIT_MAX_MS);
                if wait_max_ms == 0 {
                    return Err(ConfigError::TasksWaitMaxMs { value: wait_max_ms });
                }
                TasksConfig {
                    wake_on_completion: raw.wake_on_completion,
                    wait_max_ms,
                }
            }
            None => TasksConfig::default(),
        };

        let config = Self {
            personas,
            memories,
            harnesses,
            sessions,
            endpoint,
            console,
            file_relay,
            connections,
            plugins,
            tool_plugins,
            storage,
            retention,
            queues,
            grants,
            tickets,
            tasks,
            deps,
            lkg,
            provider_calls,
            notifications,
            worktrees_root,
            default_on_turn_error,
            delivery_limit: raw.delivery_limit.unwrap_or(DEFAULT_DELIVERY_LIMIT),
            turn_timeout_secs: raw.turn_timeout_secs.unwrap_or(DEFAULT_TURN_TIMEOUT_SECS),
        };
        Ok((config, secrets))
    }

    /// 找某条连接的配置。
    pub fn connection(&self, id: &str) -> Option<&Connection> {
        self.connections
            .iter()
            .find(|connection| connection.id == id)
    }

    /// 找一张人格卡。
    pub fn persona(&self, id: &str) -> Option<&Persona> {
        self.personas.iter().find(|persona| persona.id == id)
    }

    /// 找一份记忆。
    pub fn memory(&self, id: &str) -> Option<&Memory> {
        self.memories.iter().find(|memory| memory.id == id)
    }

    /// 找一个 harness。
    pub fn harness(&self, id: &str) -> Option<&Harness> {
        self.harnesses.iter().find(|harness| harness.id == id)
    }

    /// 找一个会话。
    pub fn session(&self, id: &str) -> Option<&Session> {
        self.sessions.iter().find(|session| session.id == id)
    }

    /// 找会话的某条订阅。发送与工具鉴权都从这里查。
    pub fn subscription(&self, session: &str, room: &RoomId) -> Option<&Subscription> {
        self.sessions
            .iter()
            .find(|candidate| candidate.id == session)?
            .subscriptions
            .iter()
            .find(|subscription| &subscription.room == room)
    }

    /// 哪（几）个会话订阅了这个房间。
    pub fn subscribers_of(&self, room: &RoomId) -> Vec<&Session> {
        self.sessions
            .iter()
            .filter(|session| {
                session
                    .subscriptions
                    .iter()
                    .any(|subscription| &subscription.room == room)
            })
            .collect()
    }

    /// 房间所属连接的登录账号。房间里没有这个连接就返回空串。
    pub fn account_of_room(&self, room: &RoomId) -> String {
        self.connection(&room.connection)
            .map(|connection| connection.account.clone())
            .unwrap_or_default()
    }

    /// 这个号是不是房间所属连接点名的系统管理员。
    pub fn is_admin(&self, room: &RoomId, sender_id: &str) -> bool {
        self.connection(&room.connection)
            .map(|connection| connection.admin_qqs.iter().any(|qq| qq == sender_id))
            .unwrap_or(false)
    }

    /// 按 harness 名找它接管的会话。握手时用：harness 报出名字 + 令牌，
    /// 令牌对上，这条连接就代表这个名字下它接管的那组会话。
    pub fn sessions_by_harness(&self, harness: &str) -> Vec<&Session> {
        self.sessions
            .iter()
            .filter(|candidate| candidate.harness == harness)
            .collect()
    }

    /// 启动时要打给人看的提醒。都不阻止启动——用户故意的配置也得能跑。
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        // 同一房间被多个会话订阅：谁说话就说不准了，发消息时要指明会话。
        let mut rooms: Vec<(String, Vec<&str>)> = Vec::new();
        for session in &self.sessions {
            for subscription in &session.subscriptions {
                let name = subscription.room.to_string();
                match rooms.iter_mut().find(|(room, _)| *room == name) {
                    Some((_, sessions)) => sessions.push(session.id.as_str()),
                    None => rooms.push((name, vec![session.id.as_str()])),
                }
            }
        }
        for (room, sessions) in rooms {
            if sessions.len() > 1 {
                warnings.push(acb_i18n::t!(
                    "kernel.config.shared_room",
                    room = room,
                    sessions = sessions.join(", ")
                ));
            }
        }

        // URL 查询串里的 token 不属于密钥来源抽象；提醒拆到 token 字段。
        // 只报连接 id 和参数名，绝不把 URL 或参数值写进提醒。
        for connection in &self.connections {
            let Some(url) = connection.url.as_deref() else {
                continue;
            };
            let Some((_, query)) = url.split_once('?') else {
                continue;
            };
            let sensitive = query
                .split('&')
                .filter_map(|pair| {
                    let key = pair
                        .split_once('=')
                        .map(|(key, _)| key)
                        .unwrap_or(pair)
                        .trim();
                    (key.eq_ignore_ascii_case("token") || key.eq_ignore_ascii_case("access_token"))
                        .then_some(key)
                })
                .next();
            if let Some(key) = sensitive {
                warnings.push(acb_i18n::t!(
                    "kernel.config.url_token_query",
                    connection = connection.id,
                    key = key
                ));
            }
        }

        warnings
    }
}

/// 适配器 `kind` 的规范值。`onebot-snowluma` 是 `onebot11` 的别名；
/// 解析后统一落回 `onebot11`，避免改变持久化标识。
fn canonical_adapter_kind(kind: &str) -> &str {
    match kind {
        "onebot-snowluma" => "onebot11",
        other => other,
    }
}

fn build_connection(raw: RawConnection, secrets: &SecretStore) -> Result<Connection, ConfigError> {
    let id = raw.id.trim().to_owned();
    if id.is_empty() {
        return Err(ConfigError::EmptyConnectionId);
    }

    // 旧配置面已移除：出现即启动失败，并给出迁移写法。
    if raw.allow_destructive.is_some() {
        return Err(ConfigError::LegacyAllowDestructive { connection: id });
    }
    if raw.policy.is_some() {
        return Err(ConfigError::LegacyPolicyRisk { connection: id });
    }

    let platform = raw.platform.trim().to_owned();
    if platform.is_empty() {
        return Err(ConfigError::EmptyConnectionPlatform { connection: id });
    }

    // 适配器种类由注册表在启动时校验：外置/动态适配器可以是任意 namespace.name。
    // `onebot-snowluma` 只是 `onebot11` 的显示别名；内部一律落回规范值，
    // 避免 provider id、LKG 与旧配置的持久化标识失配。
    let kind = canonical_adapter_kind(raw.kind.trim()).to_owned();
    if kind.is_empty() {
        return Err(ConfigError::EmptyConnectionKind { connection: id });
    }

    let transport_text = raw.transport.trim().to_owned();
    let transport =
        Transport::parse(&transport_text).ok_or_else(|| ConfigError::UnknownTransport {
            connection: id.clone(),
            transport: transport_text.clone(),
        })?;

    let url = raw
        .url
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    // ws-client 必须有不加密的 ws:// 地址；其它连接方式（process / 平台自定义）
    // 由适配器自己的 manifest 校验。
    if transport == Transport::WsClient {
        let Some(address) = url.as_deref() else {
            return Err(ConfigError::BadUrl {
                connection: id,
                url: acb_i18n::t!("kernel.config.empty_url"),
            });
        };
        if !(address.starts_with("ws://") && address.len() > "ws://".len()) {
            return Err(ConfigError::BadUrl {
                connection: id,
                url: address.to_owned(),
            });
        }
    }

    let account = raw.account.map(|value| value.trim().to_owned());
    let account = match account {
        Some(account) if !account.is_empty() => account,
        _ => return Err(ConfigError::EmptyAccount { connection: id }),
    };

    let options = serde_json::to_value(&raw.options).map_err(|error| ConfigError::ParseFailed {
        detail: acb_i18n::t!(
            "kernel.config.connection_options_failed",
            connection = id,
            error = error
        ),
    })?;

    let receive = build_receive(&id, raw.receive)?;
    let routes = build_routes(&id, raw.routes)?;
    let token = secrets
        .resolve_opt(raw.token, &format!("connections[{id}].token"))?
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty());
    Ok(Connection {
        id,
        platform,
        kind,
        transport,
        url,
        options,
        token,
        account,
        admin_qqs: raw
            .admin_qqs
            .into_iter()
            .map(|qq| qq.trim().to_owned())
            .filter(|qq| !qq.is_empty())
            .collect(),
        receive,
        routes,
    })
}

fn build_routes(connection: &str, raw: Vec<RawRoute>) -> Result<Vec<RoutePolicy>, ConfigError> {
    let mut routes = Vec::with_capacity(raw.len());
    for (index, route) in raw.into_iter().enumerate() {
        let provider = match route
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            Some(text) => {
                ProviderId::parse(text).map_err(|error| ConfigError::BadRouteProvider {
                    connection: connection.to_owned(),
                    provider: text.to_owned(),
                    detail: error.to_string(),
                })?
            }
            None => {
                return Err(ConfigError::BadRouteProvider {
                    connection: connection.to_owned(),
                    provider: format!("routes[{index}]"),
                    detail: acb_i18n::t!("kernel.config.provider_empty").to_owned(),
                });
            }
        };
        routes.push(RoutePolicy {
            provider,
            options: policy_options(
                route.options,
                &acb_i18n::t!("kernel.config.label.route_policy"),
            )?,
        });
    }
    Ok(routes)
}

fn build_receive(connection: &str, raw: RawReceive) -> Result<ReceivePolicy, ConfigError> {
    let provider = match raw
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        Some(text) => ProviderId::parse(text).map_err(|error| ConfigError::BadReceiveProvider {
            connection: connection.to_owned(),
            provider: text.to_owned(),
            detail: error.to_string(),
        })?,
        None => ProviderId::builtin_receive_all(),
    };
    Ok(ReceivePolicy {
        provider,
        options: policy_options(
            raw.options,
            &acb_i18n::t!("kernel.config.label.receive_policy"),
        )?,
    })
}

fn build_subscription(
    session: &str,
    session_window: DeliveryWindow,
    raw: RawSubscription,
) -> Result<Subscription, ConfigError> {
    let room = RoomId::new(raw.connection.trim(), raw.room.trim()).map_err(|error| {
        ConfigError::BadRoom {
            session: session.to_owned(),
            room: format!("{}:{}", raw.connection.trim(), raw.room.trim()),
            detail: error.to_string(),
        }
    })?;

    let wake = build_wake(session, &room, raw.wake.unwrap_or_default())?;
    let clock = build_clock(session, &room, raw.clock)?;
    // 逐项覆盖：订阅写了的用自己的，没写的继承会话级默认值。
    let window = DeliveryWindow {
        context_before: raw.context_before.unwrap_or(session_window.context_before),
        context_after: raw.context_after.unwrap_or(session_window.context_after),
        batch_head: raw.batch_head.unwrap_or(session_window.batch_head),
    };
    validate_window(session, &room.to_string(), &window)?;

    Ok(Subscription {
        room,
        wake,
        clock,
        window,
    })
}

/// 投递窗口的边界检查：正文条数可以有 0（不要上下文），但时钟唤醒至少投 1 条，
/// 否则「看一眼」的轮次会没有任何正文。
fn validate_window(session: &str, room: &str, window: &DeliveryWindow) -> Result<(), ConfigError> {
    for (field, value, minimum) in [
        ("context_before", window.context_before, 0),
        ("context_after", window.context_after, 0),
        ("batch_head", window.batch_head, 1),
    ] {
        if value < minimum || value > MAX_CONTEXT_RECORDS {
            return Err(ConfigError::BadDeliveryWindow {
                session: session.to_owned(),
                room: room.to_owned(),
                field: field.to_owned(),
                value,
            });
        }
    }
    Ok(())
}

fn build_clock(
    session: &str,
    room: &RoomId,
    raw: Option<RawClock>,
) -> Result<ClockPolicy, ConfigError> {
    let raw = raw.unwrap_or_default();
    let has_thresholds = raw.interval_secs.is_some() || raw.idle_secs.is_some();
    let provider = match raw
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        Some(text) => ProviderId::parse(text).map_err(|error| ConfigError::BadClockProvider {
            session: session.to_owned(),
            room: room.to_string(),
            provider: text.to_owned(),
            detail: error.to_string(),
        })?,
        // 写了间隔/空闲却没写 provider：直接当成内置时钟，不用再教一遍。
        None if has_thresholds => ProviderId::builtin_clock_interval_idle(),
        None => ProviderId::builtin_clock_off(),
    };
    Ok(ClockPolicy {
        provider,
        interval_secs: raw.interval_secs,
        idle_secs: raw.idle_secs,
        options: policy_options(
            raw.options,
            &acb_i18n::t!("kernel.config.label.clock_policy"),
        )?,
    })
}

fn build_access(session: &str, raw: RawAccess) -> Result<AccessPolicy, ConfigError> {
    let provider = match raw
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        Some(text) => {
            if is_removed_access_provider(text) {
                return Err(ConfigError::RemovedAccessProvider {
                    session: session.to_owned(),
                    provider: text.to_owned(),
                });
            }
            ProviderId::parse(text).map_err(|error| ConfigError::BadAccessProvider {
                session: session.to_owned(),
                provider: text.to_owned(),
                detail: error.to_string(),
            })?
        }
        None => ProviderId::builtin_grants_access(),
    };

    Ok(AccessPolicy {
        provider,
        options: policy_options(
            raw.options,
            &acb_i18n::t!("kernel.config.label.access_policy"),
        )?,
    })
}

/// 已移除的内置 access provider。旧配置留着这些键会直接启动失败。
fn is_removed_access_provider(provider: &str) -> bool {
    matches!(
        provider,
        "acb.access.all_rooms" | "acb.access.elevated_in_rooms" | "acb.access.privileged_origins"
    )
}

fn build_delivery(session: &str, raw: RawDelivery) -> Result<DeliveryPolicy, ConfigError> {
    let provider = match raw
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        Some(text) => {
            ProviderId::parse(text).map_err(|error| ConfigError::BadDeliveryProvider {
                session: session.to_owned(),
                provider: text.to_owned(),
                detail: error.to_string(),
            })?
        }
        None => ProviderId::builtin_delivery(),
    };
    Ok(DeliveryPolicy {
        provider,
        options: policy_options(
            raw.options,
            &acb_i18n::t!("kernel.config.label.delivery_policy"),
        )?,
    })
}

fn build_outbound(session: &str, raw: RawOutbound) -> Result<OutboundPolicy, ConfigError> {
    let parse_hooks = |ids: Vec<String>| -> Result<Vec<ProviderId>, ConfigError> {
        ids.into_iter()
            .map(|text| {
                let text = text.trim().to_owned();
                ProviderId::parse(&text).map_err(|error| ConfigError::BadSendHookProvider {
                    session: session.to_owned(),
                    provider: text,
                    detail: error.to_string(),
                })
            })
            .collect()
    };
    Ok(OutboundPolicy {
        before: parse_hooks(raw.before)?,
        after: parse_hooks(raw.after)?,
    })
}

fn build_turn_error(session: &str, raw: RawTurnError) -> Result<TurnErrorPolicy, ConfigError> {
    let provider = match raw
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        Some(text) => {
            ProviderId::parse(text).map_err(|error| ConfigError::BadTurnErrorProvider {
                session: session.to_owned(),
                provider: text.to_owned(),
                detail: error.to_string(),
            })?
        }
        None => ProviderId::builtin_turn_error(),
    };
    Ok(TurnErrorPolicy {
        provider,
        options: policy_options(
            raw.options,
            &acb_i18n::t!("kernel.config.label.turn_error_policy"),
        )?,
    })
}

/// 会话级上下文注入 provider 列表。空串/非法 id 在启动时拒绝；重复 id 去重。
fn build_prompts(session: &str, raw: Vec<String>) -> Result<Vec<ProviderId>, ConfigError> {
    let mut prompts: Vec<ProviderId> = Vec::new();
    for item in raw {
        let text = item.trim();
        if text.is_empty() {
            return Err(ConfigError::BadPromptProvider {
                session: session.to_owned(),
                provider: item.clone(),
                detail: "empty provider id".to_owned(),
            });
        }
        let id = ProviderId::parse(text).map_err(|error| ConfigError::BadPromptProvider {
            session: session.to_owned(),
            provider: text.to_owned(),
            detail: error.to_string(),
        })?;
        if !prompts.contains(&id) {
            prompts.push(id);
        }
    }
    Ok(prompts)
}

fn build_wake(session: &str, room: &RoomId, raw: RawWake) -> Result<WakePolicy, ConfigError> {
    let default = WakePolicy::default();

    if let Some(accumulate) = raw.accumulate {
        if accumulate == 0 {
            return Err(ConfigError::BadAccumulate {
                session: session.to_owned(),
                room: room.to_string(),
            });
        }
    }

    let provider = match raw
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        Some(text) => ProviderId::parse(text).map_err(|error| ConfigError::BadWakeProvider {
            session: session.to_owned(),
            room: room.to_string(),
            provider: text.to_owned(),
            detail: error.to_string(),
        })?,
        None => ProviderId::builtin_wake(),
    };

    Ok(WakePolicy {
        provider,
        at_mention: raw.at_mention.unwrap_or(default.at_mention),
        reply: raw.reply.unwrap_or(default.reply),
        name_mention: raw.name_mention.unwrap_or(default.name_mention),
        names: raw.names.unwrap_or(default.names),
        accumulate: raw.accumulate.map(Some).unwrap_or(default.accumulate),
        event_tags: normalize_list(raw.event_tags.unwrap_or(default.event_tags)),
        ignore_event_tags: {
            let mut tags =
                normalize_list(raw.ignore_event_tags.unwrap_or(default.ignore_event_tags));
            // 旧配置 `poke = false` 的语法糖：不再因为 `poke` 标签/拍我而醒。
            // `poke = true` 不必再做事——at_mention 默认就覆盖「冲我来的事件」。
            if raw.poke == Some(false) && !tags.iter().any(|tag| tag == "poke") {
                tags.push("poke".to_owned());
            }
            tags
        },
        events: normalize_list(raw.events.unwrap_or(default.events)),
        keywords: raw.keywords.unwrap_or(default.keywords),
        ignore_speakers: raw.ignore_speakers.unwrap_or(default.ignore_speakers),
        options: policy_options(
            raw.options,
            &acb_i18n::t!("kernel.config.label.wake_policy"),
        )?,
    })
}

/// 唤醒事件名带平台前缀时（`qq.poke` / `qq.*`），前缀必须等于该房间连接的 platform。
///
/// 唤醒配置是按房间存的，所以 `events = ["qq.poke"]` 配在 discord 房间会永远不生效；
/// 这种配置在启动时就拒掉，而不是等用户发现「怎么叫都不醒」。
fn validate_wake_events(
    session: &str,
    room: &RoomId,
    wake: &WakePolicy,
    platform: &str,
) -> Result<(), ConfigError> {
    for event in &wake.events {
        if let Some((prefix, _)) = event.split_once('.') {
            if prefix != platform {
                return Err(ConfigError::BadWakeEvent {
                    session: session.to_owned(),
                    room: room.to_string(),
                    event: event.clone(),
                    platform: platform.to_owned(),
                });
            }
        }
    }
    Ok(())
}

fn check_not_blank(value: &str, field: &'static str) -> Result<(), &'static str> {
    if value.trim().is_empty() {
        return Err(field);
    }
    Ok(())
}

/// 依赖表阈值：省略用默认值，0 拒绝。
fn deps_limit(field: &str, value: Option<u64>, default: u64) -> Result<u64, ConfigError> {
    let Some(value) = value else {
        return Ok(default);
    };
    if value == 0 {
        return Err(ConfigError::BadDepsConfig {
            field: field.to_owned(),
            value,
        });
    }
    Ok(value)
}

/// LKG 阈值：省略用默认值，0 拒绝。
fn lkg_limit(field: &str, value: Option<u64>, default: u64) -> Result<u64, ConfigError> {
    let Some(value) = value else {
        return Ok(default);
    };
    if value == 0 {
        return Err(ConfigError::BadLkgConfig {
            field: field.to_owned(),
            value,
        });
    }
    Ok(value)
}

/// 通知/去重参数：省略用默认值，0 拒绝。
fn notice_limit(field: &str, value: Option<u64>, default: u64) -> Result<u64, ConfigError> {
    let Some(value) = value else {
        return Ok(default);
    };
    if value == 0 {
        return Err(ConfigError::BadNotificationsConfig {
            field: field.to_owned(),
            value,
        });
    }
    Ok(value)
}

fn queue_limit(field: &str, value: Option<u64>, default: usize) -> Result<usize, ConfigError> {
    let Some(value) = value else {
        return Ok(default);
    };
    if value == 0 {
        return Err(ConfigError::BadQueueLimit {
            field: field.to_owned(),
            value,
        });
    }
    usize::try_from(value).map_err(|_| ConfigError::BadQueueLimit {
        field: field.to_owned(),
        value,
    })
}

/// 把 TOML 里的失败通知动作字符串列表解析成 [`TurnErrorAction`]。
///
/// 每条是 `log` 或 `send:<连接>:<段/段>`（房间写法同订阅，如 `send:qq:group/123`）。
/// 解析不了就报错——这是配置层面的错，不该带病启动。
fn parse_turn_error_actions(raw: &[String]) -> Result<Vec<TurnErrorAction>, ConfigError> {
    let mut actions = Vec::with_capacity(raw.len());
    for entry in raw {
        let action = entry.trim();
        if action == "log" {
            actions.push(TurnErrorAction::Log);
            continue;
        }
        if let Some(room_text) = action.strip_prefix("send:") {
            // 房间写法的第一段是连接名，后面是段/段；这里拆出连接名和路径。
            let room_text = room_text.trim();
            let (connection, path) = match room_text.split_once(':') {
                Some((connection, path)) => (connection, path),
                None => {
                    return Err(ConfigError::BadTurnErrorAction {
                        action: entry.clone(),
                    });
                }
            };
            let room = RoomId::new(connection.trim(), path.trim()).map_err(|_| {
                ConfigError::BadTurnErrorAction {
                    action: entry.clone(),
                }
            })?;
            actions.push(TurnErrorAction::Send { room });
            continue;
        }
        return Err(ConfigError::BadTurnErrorAction {
            action: entry.clone(),
        });
    }
    Ok(actions)
}

// ---------------------------------------------------------------------------
// 原始 TOML 形态
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    personas: Vec<RawPersona>,
    #[serde(default)]
    memories: Vec<RawMemory>,
    #[serde(default)]
    harnesses: Vec<RawHarness>,
    #[serde(default)]
    sessions: Vec<RawSession>,
    endpoint: Option<RawEndpoint>,
    console: Option<RawConsole>,
    file_relay: Option<RawFileRelay>,
    #[serde(default)]
    connections: Vec<RawConnection>,
    /// 动态库插件声明。
    #[serde(default)]
    plugins: Vec<RawPlugin>,
    /// 外置进程工具插件声明。
    #[serde(default)]
    tool_plugins: Vec<RawToolPlugin>,
    storage: Option<RawStorage>,
    retention: Option<RawRetention>,
    queues: Option<RawQueue>,
    /// 风险策略是连接级配置；顶层出现 `[policy]` 时启动失败，避免被静默忽略。
    #[serde(default)]
    policy: Option<toml::Value>,
    #[serde(default)]
    grants: Option<crate::grants::GrantsConfig>,
    #[serde(default)]
    tickets: Option<crate::tickets::TicketsPolicy>,
    tasks: Option<RawTasks>,
    deps: Option<RawDeps>,
    lkg: Option<RawLkg>,
    provider_calls: Option<RawProviderCalls>,
    notifications: Option<RawNotifications>,
    /// 密钥来源声明；密钥字段通过 `{ secret = "<name>" }` 引用。
    #[serde(default)]
    secrets: Option<RawSecrets>,
    worktrees_root: Option<String>,
    /// 会话没配 `on_turn_error` 时的默认动作列表（默认空 = 只记日志）。
    #[serde(default)]
    default_on_turn_error: Vec<String>,
    /// 一轮投递最多塞多少条。0 = 不截断。
    #[serde(default)]
    delivery_limit: Option<usize>,
    /// 一轮最多跑多久（秒）。0 = 关闭超时。不配 = 默认关闭。
    #[serde(default)]
    turn_timeout_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSecrets {
    #[serde(default)]
    default_kind: Option<String>,
    #[serde(default)]
    sources: Vec<RawSecretSource>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSecretSource {
    name: String,
    /// 缺省用 `[secrets].default_kind`，再缺省是 `env`。
    #[serde(default)]
    kind: Option<String>,
    /// `kind = "env"`：进程环境变量名。
    #[serde(default)]
    variable: Option<String>,
    /// `kind = "file"`：绝对路径。
    #[serde(default)]
    path: Option<String>,
    /// `kind = "file"`：`raw`（默认）或 `dotenv`。
    #[serde(default)]
    format: Option<String>,
    /// `format = "dotenv"`：要取的键。
    #[serde(default)]
    key: Option<String>,
}

/// 密钥字段的值：只允许 `{ secret = "<name>" }`；字面量一律拒绝。
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawSecretValue {
    #[allow(dead_code)]
    Literal(String),
    Reference(RawSecretRef),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSecretRef {
    secret: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretMode {
    /// 生产：所有引用必须命中声明，所有声明必须能读到值。
    Strict,
    /// 测试 fixture：未声明的引用按名字自身解析。只在测试构建里存在。
    #[cfg(any(test, feature = "test-fixtures"))]
    Fixture,
}

/// 单个密钥来源的展示信息。只含引导参数，不含值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretEntry {
    pub name: String,
    /// `env` / `file`。
    pub kind: String,
    /// 环境变量名或文件路径（含 `dotenv` 的键）。
    pub origin: String,
}

#[derive(Clone)]
struct ResolvedSecret {
    kind: String,
    origin: String,
    value: String,
}

/// 加载期解析出来的密钥集合。值只留在内存里，不写回配置文本、不进数据库。
#[derive(Clone)]
pub struct SecretStore {
    /// 只有测试 fixture 需要记模式；生产构建里这个字段不存在。
    #[cfg(any(test, feature = "test-fixtures"))]
    mode: SecretMode,
    sources: BTreeMap<String, ResolvedSecret>,
}

impl std::fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let entries = self.entries();
        f.debug_struct("SecretStore")
            .field("sources", &entries)
            .finish()
    }
}

/// 库里已解密的密钥：来源名 → 明文。加载期用它顶替文件/环境来源。
pub type SecretPreset = std::collections::BTreeMap<String, String>;

impl SecretStore {
    fn from_raw(
        raw: Option<RawSecrets>,
        mode: SecretMode,
        preset: Option<&SecretPreset>,
    ) -> Result<Self, ConfigError> {
        let raw = raw.unwrap_or_default();
        let default_kind = raw
            .default_kind
            .as_deref()
            .map(str::trim)
            .filter(|kind| !kind.is_empty())
            .unwrap_or("env");
        if !matches!(default_kind, "env" | "file") {
            return Err(ConfigError::BadSecretSource {
                name: "(default_kind)".to_owned(),
                kind: default_kind.to_owned(),
                detail: acb_i18n::t!("kernel.config.secret_kind_unknown").to_owned(),
            });
        }

        // 先校验 schema 并查重，再读值：同名来源必须在任何量表之前就失败，
        // 不能因为第一个来源读不到环境变量而把重复声明错报成「缺值」。
        let mut plans = BTreeMap::new();
        for raw_source in raw.sources {
            let name = raw_source.name.trim().to_owned();
            let plan = plan_source(&raw_source, default_kind)?;
            if plans.insert(name.clone(), plan).is_some() {
                return Err(ConfigError::DuplicateSecret { name });
            }
        }

        let mut sources = BTreeMap::new();
        for (name, plan) in plans {
            // 库里有这个名字就以库为准：文件删了、env 没设都不影响启动。
            if let Some(value) = preset.and_then(|preset| preset.get(&name)) {
                sources.insert(
                    name,
                    ResolvedSecret {
                        kind: "db".to_owned(),
                        origin: "database".to_owned(),
                        value: value.clone(),
                    },
                );
                continue;
            }
            let value = match mode {
                SecretMode::Strict => load_plan(&name, &plan)?,
                // fixture 里的来源只校验 schema；引用按名字自身解析，不读真实文件/环境。
                #[cfg(any(test, feature = "test-fixtures"))]
                SecretMode::Fixture => String::new(),
            };
            sources.insert(
                name,
                ResolvedSecret {
                    kind: plan.kind,
                    origin: plan.origin,
                    value,
                },
            );
        }
        // 库里有的名字不要求在 `[secrets]` 里再声明一次：控制台新建会话
        // 直接把令牌写进 `secrets` 表，配置文件里不该出现第二份声明。
        if let Some(preset) = preset {
            for (name, value) in preset {
                if sources.contains_key(name) {
                    continue;
                }
                sources.insert(
                    name.clone(),
                    ResolvedSecret {
                        kind: "db".to_owned(),
                        origin: "database".to_owned(),
                        value: value.clone(),
                    },
                );
            }
        }
        Ok(Self {
            #[cfg(any(test, feature = "test-fixtures"))]
            mode,
            sources,
        })
    }

    /// 引用解析：返回密钥值。调用方负责按字段语义 trim。
    fn resolve(&self, value: &RawSecretValue, field: &str) -> Result<String, ConfigError> {
        let name = match value {
            RawSecretValue::Literal(_) => {
                return Err(ConfigError::SecretLiteral {
                    field: field.to_owned(),
                });
            }
            RawSecretValue::Reference(reference) => &reference.secret,
        };
        #[cfg(any(test, feature = "test-fixtures"))]
        if self.mode == SecretMode::Fixture {
            return Ok(name.clone());
        }
        if let Some(source) = self.sources.get(name) {
            return Ok(source.value.clone());
        }
        Err(ConfigError::UnknownSecret {
            field: field.to_owned(),
            name: name.clone(),
        })
    }

    fn resolve_opt(
        &self,
        value: Option<RawSecretValue>,
        field: &str,
    ) -> Result<Option<String>, ConfigError> {
        value.map(|value| self.resolve(&value, field)).transpose()
    }

    /// 所有已声明来源的引导信息（不含值）。`acbctl secrets check` 用。
    pub fn entries(&self) -> Vec<SecretEntry> {
        self.sources
            .iter()
            .map(|(name, source)| SecretEntry {
                name: name.clone(),
                kind: source.kind.clone(),
                origin: source.origin.clone(),
            })
            .collect()
    }

    /// 按来源名取解析后的值。不存在返回 `None`。
    pub fn value(&self, name: &str) -> Option<&str> {
        self.sources.get(name).map(|source| source.value.as_str())
    }

    /// 库内密钥覆盖/补充文件来源。只给启动期用：值已经从密文解出来，来源标记为 `db`。
    pub fn insert_runtime(&mut self, name: String, value: String) {
        self.sources.insert(
            name.clone(),
            ResolvedSecret {
                kind: "db".to_owned(),
                origin: "database".to_owned(),
                value,
            },
        );
    }
}

const MAX_SECRET_FILE_BYTES: u64 = 64 * 1024;

enum SecretLoad {
    Env {
        variable: String,
    },
    File {
        path: String,
        format: String,
        key: Option<String>,
    },
}

struct SecretPlan {
    kind: String,
    origin: String,
    load: SecretLoad,
}

/// 只校验 schema，不读值。Fixture 模式也会走这里，保证示例配置拼错字段能被发现。
fn plan_source(raw: &RawSecretSource, default_kind: &str) -> Result<SecretPlan, ConfigError> {
    let name = raw.name.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
    {
        return Err(ConfigError::BadSecretSource {
            name: raw.name.clone(),
            kind: default_kind.to_owned(),
            detail: acb_i18n::t!("kernel.config.secret_name_invalid").to_owned(),
        });
    }
    let kind = raw
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
        .unwrap_or(default_kind);
    match kind {
        "env" => {
            if raw.path.is_some() || raw.format.is_some() || raw.key.is_some() {
                return Err(ConfigError::BadSecretSource {
                    name: name.to_owned(),
                    kind: kind.to_owned(),
                    detail: acb_i18n::t!("kernel.config.secret_env_fields").to_owned(),
                });
            }
            let variable = raw.variable.as_deref().map(str::trim).unwrap_or("");
            if variable.is_empty()
                || !variable
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            {
                return Err(ConfigError::BadSecretSource {
                    name: name.to_owned(),
                    kind: kind.to_owned(),
                    detail: acb_i18n::t!("kernel.config.secret_variable_invalid").to_owned(),
                });
            }
            Ok(SecretPlan {
                kind: "env".to_owned(),
                origin: variable.to_owned(),
                load: SecretLoad::Env {
                    variable: variable.to_owned(),
                },
            })
        }
        "file" => {
            if raw.variable.is_some() {
                return Err(ConfigError::BadSecretSource {
                    name: name.to_owned(),
                    kind: kind.to_owned(),
                    detail: acb_i18n::t!("kernel.config.secret_file_fields").to_owned(),
                });
            }
            let path = raw.path.as_deref().map(str::trim).unwrap_or("");
            if path.is_empty() {
                return Err(ConfigError::BadSecretSource {
                    name: name.to_owned(),
                    kind: kind.to_owned(),
                    detail: acb_i18n::t!("kernel.config.secret_path_required").to_owned(),
                });
            }
            if !Path::new(path).is_absolute() {
                return Err(ConfigError::BadSecretSource {
                    name: name.to_owned(),
                    kind: kind.to_owned(),
                    detail: acb_i18n::t!("kernel.config.secret_path_relative").to_owned(),
                });
            }
            let format = raw
                .format
                .as_deref()
                .map(str::trim)
                .filter(|format| !format.is_empty())
                .unwrap_or("raw");
            let key = match format {
                "raw" => {
                    if raw.key.is_some() {
                        return Err(ConfigError::BadSecretSource {
                            name: name.to_owned(),
                            kind: kind.to_owned(),
                            detail: acb_i18n::t!("kernel.config.secret_key_with_raw").to_owned(),
                        });
                    }
                    None
                }
                "dotenv" => {
                    let key = raw.key.as_deref().map(str::trim).unwrap_or("");
                    if key.is_empty()
                        || !key
                            .chars()
                            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                    {
                        return Err(ConfigError::BadSecretSource {
                            name: name.to_owned(),
                            kind: kind.to_owned(),
                            detail: acb_i18n::t!("kernel.config.secret_key_required").to_owned(),
                        });
                    }
                    Some(key.to_owned())
                }
                other => {
                    return Err(ConfigError::BadSecretSource {
                        name: name.to_owned(),
                        kind: kind.to_owned(),
                        detail: acb_i18n::t!("kernel.config.secret_format_unknown", format = other),
                    });
                }
            };
            let origin = match key.as_deref() {
                Some(key) => format!("{path}#{key}"),
                None => path.to_owned(),
            };
            Ok(SecretPlan {
                kind: "file".to_owned(),
                origin,
                load: SecretLoad::File {
                    path: path.to_owned(),
                    format: format.to_owned(),
                    key,
                },
            })
        }
        other => Err(ConfigError::BadSecretSource {
            name: name.to_owned(),
            kind: other.to_owned(),
            detail: acb_i18n::t!("kernel.config.secret_kind_unknown").to_owned(),
        }),
    }
}

fn load_plan(name: &str, plan: &SecretPlan) -> Result<String, ConfigError> {
    match &plan.load {
        SecretLoad::Env { variable } => {
            let value = std::env::var(variable).ok();
            value
                .filter(|value| !value.is_empty())
                .ok_or_else(|| ConfigError::MissingSecretEnv {
                    name: name.to_owned(),
                    variable: variable.clone(),
                })
        }
        SecretLoad::File { path, format, key } => {
            read_secret_file(name, path, format, key.as_deref())
        }
    }
}

fn read_secret_file(
    name: &str,
    path: &str,
    format: &str,
    key: Option<&str>,
) -> Result<String, ConfigError> {
    let path_ref = Path::new(path);
    let mut file = std::fs::File::open(path_ref).map_err(|error| ConfigError::BadSecretFile {
        name: name.to_owned(),
        path: path.to_owned(),
        detail: error.to_string(),
    })?;
    let metadata = file
        .metadata()
        .map_err(|error| ConfigError::BadSecretFile {
            name: name.to_owned(),
            path: path.to_owned(),
            detail: error.to_string(),
        })?;
    if !metadata.is_file() {
        return Err(ConfigError::BadSecretFile {
            name: name.to_owned(),
            path: path.to_owned(),
            detail: acb_i18n::t!("kernel.config.secret_not_regular").to_owned(),
        });
    }
    if metadata.len() > MAX_SECRET_FILE_BYTES {
        return Err(ConfigError::BadSecretFile {
            name: name.to_owned(),
            path: path.to_owned(),
            detail: acb_i18n::t!(
                "kernel.config.secret_file_too_large",
                limit = MAX_SECRET_FILE_BYTES
            ),
        });
    }
    check_secret_file_security(name, path, &metadata)?;

    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|error| ConfigError::BadSecretFile {
            name: name.to_owned(),
            path: path.to_owned(),
            detail: error.to_string(),
        })?;

    if text.contains('\0') {
        return Err(ConfigError::EmptySecret {
            name: name.to_owned(),
            origin: path.to_owned(),
        });
    }
    match format {
        "dotenv" => parse_dotenv(name, path, &text, key.unwrap_or_default()),
        _ => {
            let value = text.strip_suffix('\n').unwrap_or(&text);
            let value = value.strip_suffix('\r').unwrap_or(value);
            if value.is_empty() || value.chars().all(char::is_whitespace) {
                return Err(ConfigError::EmptySecret {
                    name: name.to_owned(),
                    origin: path.to_owned(),
                });
            }
            Ok(value.to_owned())
        }
    }
}

fn check_secret_file_security(
    name: &str,
    path: &str,
    metadata: &std::fs::Metadata,
) -> Result<(), ConfigError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let mode = metadata.mode() & 0o777;
        if mode & 0o077 != 0 || mode & 0o400 == 0 {
            return Err(ConfigError::InsecureSecretFile {
                name: name.to_owned(),
                path: path.to_owned(),
                detail: acb_i18n::t!("kernel.config.secret_file_mode", mode = format!("{mode:o}"))
                    .to_owned(),
            });
        }
        let owner = metadata.uid();
        let euid = unsafe { libc::geteuid() };
        if owner != euid && owner != 0 {
            return Err(ConfigError::InsecureSecretFile {
                name: name.to_owned(),
                path: path.to_owned(),
                detail: acb_i18n::t!(
                    "kernel.config.secret_file_owner",
                    owner = owner,
                    euid = euid
                )
                .to_owned(),
            });
        }
    }
    #[cfg(not(unix))]
    let _ = (name, path, metadata);
    Ok(())
}

fn parse_dotenv(name: &str, path: &str, text: &str, wanted: &str) -> Result<String, ConfigError> {
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    for (index, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(ConfigError::BadSecretDotenv {
                name: name.to_owned(),
                path: path.to_owned(),
                line: index + 1,
                detail: acb_i18n::t!("kernel.config.secret_dotenv_no_equal").to_owned(),
            });
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            return Err(ConfigError::BadSecretDotenv {
                name: name.to_owned(),
                path: path.to_owned(),
                line: index + 1,
                detail: acb_i18n::t!("kernel.config.secret_dotenv_empty_key").to_owned(),
            });
        }
        if value.is_empty() {
            return Err(ConfigError::EmptySecret {
                name: name.to_owned(),
                origin: format!("{path}#{key}"),
            });
        }
        if values.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(ConfigError::BadSecretDotenv {
                name: name.to_owned(),
                path: path.to_owned(),
                line: index + 1,
                detail: acb_i18n::t!("kernel.config.secret_dotenv_duplicate", key = key),
            });
        }
    }
    values
        .remove(wanted)
        .ok_or_else(|| ConfigError::BadSecretDotenv {
            name: name.to_owned(),
            path: path.to_owned(),
            line: 0,
            detail: acb_i18n::t!("kernel.config.secret_dotenv_missing", key = wanted),
        })
}

#[derive(Debug, Deserialize)]
struct RawPersona {
    id: String,
    name: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct RawMemory {
    id: String,
    name: String,
    kind: String,
    location: String,
}

#[derive(Debug, Deserialize)]
struct RawHarness {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct RawSession {
    id: String,
    harness: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    model: Option<String>,
    /// dsh 驱动的 MCP 端口；缺省时 acbctl 仍按历史下标推导，只用于旧配置迁移。
    #[serde(default)]
    driver_port: Option<u16>,
    persona: String,
    #[serde(default)]
    memories: Vec<String>,
    /// 接入令牌引用。必填——没有令牌就没有身份，桥不知道该把这条连接绑到哪些会话。
    token: RawSecretValue,
    #[serde(default)]
    subscriptions: Vec<RawSubscription>,
    /// 访问策略（默认按订阅）。
    #[serde(default)]
    access: RawAccess,
    /// 投递批次选择（默认内置 composer）。
    #[serde(default)]
    delivery: RawDelivery,
    /// 发送前/后钩子（默认空）。
    #[serde(default)]
    outbound: RawOutbound,
    /// 失败通知策略（默认内置）。
    #[serde(default)]
    turn_error: RawTurnError,
    /// 一轮失败时的通知动作列表（默认空 = 只记日志）。
    #[serde(default)]
    on_turn_error: Vec<String>,
    /// 上下文注入 provider 列表（默认空 = 不注入）。
    #[serde(default)]
    prompts: Vec<String>,
    /// 投递窗口的会话级默认值；订阅可以逐项覆盖。不写 = 3/3/3。
    #[serde(default)]
    context_before: Option<usize>,
    #[serde(default)]
    context_after: Option<usize>,
    #[serde(default)]
    batch_head: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDelivery {
    provider: Option<String>,
    /// 插件专属配置；core 原样交给 provider。
    #[serde(default)]
    options: toml::value::Table,
}

#[derive(Debug, Default, Deserialize)]
struct RawOutbound {
    #[serde(default)]
    before: Vec<String>,
    #[serde(default)]
    after: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawTurnError {
    provider: Option<String>,
    /// 插件专属配置；core 原样交给 provider。
    #[serde(default)]
    options: toml::value::Table,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAccess {
    /// 访问策略 provider id。不写就用内置 `[grants]` 评估器。
    provider: Option<String>,
    #[serde(default)]
    options: toml::value::Table,
}

#[derive(Debug, Deserialize)]
struct RawStorage {
    path: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawRetention {
    max_records_per_room: Option<u64>,
    max_age_days: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawPlugin {
    path: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawProviderCalls {
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawNotifications {
    #[serde(default = "default_enabled")]
    enabled: bool,
    url: Option<String>,
    topic: Option<RawSecretValue>,
    token: Option<RawSecretValue>,
    min_interval_secs: Option<u64>,
    #[serde(default)]
    in_band: RawInBand,
}

#[derive(Debug, Default, Deserialize)]
struct RawInBand {
    window_secs: Option<u64>,
    room_limit: Option<u64>,
    max_entries: Option<u64>,
    remind_secs: Option<u64>,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct RawDeps {
    #[serde(default)]
    failure_threshold: Option<u64>,
    #[serde(default)]
    failure_window_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawLkg {
    #[serde(default)]
    max_failures: Option<u64>,
    #[serde(default)]
    window_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawQueue {
    #[serde(default)]
    inbound: Option<u64>,
    #[serde(default)]
    route: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawToolPlugin {
    id: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawConnection {
    id: String,
    platform: String,
    kind: String,
    /// 没写就是 `ws-client`（正向 WS），因为这是现在唯一实现的。
    #[serde(default = "default_transport")]
    transport: String,
    /// `ws-client` 必填；外置进程/自定义传输可以不带。
    url: Option<String>,
    /// 适配器专属配置；core 原样交给适配器。
    #[serde(default)]
    options: toml::value::Table,
    token: Option<RawSecretValue>,
    account: Option<String>,
    /// 系统管理员账号列表，可为空。
    #[serde(default)]
    admin_qqs: Vec<String>,
    /// 旧键：只在启动期给出迁移报错，不参与运行。
    #[serde(default)]
    allow_destructive: Option<toml::Value>,
    /// 旧 `[connections.policy.risk]`：只在启动期给出迁移报错。
    #[serde(default)]
    policy: Option<toml::Value>,
    /// 入站策略（默认全部接收）。
    #[serde(default)]
    receive: RawReceive,
    /// 消息路由链：`[[connections.routes]]`，按顺序执行。
    #[serde(default)]
    routes: Vec<RawRoute>,
}

#[derive(Debug, Default, Deserialize)]
struct RawRoute {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    options: toml::value::Table,
}

#[derive(Debug, Default, Deserialize)]
struct RawReceive {
    provider: Option<String>,
    #[serde(default)]
    options: toml::value::Table,
}

fn default_transport() -> String {
    "ws-client".to_owned()
}

#[derive(Debug, Deserialize)]
struct RawEndpoint {
    listen: String,
}

#[derive(Debug, Deserialize)]
struct RawConsole {
    listen: String,
    token: RawSecretValue,
}

#[derive(Debug, Deserialize)]
struct RawFileRelay {
    /// 中继方式，缺省 `file-server`。写别的值启动即报错。
    mode: Option<String>,
    listen: String,
    public_base: String,
    dir: Option<String>,
    /// 整数按字节；字符串支持 `"4 GiB"` / `"4096 MiB"` / `"1.5 GiB"`。
    max_publish_bytes: Option<toml::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct RawTasks {
    /// 任务完成时是否直接唤醒发起会话。缺省 false。
    #[serde(default)]
    wake_on_completion: bool,
    /// `tasks op=wait` 的服务端上限（毫秒）。缺省 45 秒。
    #[serde(default)]
    wait_max_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawSubscription {
    /// 走哪条连接收发。
    connection: String,
    /// 平台侧房间路径，如 `group/123`。
    room: String,
    wake: Option<RawWake>,
    clock: Option<RawClock>,
    /// 投递窗口覆盖项；不写就继承会话级默认值。
    context_before: Option<usize>,
    context_after: Option<usize>,
    batch_head: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct RawClock {
    provider: Option<String>,
    interval_secs: Option<u64>,
    idle_secs: Option<u64>,
    #[serde(default)]
    options: toml::value::Table,
}

#[derive(Debug, Default, Deserialize)]
struct RawWake {
    /// 唤醒策略 provider id（ADR-0026）。不写就用内置实现。
    provider: Option<String>,
    at_mention: Option<bool>,
    reply: Option<bool>,
    name_mention: Option<bool>,
    names: Option<Vec<String>>,
    accumulate: Option<u32>,
    /// 旧配置的“被拍醒”开关；`false` 会转成 `ignore_event_tags` 里的 `poke`。
    poke: Option<bool>,
    event_tags: Option<Vec<String>>,
    ignore_event_tags: Option<Vec<String>>,
    events: Option<Vec<String>>,
    keywords: Option<Vec<String>>,
    ignore_speakers: Option<Vec<String>>,
    #[serde(default)]
    options: toml::value::Table,
}

struct ParsedConfigFile {
    value: toml::Value,
    source: ConfigSource,
    rules: Vec<toml::Value>,
    has_grants: bool,
}

fn read_config_file(path: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(path).map_err(|error| ConfigError::FileReadFailed {
        path: path.display().to_string(),
        detail: error.to_string(),
    })
}

fn list_config_fragments(dir: &Path) -> Result<Vec<PathBuf>, ConfigError> {
    let entries = std::fs::read_dir(dir).map_err(|error| ConfigError::DirReadFailed {
        path: dir.display().to_string(),
        detail: error.to_string(),
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| ConfigError::DirReadFailed {
            path: dir.display().to_string(),
            detail: error.to_string(),
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
            continue;
        }
        paths.push(path);
    }
    paths.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    Ok(paths)
}

fn parse_config_file(path: &Path, text: &str) -> Result<ParsedConfigFile, ConfigError> {
    let label = path.display().to_string();
    let value: toml::Value =
        toml::from_str(text).map_err(|error| ConfigError::FileParseFailed {
            path: label.clone(),
            detail: error.to_string(),
        })?;
    // 逐文件先按 RawConfig 校验一次：fragment 里的未知字段/缺必填字段必须在
    // 这里带着文件名失败，不能等到合并后才报一个无来源的错误。
    let _raw: RawConfig = toml::from_str(text).map_err(|error| ConfigError::FileParseFailed {
        path: label.clone(),
        detail: error.to_string(),
    })?;
    let source = ConfigSource {
        path: label,
        connections: array_count(&value, "connections"),
        sessions: array_count(&value, "sessions"),
        rules: rules_count(&value),
    };
    let rules = value
        .get("grants")
        .and_then(|grants| grants.get("rules"))
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let has_grants = value.get("grants").is_some();
    Ok(ParsedConfigFile {
        value,
        source,
        rules,
        has_grants,
    })
}

/// 单文件加载 + 可选的库对象注入。
///
/// 没有对象时直接走 [`load_single`]（行为、错误信息一个字节都不变）；有对象时
/// 先把五类数组换成库里的那一份，再走同一条收尾路径。
fn load_single_with_objects(
    base_path: &Path,
    text: &str,
    mode: SecretMode,
    objects: Option<&crate::config_objects::ConfigObjects>,
    preset: Option<&SecretPreset>,
) -> Result<LoadedConfig, ConfigError> {
    let Some(objects) = objects else {
        return load_single(base_path, text, mode, preset);
    };
    let mut merged: toml::Value =
        toml::from_str(text).map_err(|error| ConfigError::ParseFailed {
            detail: error.to_string(),
        })?;
    objects.splice_into(&mut merged)?;
    let source = ConfigSource {
        path: base_path.display().to_string(),
        connections: array_count(&merged, "connections"),
        sessions: array_count(&merged, "sessions"),
        rules: rules_count(&merged),
    };
    finish_loaded(merged, vec![source], false, mode, preset)
}

fn load_single(
    base_path: &Path,
    text: &str,
    mode: SecretMode,
    preset: Option<&SecretPreset>,
) -> Result<LoadedConfig, ConfigError> {
    let raw: RawConfig = toml::from_str(text).map_err(|error| ConfigError::ParseFailed {
        detail: error.to_string(),
    })?;
    let (config, secrets) = Config::from_raw_with_secrets(raw, mode, preset)?;
    Ok(LoadedConfig {
        sources: vec![ConfigSource {
            path: base_path.display().to_string(),
            connections: config.connections.len(),
            sessions: config.sessions.len(),
            rules: config.grants.rules.len(),
        }],
        config,
        secrets,
        merged_toml: text.to_owned(),
        duplicate_rules: false,
    })
}

fn duplicate_rules_across_files(files: &[ParsedConfigFile]) -> bool {
    // 同 id 已在 GrantsConfig::validate 里硬失败；这里只提示“内容相同但 id 不同”
    // 的重复，避免把 warning 当成第二套身份对账。
    fn payload(rule: &toml::Value) -> toml::Value {
        let mut value = rule.clone();
        if let Some(table) = value.as_table_mut() {
            table.remove("id");
        }
        value
    }

    let mut seen: Vec<(usize, toml::Value)> = Vec::new();
    for (index, file) in files.iter().enumerate() {
        for rule in &file.rules {
            let payload = payload(rule);
            if seen
                .iter()
                .any(|(seen_index, seen_rule)| *seen_index != index && seen_rule == &payload)
            {
                return true;
            }
            seen.push((index, payload));
        }
    }
    false
}

fn finish_loaded(
    mut merged: toml::Value,
    sources: Vec<ConfigSource>,
    duplicate_rules: bool,
    mode: SecretMode,
    preset: Option<&SecretPreset>,
) -> Result<LoadedConfig, ConfigError> {
    dedupe_admin_lists(&mut merged);
    let merged_toml = toml::to_string(&merged).map_err(|error| ConfigError::ParseFailed {
        detail: error.to_string(),
    })?;
    let raw: RawConfig =
        toml::from_str(&merged_toml).map_err(|error| ConfigError::ParseFailed {
            detail: error.to_string(),
        })?;
    let (config, secrets) = Config::from_raw_with_secrets(raw, mode, preset)?;
    Ok(LoadedConfig {
        config,
        secrets,
        sources,
        merged_toml,
        duplicate_rules,
    })
}

fn array_count(value: &toml::Value, key: &str) -> usize {
    value
        .get(key)
        .and_then(toml::Value::as_array)
        .map_or(0, Vec::len)
}

fn rules_count(value: &toml::Value) -> usize {
    value
        .get("grants")
        .and_then(|grants| grants.get("rules"))
        .and_then(toml::Value::as_array)
        .map_or(0, Vec::len)
}

fn array_ids(value: &toml::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(toml::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("id").and_then(toml::Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn secret_source_names(value: &toml::Value) -> Vec<String> {
    value
        .get("secrets")
        .and_then(|secrets| secrets.get("sources"))
        .and_then(toml::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("name").and_then(toml::Value::as_str))
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn check_duplicate_ids(files: &[ParsedConfigFile]) -> Result<(), ConfigError> {
    let mut connections: BTreeMap<String, String> = BTreeMap::new();
    let mut sessions: BTreeMap<String, String> = BTreeMap::new();
    let mut secret_names: BTreeMap<String, String> = BTreeMap::new();
    for file in files {
        for id in array_ids(&file.value, "connections") {
            if let Some(first) = connections.insert(id.clone(), file.source.path.clone()) {
                return Err(ConfigError::DuplicateAcrossFiles {
                    kind: "connections".to_owned(),
                    id,
                    first,
                    second: file.source.path.clone(),
                });
            }
        }
        for id in array_ids(&file.value, "sessions") {
            if let Some(first) = sessions.insert(id.clone(), file.source.path.clone()) {
                return Err(ConfigError::DuplicateAcrossFiles {
                    kind: "sessions".to_owned(),
                    id,
                    first,
                    second: file.source.path.clone(),
                });
            }
        }
        for name in secret_source_names(&file.value) {
            if let Some(first) = secret_names.insert(name.clone(), file.source.path.clone()) {
                return Err(ConfigError::DuplicateAcrossFiles {
                    kind: "secrets.sources".to_owned(),
                    id: name,
                    first,
                    second: file.source.path.clone(),
                });
            }
        }
    }
    Ok(())
}

fn merge_document(acc: &mut toml::Value, next: toml::Value) {
    match next {
        toml::Value::Table(next_table) => {
            let Some(acc_table) = acc.as_table_mut() else {
                *acc = toml::Value::Table(next_table);
                return;
            };
            for (key, value) in next_table {
                if key == "grants" {
                    merge_grants(acc_table, value);
                } else if key == "secrets" {
                    merge_secrets(acc_table, value);
                } else {
                    merge_entry(acc_table, key, value);
                }
            }
        }
        other => *acc = other,
    }
}

fn merge_entry(acc: &mut toml::Table, key: String, value: toml::Value) {
    if let Some(existing) = acc.get_mut(&key) {
        match (existing, value) {
            (toml::Value::Array(current), toml::Value::Array(next)) => current.extend(next),
            // 普通表按「后出现者覆盖」处理：分片要改表内字段，就在分片里写完整那张表。
            // grants 的 rooms/accounts/rules 在 merge_grants 里另有按键/追加规则。
            (existing, value) => *existing = value,
        }
        return;
    }
    acc.insert(key, value);
}

fn merge_grants(acc: &mut toml::Table, value: toml::Value) {
    let next = match value {
        toml::Value::Table(next) => next,
        other => {
            acc.insert("grants".to_owned(), other);
            return;
        }
    };
    if acc.get("grants").and_then(toml::Value::as_table).is_none() {
        acc.insert("grants".to_owned(), toml::Value::Table(next));
        return;
    }
    let current = acc
        .get_mut("grants")
        .and_then(toml::Value::as_table_mut)
        .expect("checked above");
    for (key, value) in next {
        match key.as_str() {
            "rules" | "deny" => merge_entry(current, key, value),
            "rooms" | "accounts" => merge_keyed_table(current, key, value),
            _ => {
                current.insert(key, value);
            }
        }
    }
}

/// `[secrets]` 跨分片合并：`sources` 数组追加，`default_kind` 后者覆盖。
fn merge_secrets(acc: &mut toml::Table, value: toml::Value) {
    let next = match value {
        toml::Value::Table(next) => next,
        other => {
            acc.insert("secrets".to_owned(), other);
            return;
        }
    };
    if acc.get("secrets").and_then(toml::Value::as_table).is_none() {
        acc.insert("secrets".to_owned(), toml::Value::Table(next));
        return;
    }
    let current = acc
        .get_mut("secrets")
        .and_then(toml::Value::as_table_mut)
        .expect("checked above");
    for (key, value) in next {
        match key.as_str() {
            "sources" => merge_entry(current, key, value),
            _ => {
                current.insert(key, value);
            }
        }
    }
}

fn merge_keyed_table(acc: &mut toml::Table, key: String, value: toml::Value) {
    let next = match value {
        toml::Value::Table(next) => next,
        other => {
            acc.insert(key, other);
            return;
        }
    };
    if let Some(current) = acc.get_mut(&key).and_then(toml::Value::as_table_mut) {
        for (entry_key, entry_value) in next {
            current.insert(entry_key, entry_value);
        }
        return;
    }
    acc.insert(key, toml::Value::Table(next));
}

fn apply_console_overlay(merged: &mut toml::Value, overlay: toml::Value) {
    match overlay {
        toml::Value::Table(next) => {
            let Some(table) = merged.as_table_mut() else {
                *merged = toml::Value::Table(next);
                return;
            };
            for (key, value) in next {
                table.insert(key, value);
            }
        }
        other => *merged = other,
    }
}

fn dedupe_admin_lists(value: &mut toml::Value) {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table.iter_mut() {
                if key == "admin_qqs" {
                    if let toml::Value::Array(items) = child {
                        let mut seen = Vec::new();
                        items.retain(|item| {
                            if seen.contains(item) {
                                false
                            } else {
                                seen.push(item.clone());
                                true
                            }
                        });
                    }
                }
                dedupe_admin_lists(child);
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                dedupe_admin_lists(item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(connections: &str, sessions: &str) -> String {
        format!("{connections}\n{sessions}")
    }

    #[test]
    fn onebot_snowluma_kind_is_a_canonical_alias() {
        let alias = ONE_CONNECTION.replace("kind = \"onebot11\"", "kind = \"onebot-snowluma\"");
        let config = Config::parse_fixture(&config_with(&alias, ONE_SESSION)).expect("别名能解析");
        assert_eq!(config.connections[0].kind, "onebot11");

        let canonical =
            Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("旧写法");
        assert_eq!(canonical.connections[0].kind, "onebot11");
    }

    #[test]
    fn tasks_policy_defaults_and_overrides() {
        let config =
            Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        assert!(!config.tasks.wake_on_completion, "默认关闭，保持旧行为");
        assert_eq!(config.tasks.wait_max_ms, DEFAULT_TASK_WAIT_MAX_MS);

        let text = format!(
            "{}\n[tasks]\nwake_on_completion = true\nwait_max_ms = 20000",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&text).expect("覆盖值能解析");
        assert!(config.tasks.wake_on_completion);
        assert_eq!(config.tasks.wait_max_ms, 20_000);

        let bad = format!(
            "{}\n[tasks]\nwait_max_ms = 0",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        assert!(
            Config::parse_fixture(&bad).is_err(),
            "0 会让 op=wait 立刻返回，应当直接报错"
        );
    }

    #[test]
    fn tickets_policy_defaults_and_overrides() {
        let config =
            Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        assert_eq!(config.tickets.request_cooldown_secs, 60);
        assert_eq!(config.tickets.admin_cooldown_secs, 30);

        let text = format!(
            "{}\n[tickets]\nrequest_cooldown_secs = 5\nadmin_cooldown_secs = 2",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&text).expect("覆盖值能解析");
        assert_eq!(config.tickets.request_cooldown_secs, 5);
        assert_eq!(config.tickets.admin_cooldown_secs, 2);
    }

    #[test]
    fn delivery_window_defaults_to_three_and_three() {
        let config =
            Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        let window = config.sessions[0].subscriptions[0].window;
        assert_eq!(
            window,
            DeliveryWindow {
                context_before: 3,
                context_after: 3,
                batch_head: 3
            }
        );
    }

    #[test]
    fn subscription_overrides_session_window_per_field() {
        let text = config_with(ONE_CONNECTION, ONE_SESSION).replace(
            "token = { secret = \"t-main\" }",
            "token = { secret = \"t-main\" }\n        context_before = 5",
        );
        let text = text.replace(
            "room = \"group/123\"",
            "room = \"group/123\"\n        context_after = 1",
        );
        let config = Config::parse_fixture(&text).expect("能解析");
        let window = config.sessions[0].subscriptions[0].window;
        assert_eq!(window.context_before, 5, "会话级默认值");
        assert_eq!(window.context_after, 1, "订阅逐项覆盖");
        assert_eq!(window.batch_head, 3, "没写的还是默认值");
    }

    #[test]
    fn delivery_window_rejects_zero_batch_head_and_oversized_values() {
        let zero = config_with(ONE_CONNECTION, ONE_SESSION).replace(
            "room = \"group/123\"",
            "room = \"group/123\"\n        batch_head = 0",
        );
        assert!(
            matches!(
                Config::parse_fixture(&zero),
                Err(ConfigError::BadDeliveryWindow { .. })
            ),
            "batch_head = 0 会让时钟轮没有正文"
        );

        let huge = config_with(ONE_CONNECTION, ONE_SESSION).replace(
            "room = \"group/123\"",
            "room = \"group/123\"\n        context_before = 51",
        );
        assert!(
            matches!(
                Config::parse_fixture(&huge),
                Err(ConfigError::BadDeliveryWindow { .. })
            ),
            "超过上限应当直接报错，而不是悄悄截断"
        );
    }

    const ONE_CONNECTION: &str = r#"
        [[connections]]
        id = "qq"
        platform = "qq"
        kind = "onebot11"
        url = "ws://127.0.0.1:3001"
        account = "10001"
    "#;

    const ONE_SESSION: &str = r#"
        [[personas]]
        id = "default"
        name = "默认人格"
        text = "你是桥。"

        [[harnesses]]
        id = "dsh"
        name = "DeepSeek"

        [[sessions]]
        id = "main"
        harness = "dsh"
        persona = "default"
        token = { secret = "t-main" }

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"
    "#;

    #[test]
    fn provider_references_cover_all_current_fields() {
        let text = r#"
            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3001"
            account = "10001"

            [connections.receive]
            provider = "test.provider"

            [[connections.routes]]
            provider = "test.provider"

            [[personas]]
            id = "p"
            name = "P"
            text = "你是桥。"

            [[harnesses]]
            id = "dsh"
            name = "DSH"

            [[sessions]]
            id = "main"
            harness = "dsh"
            persona = "p"
            token = { secret = "t-main" }
            prompts = ["test.provider"]

            [sessions.access]
            provider = "test.provider"

            [sessions.delivery]
            provider = "test.provider"

            [sessions.outbound]
            before = ["test.provider"]

            [sessions.turn_error]
            provider = "test.provider"

            [[sessions.subscriptions]]
            connection = "qq"
            room = "group/123"

            [sessions.subscriptions.wake]
            provider = "test.provider"

            [sessions.subscriptions.clock]
            provider = "test.provider"
        "#;
        let config = Config::parse_fixture(text).expect("配置合法");
        let refs = config.provider_references();
        let locations: Vec<&str> = refs
            .iter()
            .filter(|reference| reference.id == ProviderId::parse("test.provider").expect("合法"))
            .map(|reference| reference.location.as_str())
            .collect();
        assert_eq!(
            locations,
            vec![
                "connection:qq receive",
                "connection:qq route",
                "session:main access",
                "session:main clock qq:group/123",
                "session:main delivery",
                "session:main outbound",
                "session:main prompts",
                "session:main turn_error",
                "session:main wake qq:group/123",
            ]
        );
        assert!(
            config
                .provider_references()
                .iter()
                .all(|reference| reference.id == ProviderId::parse("test.provider").expect("合法")),
            "没有其它 provider 被误算进引用"
        );
    }

    #[test]
    fn minimal_config_uses_documented_defaults() {
        let config =
            Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("应当能解析");
        let subscription = &config.sessions[0].subscriptions[0];

        assert!(config.sessions[0].prompts.is_empty(), "默认不注入上下文");
        assert_eq!(subscription.room.to_string(), "qq:group/123");
        assert!(subscription.wake.at_mention);
        assert!(subscription.wake.reply);
        assert!(subscription.wake.name_mention);
        assert!(subscription.wake.event_tags.is_empty());
        assert_eq!(subscription.wake.accumulate, Some(DEFAULT_ACCUMULATE));
        assert!(subscription.wake.keywords.is_empty());
        assert_eq!(config.connections[0].transport, Transport::WsClient);
        // 超时默认关闭，协议帧里带 0 表示 harness 不许自己掐。
        assert_eq!(config.turn_timeout_secs, DEFAULT_TURN_TIMEOUT_SECS);
        assert_eq!(config.turn_timeout_ms(), 0);
    }

    #[test]
    fn driver_port_is_optional_and_parsed() {
        let without = Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION))
            .expect("旧配置没有 driver_port");
        assert_eq!(without.sessions[0].driver_port, None);

        let with = ONE_SESSION.replace(
            "harness = \"dsh\"",
            "harness = \"dsh\"\n        driver_port = 3101",
        );
        let config =
            Config::parse_fixture(&config_with(ONE_CONNECTION, &with)).expect("显式端口能解析");
        assert_eq!(config.sessions[0].driver_port, Some(3101));
    }

    #[test]
    fn driver_port_rejects_privileged_ports() {
        let text = ONE_SESSION.replace(
            "harness = \"dsh\"",
            "harness = \"dsh\"\n        driver_port = 80",
        );
        let error = Config::parse_fixture(&config_with(ONE_CONNECTION, &text))
            .expect_err("80 是特权端口，应当拒绝");
        assert!(
            error.to_string().contains("below 1024"),
            "错误要指明端口范围：{error}"
        );
    }

    #[test]
    fn runtime_settings_override_code_defaults() {
        let mut config =
            Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        config.reset_runtime_defaults();
        assert_eq!(config.delivery_limit, DEFAULT_DELIVERY_LIMIT);
        assert_eq!(config.turn_timeout_secs, DEFAULT_TURN_TIMEOUT_SECS);

        let settings = BTreeMap::from([
            ("delivery_limit".to_owned(), serde_json::json!(20)),
            ("turn_timeout_secs".to_owned(), serde_json::json!(45)),
            (
                "tickets.pending_ttl_secs".to_owned(),
                serde_json::json!(1800),
            ),
            (
                "default_on_turn_error".to_owned(),
                serde_json::json!(["send:qq:group/123"]),
            ),
        ]);
        config.apply_settings(&settings).expect("设置能应用");
        assert_eq!(config.delivery_limit, 20);
        assert_eq!(config.turn_timeout_secs, 45);
        assert_eq!(config.tickets.pending_ttl_secs, 1800);
        assert_eq!(config.default_on_turn_error.len(), 1);
    }

    #[test]
    fn runtime_settings_reject_bad_values() {
        let mut config =
            Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        let settings = BTreeMap::from([("delivery_limit".to_owned(), serde_json::json!(0))]);
        let error = config.apply_settings(&settings).expect_err("0 必须拒绝");
        assert!(error.contains("greater than zero"), "{error}");
    }

    #[test]
    fn access_defaults_to_the_builtin_grants_evaluator() {
        let config =
            Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        let access = &config.sessions[0].access;
        assert_eq!(access.provider, ProviderId::builtin_grants_access());
    }

    /// `[sessions.access]` 写在 `[[sessions]]` 里、订阅表之前。
    fn session_with_access(access: &str) -> String {
        ONE_SESSION.replacen(
            "        [[sessions.subscriptions]]",
            &format!("        [sessions.access]\n{access}\n\n        [[sessions.subscriptions]]"),
            1,
        )
    }

    #[test]
    fn removed_access_providers_fail_at_startup() {
        for provider in [
            "acb.access.all_rooms",
            "acb.access.elevated_in_rooms",
            "acb.access.privileged_origins",
        ] {
            let sessions = session_with_access(&format!("        provider = \"{provider}\""));
            let error = Config::parse_fixture(&config_with(ONE_CONNECTION, &sessions))
                .expect_err("旧 provider 必须启动失败");
            assert!(
                matches!(error, ConfigError::RemovedAccessProvider { .. }),
                "{provider} 应当报 RemovedAccessProvider，实际：{error}"
            );
            let text = error.to_string();
            assert!(
                text.contains("removed") || text.contains("已移除"),
                "错误文案要说清已移除：{text}"
            );
            assert!(text.contains("[grants]"), "错误文案要指向新写法：{text}");
        }
    }

    #[test]
    fn removed_access_keys_fail_at_startup() {
        for key in [
            "        rooms = [\"qq:group/999\"]",
            "        origins = [\"qq:private/7\"]",
        ] {
            let sessions = session_with_access(key);
            assert!(
                Config::parse_fixture(&config_with(ONE_CONNECTION, &sessions)).is_err(),
                "旧键 {key} 不能被静默忽略"
            );
        }
    }

    #[test]
    fn access_policy_rejects_bad_provider() {
        let bad_provider = session_with_access("        provider = \"subscribed\"");
        assert!(matches!(
            Config::parse_fixture(&config_with(ONE_CONNECTION, &bad_provider)),
            Err(ConfigError::BadAccessProvider { .. })
        ));
    }

    #[test]
    fn turn_timeout_is_configurable_and_disablable() {
        // 0 = 关闭：桥不掐，帧里带 0，harness 也不许自己兜底。
        let config = Config::parse_fixture(&format!(
            "turn_timeout_secs = 0\n{}",
            config_with(ONE_CONNECTION, ONE_SESSION)
        ))
        .expect("应当能解析");
        assert_eq!(config.turn_timeout_secs, 0);
        assert_eq!(config.turn_timeout_ms(), 0);

        // 自定义值原样进帧。
        let config = Config::parse_fixture(&format!(
            "turn_timeout_secs = 3600\n{}",
            config_with(ONE_CONNECTION, ONE_SESSION)
        ))
        .expect("应当能解析");
        assert_eq!(config.turn_timeout_ms(), 3_600_000);
    }

    #[test]
    fn transport_defaults_to_forward_websocket() {
        let config =
            Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        assert_eq!(config.connections[0].transport.as_str(), "ws-client");
    }

    /// relay 方式可配置（用户 2026-10-31 要求）：缺省 = 当前唯一实现 file-server。
    #[test]
    fn file_relay_mode_defaults_to_file_server() {
        let text = format!(
            "{}\n[file_relay]\nlisten = \"0.0.0.0:3131\"\npublic_base = \"http://10.89.0.1:3131\"",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&text).expect("能解析");
        let relay = config.file_relay.expect("应解析出 file_relay");
        assert_eq!(relay.mode, "file-server");
    }

    #[test]
    fn file_relay_accepts_explicit_file_server_mode() {
        let text = format!(
            "{}\n[file_relay]\nmode = \"file-server\"\nlisten = \"0.0.0.0:3131\"\npublic_base = \"http://10.89.0.1:3131\"",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&text).expect("能解析");
        assert_eq!(config.file_relay.unwrap().mode, "file-server");
    }

    /// 写了还不存在的方式要启动即报错，别等发文件时才炸。
    #[test]
    fn file_relay_rejects_unknown_mode() {
        let text = format!(
            "{}\n[file_relay]\nmode = \"carrier-pigeon\"\nlisten = \"0.0.0.0:3131\"\npublic_base = \"http://10.89.0.1:3131\"",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let error = Config::parse_fixture(&text).expect_err("应当报错");
        assert!(error.to_string().contains("file_relay.mode"), "{error}");
    }

    /// 写入侧上限缺省对齐平台的文件上传上限（4 GiB），不是旧的 128 MiB。
    #[test]
    fn file_relay_max_publish_bytes_defaults_to_4_gib() {
        let text = format!(
            "{}\n[file_relay]\nlisten = \"0.0.0.0:3131\"\npublic_base = \"http://10.89.0.1:3131\"",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&text).expect("能解析");
        let relay = config.file_relay.expect("应解析出 file_relay");
        assert_eq!(
            relay.max_publish_bytes,
            crate::relay::DEFAULT_MAX_PUBLISH_BYTES
        );
        assert_eq!(relay.max_publish_bytes, 4 * 1024 * 1024 * 1024);
    }

    /// 人类可读写法与整数字节都认；小数按字节向下取整。
    #[test]
    fn file_relay_max_publish_bytes_accepts_human_readable_sizes() {
        let cases = [
            ("\"4 GiB\"", 4 * 1024 * 1024 * 1024),
            ("\"4096 MiB\"", 4 * 1024 * 1024 * 1024),
            ("\"1.5 GiB\"", 1_610_612_736),
            ("\"1500 MB\"", 1_500_000_000),
            ("\"2048\"", 2048),
            ("2048", 2048),
        ];
        for (literal, expected) in cases {
            let text = format!(
                "{}\n[file_relay]\nlisten = \"0.0.0.0:3131\"\npublic_base = \"http://10.89.0.1:3131\"\nmax_publish_bytes = {literal}",
                config_with(ONE_CONNECTION, ONE_SESSION)
            );
            let config = Config::parse_fixture(&text).unwrap_or_else(|error| {
                panic!("{literal} 应当能解析：{error}");
            });
            let relay = config.file_relay.expect("应解析出 file_relay");
            assert_eq!(relay.max_publish_bytes, expected, "{literal}");
        }
    }

    /// 写不出来的值（未知单位、0、非数字）要在启动时报错并指出配置键。
    #[test]
    fn file_relay_max_publish_bytes_rejects_bad_values() {
        for literal in ["\"banana\"", "\"0\"", "\"4 gibberish\"", "0"] {
            let text = format!(
                "{}\n[file_relay]\nlisten = \"0.0.0.0:3131\"\npublic_base = \"http://10.89.0.1:3131\"\nmax_publish_bytes = {literal}",
                config_with(ONE_CONNECTION, ONE_SESSION)
            );
            let error = Config::parse_fixture(&text).expect_err(&format!("{literal} 应当报错"));
            let message = error.to_string();
            assert!(
                message.contains("[file_relay].max_publish_bytes"),
                "{literal} 的报错要指向配置键：{message}"
            );
        }
    }

    /// 连接级风险策略已移除：任何 `[connections.policy.risk]` 都启动失败并指向 grants。
    #[test]
    fn connection_policy_risk_fails_at_startup_with_a_migration_hint() {
        let text = format!(
            "{}\n[connections.policy.risk]\ndestructive = \"allow\"\n{}",
            ONE_CONNECTION, ONE_SESSION
        );
        let error = Config::parse_fixture(&text).expect_err("policy.risk 必须启动失败");
        let message = error.to_string();
        eprintln!("policy.risk 启动报错：{message}");
        assert!(message.contains("policy.risk"), "{message}");
        assert!(message.contains("grants.rules"), "{message}");
        assert!(message.contains("platform_op"), "{message}");
    }

    /// 旧 allow_destructive 不静默忽略也不映射：启动失败，报错给出可直接抄的 grants 写法。
    #[test]
    fn allow_destructive_fails_at_startup_with_a_migration_hint() {
        for legacy in [
            "allow_destructive = true",
            "allow_destructive = false",
            "allow_destructive = [\"group.mute\"]",
        ] {
            let connection = ONE_CONNECTION.replace(
                "account = \"10001\"",
                &format!("account = \"10001\"\n{legacy}"),
            );
            let error = Config::parse_fixture(&config_with(&connection, ONE_SESSION))
                .expect_err("旧键必须启动失败");
            let message = error.to_string();
            eprintln!("allow_destructive 启动报错：{message}");
            assert!(message.contains("allow_destructive"), "{message}");
            assert!(message.contains("[[grants.rules]]"), "{message}");
            assert!(message.contains("scope = \"platform_op\""), "{message}");
            assert!(message.contains("destructive"), "{message}");
        }
    }

    /// 顶层 [policy] 已移除：启动失败，指向 [grants]。
    #[test]
    fn top_level_policy_is_rejected_with_a_migration_hint() {
        let text = format!(
            "{}\n[policy.risk]\ndestructive = \"allow\"\n{}",
            ONE_CONNECTION, ONE_SESSION
        );
        let error = Config::parse_fixture(&text).expect_err("顶层 policy 必须启动失败");
        let message = error.to_string();
        assert!(message.contains("grants.rules"), "{message}");
    }

    /// 连接方式不再由 config 写死：manifest 在启动时决定支不支持。
    /// `ws-server` 能解析；onebot11 的 manifest 现在声明了 `ws-client` / `ws-server`。
    #[test]
    fn transport_is_parsed_and_checked_by_the_adapter_manifest() {
        let text = ONE_CONNECTION.replace(
            r#"kind = "onebot11""#,
            "kind = \"onebot11\"\ntransport = \"ws-server\"",
        );
        let config =
            Config::parse_fixture(&config_with(&text, ONE_SESSION)).expect("config 层不拦");
        assert_eq!(config.connections[0].transport, Transport::WsServer);
    }

    #[test]
    fn grants_parse_and_reject_unknown_fields() {
        let text = format!(
            "{}\n[grants]\n[grants.bot_admin]\naccounts = [{{ connection = \"qq\", account = \"10001\" }}]\n[grants.system_admin]\nconnection = \"qq\"\naccount = \"10002\"\n[[grants.rules]]\nid = \"fixture-grant-5\"\nsubject = \"bot_admin\"\nplace = \"private\"\nscope = \"subscribed\"\ncapability = [\"room_read\", \"room_write\", \"plugin\"]\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&text).expect("合法 grants");
        assert_eq!(config.grants.rules.len(), 1);
        assert!(config.grants.bot_admin.is_some());
        assert!(config.grants.system_admin.is_some());

        let unknown = format!(
            "{}\n[grants]\ntypo_field = \"x\"\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        assert!(
            Config::parse_fixture(&unknown).is_err(),
            "未知字段必须启动失败，不能静默忽略"
        );

        let bad_target = format!(
            "{}\n[[grants.rules]]\nid = \"fixture-grant-6\"\nsubject = \"system\"\nplace = \"none\"\nscope = \"any\"\ntarget = \"qq:group/1\"\ncapability = [\"room_read\"]\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        assert!(
            Config::parse_fixture(&bad_target).is_err(),
            "旧字符串 target 必须在启动期报错，不留兼容层"
        );

        let typed_target = format!(
            "{}\n[[grants.rules]]\nid = \"fixture-grant-7\"\nsubject = \"user\"\nplace = \"group\"\nscope = \"room\"\ntarget = {{ kind = \"room\", id = \"qq:group/1\" }}\ncapability = [\"room_read\"]\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        assert!(
            Config::parse_fixture(&typed_target).is_ok(),
            "typed target 应能解析"
        );

        let bad_capability = format!(
            "{}\n[[grants.rules]]\nid = \"fixture-grant-8\"\nsubject = \"system\"\nplace = \"none\"\nscope = \"any\"\ncapability = [\"root\"]\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        assert!(
            Config::parse_fixture(&bad_capability).is_err(),
            "能力枚举必须封闭"
        );
    }

    #[test]
    fn deps_thresholds_default_and_validate() {
        let config =
            Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        assert_eq!(
            config.deps.failure_threshold,
            DEFAULT_DEPS_FAILURE_THRESHOLD
        );
        assert_eq!(
            config.deps.failure_window_secs,
            DEFAULT_DEPS_FAILURE_WINDOW_SECS
        );

        let text = format!(
            "{}\n[deps]\nfailure_threshold = 5\nfailure_window_secs = 60\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&text).expect("能解析");
        assert_eq!(config.deps.failure_threshold, 5);
        assert_eq!(config.deps.failure_window_secs, 60);

        for field in ["failure_threshold", "failure_window_secs"] {
            let text = format!(
                "{}\n[deps]\n{field} = 0\n",
                config_with(ONE_CONNECTION, ONE_SESSION)
            );
            assert!(
                matches!(
                    Config::parse_fixture(&text),
                    Err(ConfigError::BadDepsConfig { .. })
                ),
                "{field} = 0 要拒绝"
            );
        }
    }

    #[test]
    fn lkg_thresholds_default_and_validate() {
        let config =
            Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        assert_eq!(config.lkg.max_failures, DEFAULT_LKG_MAX_FAILURES);
        assert_eq!(config.lkg.window_secs, DEFAULT_LKG_WINDOW_SECS);

        let text = format!(
            "{}\n[lkg]\nmax_failures = 5\nwindow_secs = 60\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&text).expect("能解析");
        assert_eq!(config.lkg.max_failures, 5);
        assert_eq!(config.lkg.window_secs, 60);

        for field in ["max_failures", "window_secs"] {
            let text = format!(
                "{}\n[lkg]\n{field} = 0\n",
                config_with(ONE_CONNECTION, ONE_SESSION)
            );
            assert!(matches!(
                Config::parse_fixture(&text),
                Err(ConfigError::BadLkgConfig { .. })
            ));
        }
    }

    #[test]
    fn provider_calls_timeout_defaults_and_validates() {
        let config =
            Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        assert_eq!(
            config.provider_calls.timeout_ms,
            DEFAULT_PROVIDER_CALL_TIMEOUT_MS
        );

        let text = format!(
            "{}\n[provider_calls]\ntimeout_ms = 100\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&text).expect("能解析");
        assert_eq!(config.provider_calls.timeout_ms, 100);

        let text = format!(
            "{}\n[provider_calls]\ntimeout_ms = 0\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&text).expect("能解析");
        assert_eq!(config.provider_calls.timeout_ms, 0, "0 = 关闭超时");

        let text = format!(
            "{}\n[provider_calls]\ntimeout_ms = {}\n",
            config_with(ONE_CONNECTION, ONE_SESSION),
            MAX_PROVIDER_CALL_TIMEOUT_MS + 1
        );
        assert!(matches!(
            Config::parse_fixture(&text),
            Err(ConfigError::BadProviderCallsTimeout { .. })
        ));
    }

    #[test]
    fn notifications_are_optional_and_inactive_without_url_or_topic() {
        let config =
            Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        assert!(config.notifications.is_none(), "没写整块就不加载");

        let missing_topic = format!(
            "{}\n[notifications]\nurl = \"http://127.0.0.1:8080\"\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&missing_topic).expect("能解析");
        assert!(
            !config.notifications.expect("块存在").active(),
            "缺 topic 时不加载"
        );

        let disabled = format!(
            "{}\n[notifications]\nenabled = false\nurl = \"http://127.0.0.1:8080\"\ntopic = {{ secret = \"acb\" }}\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&disabled).expect("能解析");
        assert!(
            !config.notifications.expect("块存在").active(),
            "总开关关掉时不加载"
        );
    }

    #[test]
    fn notifications_parse_defaults_and_validate() {
        let text = format!(
            "{}\n[notifications]\nurl = \"http://127.0.0.1:8080/\"\ntopic = {{ secret = \"/acb\" }}\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&text).expect("能解析");
        let notifications = config.notifications.expect("块存在");
        assert!(notifications.active());
        assert_eq!(notifications.url.as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(notifications.topic.as_deref(), Some("acb"));
        assert_eq!(
            notifications.min_interval_secs,
            DEFAULT_NOTIFICATION_MIN_INTERVAL_SECS
        );
        assert_eq!(notifications.in_band, InBandNoticeConfig::default());

        let custom = format!(
            "{}\n[notifications]\nurl = \"https://ntfy.example\"\ntopic = {{ secret = \"acb\" }}\nmin_interval_secs = 60\n[notifications.in_band]\nwindow_secs = 30\nroom_limit = 5\nmax_entries = 100\nremind_secs = 120\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&custom).expect("能解析");
        let notifications = config.notifications.expect("块存在");
        assert_eq!(notifications.min_interval_secs, 60);
        assert_eq!(notifications.in_band.window_secs, 30);
        assert_eq!(notifications.in_band.room_limit, 5);
        assert_eq!(notifications.in_band.max_entries, 100);
        assert_eq!(notifications.in_band.remind_secs, 120);

        let bad_url = format!(
            "{}\n[notifications]\nurl = \"127.0.0.1:8080\"\ntopic = {{ secret = \"acb\" }}\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        assert!(matches!(
            Config::parse_fixture(&bad_url),
            Err(ConfigError::BadNotificationsUrl { .. })
        ));

        for setting in [
            "min_interval_secs = 0",
            "in_band.window_secs = 0",
            "in_band.room_limit = 0",
            "in_band.max_entries = 0",
        ] {
            let text = format!(
                "{}\n[notifications]\nurl = \"http://127.0.0.1:8080\"\ntopic = {{ secret = \"acb\" }}\n{setting}\n",
                config_with(ONE_CONNECTION, ONE_SESSION)
            );
            assert!(matches!(
                Config::parse_fixture(&text),
                Err(ConfigError::BadNotificationsConfig { .. })
            ));
        }
    }

    #[test]
    fn queues_default_and_reject_zero() {
        let config =
            Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        assert_eq!(config.queues.inbound, DEFAULT_INBOUND_QUEUE);
        assert_eq!(config.queues.route, DEFAULT_ROUTE_QUEUE);

        let text = format!(
            "{}\n[queues]\ninbound = 64\nroute = 16\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&text).expect("能解析");
        assert_eq!(config.queues.inbound, 64);
        assert_eq!(config.queues.route, 16);

        for field in ["inbound", "route"] {
            let text = format!(
                "{}\n[queues]\n{field} = 0\n",
                config_with(ONE_CONNECTION, ONE_SESSION)
            );
            assert!(matches!(
                Config::parse_fixture(&text),
                Err(ConfigError::BadQueueLimit { .. })
            ));
        }
    }

    #[test]
    fn tool_plugins_are_parsed_and_validated() {
        let text = format!(
            "{}\n[[tool_plugins]]\nid = \"echo.tools\"\ncommand = \"/usr/bin/node\"\nargs = [\"/opt/echo.js\"]\ntimeout_ms = 500\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&text).expect("能解析");
        assert_eq!(config.tool_plugins.len(), 1);
        assert_eq!(config.tool_plugins[0].id, "echo.tools");
        assert_eq!(config.tool_plugins[0].command, "/usr/bin/node");
        assert_eq!(config.tool_plugins[0].args, vec!["/opt/echo.js"]);
        assert_eq!(config.tool_plugins[0].timeout(), 500);

        let defaults = format!(
            "{}\n[[tool_plugins]]\nid = \"echo.tools\"\ncommand = \"/bin/echo\"\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&defaults).expect("能解析");
        assert_eq!(config.tool_plugins[0].timeout(), 5000);

        let duplicate =
            format!("{text}\n[[tool_plugins]]\nid = \"echo.tools\"\ncommand = \"/bin/echo\"\n");
        assert!(matches!(
            Config::parse_fixture(&duplicate),
            Err(ConfigError::DuplicateToolPluginId { .. })
        ));

        let empty = format!(
            "{}\n[[tool_plugins]]\nid = \"echo.tools\"\ncommand = \"  \"\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        assert!(matches!(
            Config::parse_fixture(&empty),
            Err(ConfigError::EmptyToolPluginCommand { .. })
        ));

        let reserved = format!(
            "{}\n[[tool_plugins]]\nid = \"acb.tools\"\ncommand = \"/bin/echo\"\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        assert!(matches!(
            Config::parse_fixture(&reserved),
            Err(ConfigError::ReservedToolPluginId { .. })
        ));

        for timeout_ms in [1, 50, 70_000] {
            let text = format!(
                "{}\n[[tool_plugins]]\nid = \"echo.tools\"\ncommand = \"/bin/echo\"\ntimeout_ms = {timeout_ms}\n",
                config_with(ONE_CONNECTION, ONE_SESSION)
            );
            assert!(matches!(
                Config::parse_fixture(&text),
                Err(ConfigError::BadToolPluginTimeout { .. })
            ));
        }
    }

    #[test]
    fn dynamic_adapter_plugins_are_parsed_and_deduplicated() {
        let text = format!(
            "{}\n[[plugins]]\npath = \"/tmp/libdemo.so\"\nkind = \"demo\"\ntimeout_ms = 300\nversion = \"1.2.3\"\n",
            config_with(ONE_CONNECTION, ONE_SESSION)
        );
        let config = Config::parse_fixture(&text).expect("能解析");
        assert_eq!(config.plugins.len(), 1);
        assert_eq!(config.plugins[0].path, "/tmp/libdemo.so");
        assert_eq!(config.plugins[0].kind.as_deref(), Some("demo"));
        assert_eq!(config.plugins[0].timeout_ms, Some(300));
        assert_eq!(config.plugins[0].version.as_deref(), Some("1.2.3"));

        let duplicate = format!("{text}\n[[plugins]]\npath = \"/tmp/libdemo.so\"\n");
        assert!(matches!(
            Config::parse_fixture(&duplicate),
            Err(ConfigError::DuplicatePluginPath { .. })
        ));

        let empty = config_with(ONE_CONNECTION, ONE_SESSION) + "\n[[plugins]]\npath = \"  \"\n";
        assert!(matches!(
            Config::parse_fixture(&empty),
            Err(ConfigError::EmptyPluginPath)
        ));
    }

    #[test]
    fn two_connections_on_one_platform_are_allowed() {
        let two = format!(
            "{ONE_CONNECTION}\n{}",
            ONE_CONNECTION
                .replace("id = \"qq\"", "id = \"qq2\"")
                .replace("10001", "10002")
        );
        let config =
            Config::parse_fixture(&config_with(&two, ONE_SESSION)).expect("同一平台两条连接要允许");
        assert_eq!(config.connections.len(), 2);
        assert_eq!(config.connection("qq2").expect("有第二条").account, "10002");
    }

    #[test]
    fn duplicate_connection_id_is_rejected() {
        let twice = format!("{ONE_CONNECTION}\n{ONE_CONNECTION}");
        assert!(matches!(
            Config::parse_fixture(&config_with(&twice, ONE_SESSION)),
            Err(ConfigError::DuplicateConnection { .. })
        ));
    }

    #[test]
    fn connection_needs_a_login_account() {
        let no_account = ONE_CONNECTION.replace("        account = \"10001\"", "");
        assert!(matches!(
            Config::parse_fixture(&config_with(&no_account, ONE_SESSION)),
            Err(ConfigError::EmptyAccount { .. })
        ));
    }

    #[test]
    fn subscription_to_a_missing_connection_is_rejected() {
        let session = ONE_SESSION.replace("connection = \"qq\"", "connection = \"nope\"");
        assert!(matches!(
            Config::parse_fixture(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::UnknownConnection { .. })
        ));
    }

    #[test]
    fn same_room_twice_in_one_session_is_rejected() {
        let session = format!(
            "{ONE_SESSION}\n{}",
            r#"
        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"
        "#
        );
        assert!(matches!(
            Config::parse_fixture(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::DuplicateRoom { .. })
        ));
    }

    #[test]
    fn same_harness_groups_sessions() {
        let session = format!(
            "{ONE_SESSION}\n{}",
            r#"
        [[sessions]]
        id = "second"
        harness = "dsh"
        persona = "default"
        token = { secret = "t-main" }
        "#
        );
        let config = Config::parse_fixture(&config_with(ONE_CONNECTION, &session))
            .expect("同 harness 要算一组");
        let group = config.sessions_by_harness("dsh");
        assert_eq!(group.len(), 2);
        assert_eq!(group[1].id, "second");
    }

    #[test]
    fn shared_room_is_a_warning_naming_the_room() {
        let session = format!(
            "{ONE_SESSION}\n{}",
            r#"
        [[sessions]]
        id = "second"
        harness = "dsh"
        persona = "default"
        token = { secret = "t-second" }

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"
        "#
        );
        let config = Config::parse_fixture(&config_with(ONE_CONNECTION, &session)).expect("能解析");
        let warnings = config.warnings();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("qq:group/123")),
            "应当提醒房间撞车：{warnings:?}"
        );
    }

    #[test]
    fn url_token_query_is_a_warning_that_does_not_leak_the_value() {
        let connection = r#"
        [[connections]]
        id = "qq"
        platform = "qq"
        kind = "onebot11"
        url = "ws://127.0.0.1:3001?access_token=s3cret"
        account = "10001"
        "#;
        let config = Config::parse_fixture(&config_with(connection, ONE_SESSION)).expect("能解析");
        let warnings = config.warnings();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("qq") && warning.contains("access_token")),
            "应当提醒 URL 里的 token：{warnings:?}"
        );
        assert!(
            !warnings.iter().any(|warning| warning.contains("s3cret")),
            "提醒不能带出 token 值：{warnings:?}"
        );
    }

    #[test]
    fn empty_room_path_is_rejected() {
        let session = ONE_SESSION.replace("room = \"group/123\"", "room = \"  \"");
        assert!(matches!(
            Config::parse_fixture(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::BadRoom { .. })
        ));
    }

    #[test]
    fn zero_accumulate_is_rejected() {
        let session = ONE_SESSION.replace(
            "room = \"group/123\"",
            "room = \"group/123\"\n        [sessions.subscriptions.wake]\n        accumulate = 0",
        );
        assert!(matches!(
            Config::parse_fixture(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::BadAccumulate { .. })
        ));
    }

    #[test]
    fn empty_token_is_rejected() {
        let session = ONE_SESSION.replace(
            "token = { secret = \"t-main\" }",
            "token = { secret = \" \" }",
        );
        assert!(matches!(
            Config::parse_fixture(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::EmptyToken { .. })
        ));
    }

    #[test]
    fn unknown_persona_is_rejected() {
        let session = ONE_SESSION.replace("persona = \"default\"", "persona = \"nope\"");
        assert!(matches!(
            Config::parse_fixture(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::UnknownPersona { .. })
        ));
    }

    #[test]
    fn unknown_memory_is_rejected() {
        let session = ONE_SESSION.replace(
            "persona = \"default\"",
            "persona = \"default\"\n        memories = [\"nope\"]",
        );
        assert!(matches!(
            Config::parse_fixture(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::UnknownMemory { .. })
        ));
    }

    #[test]
    fn unknown_harness_is_rejected() {
        let session = ONE_SESSION.replace("harness = \"dsh\"", "harness = \"nope\"");
        assert!(matches!(
            Config::parse_fixture(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::UnknownHarness { .. })
        ));
    }

    #[test]
    fn duplicate_harness_is_rejected() {
        let session = format!(
            "{ONE_SESSION}\n{}",
            r#"
        [[harnesses]]
        id = "dsh"
        name = "另一个"
        "#
        );
        assert!(matches!(
            Config::parse_fixture(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::DuplicateHarness { .. })
        ));
    }

    #[test]
    fn session_name_defaults_to_id_and_model_is_optional() {
        // 不写 name / model：name 默认 = id，model 为 None。
        let config =
            Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        let session = &config.sessions[0];
        assert_eq!(session.name, "main");
        assert_eq!(session.model, None);

        // 写 name + model：都读进来。
        let with = ONE_SESSION.replace(
            "token = { secret = \"t-main\" }",
            "name = \"主群助手\"\n        model = \"deepseek-chat\"\n        token = { secret = \"t-main\" }",
        );
        let config = Config::parse_fixture(&config_with(ONE_CONNECTION, &with)).expect("能解析");
        let session = &config.sessions[0];
        assert_eq!(session.name, "主群助手");
        assert_eq!(session.model.as_deref(), Some("deepseek-chat"));
    }

    #[test]
    fn harness_is_a_first_class_object() {
        let config =
            Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        assert_eq!(config.harnesses.len(), 1);
        let harness = config.harness("dsh").expect("有 dsh");
        assert_eq!(harness.id, "dsh");
        assert_eq!(harness.name, "DeepSeek");
    }

    #[test]
    fn memory_kind_is_checked() {
        let session = format!(
            "{ONE_SESSION}\n{}",
            r#"
        [[memories]]
        id = "m1"
        name = "记忆"
        kind = "sqlite"
        location = "/tmp/m.db"
        "#
        );
        assert!(matches!(
            Config::parse_fixture(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::UnknownMemoryKind { .. })
        ));
    }

    #[test]
    fn wake_defaults_are_the_documented_switches() {
        let policy = WakePolicy::default();
        assert_eq!(policy.provider, ProviderId::builtin_wake());
        assert!(policy.at_mention && policy.reply && policy.name_mention);
        assert!(policy.event_tags.is_empty() && policy.ignore_event_tags.is_empty());
        assert_eq!(policy.accumulate, Some(DEFAULT_ACCUMULATE));
    }

    #[test]
    fn wake_provider_is_parsed_and_bad_ids_are_rejected() {
        let text = format!(
            "{ONE_CONNECTION}\n{ONE_SESSION}\n[sessions.subscriptions.wake]\nprovider = \"vendor.test\"\n"
        );
        let config = Config::parse_fixture(&text).expect("能解析");
        let wake = &config.session("main").expect("会话在").subscriptions[0].wake;
        assert_eq!(wake.provider.to_string(), "vendor.test");

        let bad = format!(
            "{ONE_CONNECTION}\n{ONE_SESSION}\n[sessions.subscriptions.wake]\nprovider = \"nodot\"\n"
        );
        assert!(matches!(
            Config::parse_fixture(&bad),
            Err(ConfigError::BadWakeProvider { .. })
        ));
    }

    #[test]
    fn wake_events_are_parsed_and_normalized() {
        let text = format!(
            "{ONE_CONNECTION}\n{ONE_SESSION}\n[sessions.subscriptions.wake]\n\
             event_tags = [\"reaction\", \" reaction \"]\n\
             events = [\"qq.poke\", \"qq.*\"]\n"
        );
        let config = Config::parse_fixture(&text).expect("能解析");
        let wake = &config.sessions[0].subscriptions[0].wake;
        assert_eq!(wake.event_tags, vec!["reaction"]);
        assert_eq!(wake.events, vec!["qq.poke", "qq.*"]);
    }

    #[test]
    fn plugin_options_are_parsed_and_passed_through() {
        let text = format!(
            "{ONE_CONNECTION}\n{ONE_SESSION}\n[sessions.subscriptions.wake]\n\
             provider = \"lua.frequent_speaker\"\n\
             [sessions.subscriptions.wake.options]\n\
             min_count = 5\n"
        );
        let config = Config::parse_fixture(&text).expect("能解析");
        let wake = &config.sessions[0].subscriptions[0].wake;
        assert_eq!(wake.provider.to_string(), "lua.frequent_speaker");
        assert_eq!(wake.options["min_count"], 5);

        let access = session_with_access(
            "        provider = \"lua.allow_read_only\"\n        [sessions.access.options]\n        allow_send = false",
        );
        let config = Config::parse_fixture(&config_with(ONE_CONNECTION, &access)).expect("能解析");
        assert_eq!(config.sessions[0].access.options["allow_send"], false);
    }

    #[test]
    fn wake_events_must_belong_to_the_room_platform() {
        // 唤醒配置按房间存：discord 的事件写在 qq 房间里永远不生效，启动就拒掉。
        let bad = format!(
            "{ONE_CONNECTION}\n{ONE_SESSION}\n[sessions.subscriptions.wake]\n\
             events = [\"discord.reaction_add\"]\n"
        );
        assert!(matches!(
            Config::parse_fixture(&bad),
            Err(ConfigError::BadWakeEvent { .. })
        ));

        // 同一会话的两个房间各配自己平台的事件。
        let discord = r#"
            [[connections]]
            id = "dc"
            platform = "discord"
            kind = "process"
            transport = "process"
            account = "bot"
        "#;
        let session = ONE_SESSION.replace(
            "        room = \"group/123\"",
            "        room = \"group/123\"\n\n        [[sessions.subscriptions]]\n        connection = \"dc\"\n        room = \"channel/456\"",
        );
        let text = format!(
            "{ONE_CONNECTION}\n{discord}\n{session}\n\
             [sessions.subscriptions.wake]\n\
             events = [\"discord.reaction_add\", \"discord.*\"]\n"
        );
        // 这个 wake 表落在最后一个订阅（discord 房间）上；qq 房间没有配事件。
        let config = Config::parse_fixture(&text).expect("每个房间配自己平台的事件应当能解析");
        let subscriptions = &config.sessions[0].subscriptions;
        assert!(subscriptions[0].wake.events.is_empty());
        assert_eq!(
            subscriptions[1].wake.events,
            vec!["discord.reaction_add", "discord.*"]
        );
    }

    #[test]
    fn legacy_poke_false_becomes_an_ignored_tag() {
        let text = format!(
            "{ONE_CONNECTION}\n{ONE_SESSION}\n[sessions.subscriptions.wake]\npoke = false\n"
        );
        let config = Config::parse_fixture(&text).expect("能解析");
        assert_eq!(
            config.sessions[0].subscriptions[0].wake.ignore_event_tags,
            vec!["poke"]
        );
    }

    #[test]
    fn worktrees_root_defaults_and_can_be_overridden() {
        let config =
            Config::parse_fixture(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        assert_eq!(config.worktrees_root, DEFAULT_WORKTREES_ROOT);

        // worktrees_root 是顶层字段，得放在任何 [[...]] 表格之前。
        let with_root =
            format!("worktrees_root = \"/srv/worktrees\"\n{ONE_CONNECTION}\n{ONE_SESSION}");
        let config = Config::parse_fixture(&with_root).expect("能解析");
        assert_eq!(config.worktrees_root, "/srv/worktrees");
    }

    #[test]
    fn quoting_my_message_wakes_by_default() {
        let policy = WakePolicy::default();
        let input = WakeInput {
            sender_id: "someone",
            text: "接着说",
            at_self: false,
            reply_to_self: true,
            has_body: true,
            event: None,
            platform: "qq",
            tags: &[],
            unread: 1,
        };
        assert_eq!(policy.decide(&input), WakeDecision::Wake);
    }

    #[test]
    fn reply_switch_can_be_turned_off() {
        let policy = WakePolicy {
            reply: false,
            ..WakePolicy::default()
        };
        let input = WakeInput {
            sender_id: "someone",
            text: "接着说",
            at_self: false,
            reply_to_self: true,
            has_body: true,
            event: None,
            platform: "qq",
            tags: &[],
            unread: 1,
        };
        assert_eq!(policy.decide(&input), WakeDecision::Stay);
    }

    #[test]
    fn reaction_on_my_message_wakes_by_the_reply_switch() {
        let tags: &[&str] = &["notice", "reaction", "group_msg_emoji_like", "reaction_add"];
        let input = WakeInput {
            sender_id: "someone",
            text: "",
            at_self: false,
            reply_to_self: true,
            has_body: false,
            event: Some("group_msg_emoji_like"),
            platform: "qq",
            tags,
            unread: 1,
        };
        assert_eq!(WakePolicy::default().decide(&input), WakeDecision::Wake);

        let policy = WakePolicy {
            reply: false,
            ..WakePolicy::default()
        };
        assert_eq!(
            policy.decide(&input),
            WakeDecision::Stay,
            "关掉 reply 后，反应和引用一起失效"
        );
    }

    #[test]
    fn reaction_on_someone_elses_message_does_not_wake() {
        let tags: &[&str] = &["notice", "reaction", "reaction_add"];
        let input = WakeInput {
            sender_id: "someone",
            text: "",
            at_self: false,
            reply_to_self: false,
            has_body: false,
            event: Some("group_msg_emoji_like"),
            platform: "qq",
            tags,
            unread: 1,
        };
        assert_eq!(WakePolicy::default().decide(&input), WakeDecision::Stay);
    }

    #[test]
    fn reaction_removal_is_recorded_but_does_not_wake() {
        let tags: &[&str] = &["notice", "reaction", "reaction_remove"];
        let input = WakeInput {
            sender_id: "someone",
            text: "",
            at_self: false,
            reply_to_self: true,
            has_body: false,
            event: Some("group_msg_emoji_like"),
            platform: "qq",
            tags,
            unread: 1,
        };
        assert_eq!(WakePolicy::default().decide(&input), WakeDecision::Stay);
    }

    #[test]
    fn wake_reason_names_the_cause_and_the_backlog() {
        let input = WakeInput {
            sender_id: "someone",
            text: "在吗",
            at_self: true,
            reply_to_self: false,
            has_body: true,
            event: None,
            platform: "qq",
            tags: &[],
            unread: 12,
        };
        let verdict = WakePolicy::default().decide_with_reason(&input);
        assert_eq!(verdict.decision, WakeDecision::Wake);
        assert_eq!(
            verdict.reason.as_deref(),
            Some("@ 了我，这轮共 12 条未读"),
            "主因和积压量一句话讲清"
        );
    }

    #[test]
    fn accumulate_reason_separates_threshold_from_the_actual_backlog() {
        let policy = WakePolicy {
            accumulate: Some(5),
            ..WakePolicy::default()
        };
        let input = WakeInput {
            sender_id: "someone",
            text: "闲聊",
            at_self: false,
            reply_to_self: false,
            has_body: true,
            event: None,
            platform: "qq",
            tags: &[],
            unread: 12,
        };
        let verdict = policy.decide_with_reason(&input);
        assert_eq!(verdict.decision, WakeDecision::Wake);
        assert_eq!(
            verdict.reason.as_deref(),
            Some("积压达到阈值（5 条），这轮共 12 条未读")
        );
    }

    #[test]
    fn keywords_can_wake() {
        let policy = WakePolicy {
            keywords: vec!["桥".into()],
            accumulate: None,
            ..WakePolicy::default()
        };
        // 关掉攒条数，避免它把「没命中关键词」也判成醒。
        let hit = WakeInput {
            sender_id: "someone",
            text: "这个桥能不能重启一下",
            at_self: false,
            reply_to_self: false,
            has_body: true,
            event: None,
            platform: "qq",
            tags: &[],
            unread: 1,
        };
        let miss = WakeInput {
            text: "今天天气不错",
            ..hit
        };
        assert_eq!(policy.decide(&hit), WakeDecision::Wake);
        assert_eq!(policy.decide(&miss), WakeDecision::Stay);
    }

    #[test]
    fn accumulating_enough_messages_wakes() {
        let policy = WakePolicy {
            accumulate: Some(3),
            ..WakePolicy::default()
        };
        let input = WakeInput {
            sender_id: "someone",
            text: "闲聊",
            at_self: false,
            reply_to_self: false,
            has_body: true,
            event: None,
            platform: "qq",
            tags: &[],
            unread: 2,
        };
        assert_eq!(policy.decide(&input), WakeDecision::Stay);

        let enough = WakeInput { unread: 3, ..input };
        assert_eq!(policy.decide(&enough), WakeDecision::Wake);
    }

    #[test]
    fn accumulate_off_means_no_waiting_for_a_number() {
        let policy = WakePolicy {
            accumulate: None,
            ..WakePolicy::default()
        };
        let input = WakeInput {
            sender_id: "someone",
            text: "闲聊",
            at_self: false,
            reply_to_self: false,
            has_body: true,
            event: None,
            platform: "qq",
            tags: &[],
            unread: 999,
        };
        // 不看条数：攒再多也不醒，得靠 @ / 引用 / 关键词这些。
        assert_eq!(policy.decide(&input), WakeDecision::Stay);
    }

    #[test]
    fn name_mention_wakes_only_when_names_configured() {
        let without_names = WakePolicy {
            accumulate: None,
            ..WakePolicy::default()
        };
        let input = WakeInput {
            sender_id: "someone",
            text: "小桥你说说看",
            at_self: false,
            reply_to_self: false,
            has_body: true,
            event: None,
            platform: "qq",
            tags: &[],
            unread: 1,
        };
        // 没配昵称就等于没法点名：默认开着也不能凭空认出一个名字。
        assert_eq!(without_names.decide(&input), WakeDecision::Stay);

        let with_names = WakePolicy {
            names: vec!["小桥".into()],
            ..without_names
        };
        assert_eq!(with_names.decide(&input), WakeDecision::Wake);
    }

    #[test]
    fn poke_wakes_only_when_it_targets_me() {
        let policy = WakePolicy::default();
        let aimed = WakeInput {
            sender_id: "someone",
            text: "",
            at_self: true,
            reply_to_self: false,
            has_body: true,
            event: Some("poke"),
            platform: "qq",
            tags: &["poke", "notice"],
            unread: 1,
        };
        let elsewhere = WakeInput {
            at_self: false,
            ..aimed
        };
        assert_eq!(policy.decide(&aimed), WakeDecision::Wake);
        // 拍别人不该把我也吵起来。
        assert_eq!(policy.decide(&elsewhere), WakeDecision::Stay);

        let deaf = WakePolicy {
            ignore_event_tags: vec!["poke".into()],
            ..policy
        };
        assert_eq!(deaf.decide(&aimed), WakeDecision::Stay);
    }

    #[test]
    fn event_tags_and_platform_qualified_events_wake() {
        let policy = WakePolicy {
            event_tags: vec!["reaction".into()],
            events: vec!["qq.poke".into()],
            accumulate: None,
            ..WakePolicy::default()
        };

        // 跨平台标签：不管哪个平台，适配器打上 reaction 就醒。
        let tagged = WakeInput {
            sender_id: "someone",
            text: "",
            at_self: false,
            reply_to_self: false,
            has_body: true,
            event: Some("reaction_add"),
            platform: "discord",
            tags: &["reaction", "notice"],
            unread: 1,
        };
        assert_eq!(policy.decide(&tagged), WakeDecision::Wake);

        // 平台限定名只在对应平台生效。
        let qualified = WakeInput {
            event: Some("poke"),
            platform: "qq",
            tags: &[],
            ..tagged
        };
        assert_eq!(policy.decide(&qualified), WakeDecision::Wake);
        let other_platform = WakeInput {
            platform: "discord",
            ..qualified
        };
        assert_eq!(policy.decide(&other_platform), WakeDecision::Stay);

        // 旧配置 poke = false 转成的忽略标签：拍了也不醒。
        let muted = WakePolicy {
            ignore_event_tags: vec!["poke".into()],
            ..policy.clone()
        };
        let aimed = WakeInput {
            event: Some("poke"),
            at_self: true,
            tags: &["poke", "notice"],
            ..tagged
        };
        assert_eq!(muted.decide(&aimed), WakeDecision::Stay);
    }

    #[test]
    fn plain_chatter_stays_quiet_under_the_default_policy() {
        let policy = WakePolicy {
            accumulate: None,
            ..WakePolicy::default()
        };
        let input = WakeInput {
            sender_id: "someone",
            text: "群里日常闲聊",
            at_self: false,
            reply_to_self: false,
            has_body: true,
            event: None,
            platform: "qq",
            tags: &[],
            unread: 1,
        };
        // 默认策略不能变成「谁说话都回」，否则群里没法待。
        assert_eq!(policy.decide(&input), WakeDecision::Stay);
    }

    #[test]
    fn ignored_speaker_never_wakes() {
        let policy = WakePolicy {
            ignore_speakers: vec!["noisy".into()],
            ..WakePolicy::default()
        };
        let input = WakeInput {
            sender_id: "noisy",
            text: "@bot",
            at_self: true,
            reply_to_self: true,
            has_body: true,
            event: None,
            platform: "qq",
            tags: &[],
            unread: 99,
        };
        assert_eq!(policy.decide(&input), WakeDecision::Stay);
    }

    // -----------------------------------------------------------------
    // config.d 分片
    // -----------------------------------------------------------------

    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "acb-config-drop-in-{label}-{}-{}",
                std::process::id(),
                TEMP_DIR_COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&path).expect("创建临时目录");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_config(path: &Path, text: &str) {
        std::fs::write(path, text).expect("写测试配置");
    }

    fn drop_in_base() -> String {
        format!(
            "delivery_limit = 10\n\n{}\n\n{}",
            config_with(ONE_CONNECTION, ONE_SESSION).trim(),
            r#"
[grants]

[[grants.rules]]
id = "fixture-grant-1"
subject = "user"
place = "group"
scope = "subscribed"
capability = ["room_read"]

[grants.rooms."qq:group/123"]
members = ["10001"]

[grants.accounts."qq:10001"]
approval_route = ["qq:group/123"]
"#
        )
    }

    const DROP_IN_FRAGMENT: &str = r#"
delivery_limit = 20

[[connections]]
id = "qq2"
platform = "qq"
kind = "onebot11"
url = "ws://127.0.0.1:3002"
account = "10002"

[[grants.rules]]
id = "fixture-grant-2"
subject = "user"
place = "group"
scope = "subscribed"
capability = ["room_read"]

[grants.rooms."qq:group/123"]
members = ["10002"]

[grants.accounts."qq:10001"]
approval_route = ["qq:group/456"]
"#;

    fn load_with_fragments(
        label: &str,
        base: &str,
        fragments: &[(&str, &str)],
    ) -> (TempDir, Result<LoadedConfig, ConfigError>) {
        let dir = TempDir::new(label);
        let base_path = dir.path().join("base.toml");
        let config_dir = dir.path().join("config.d");
        std::fs::create_dir_all(&config_dir).expect("创建 config.d");
        write_config(&base_path, base);
        for (name, text) in fragments {
            write_config(&config_dir.join(name), text);
        }
        let loaded = Config::load_fixture(&base_path, Some(&config_dir));
        (dir, loaded)
    }

    #[test]
    fn drop_in_scalar_override_and_array_append() {
        let (_dir, loaded) = load_with_fragments(
            "scalar-array",
            &drop_in_base(),
            &[("10-fragment.toml", DROP_IN_FRAGMENT)],
        );
        let loaded = loaded.expect("分片应能加载");
        assert_eq!(loaded.config.delivery_limit, 20, "标量后出现者覆盖");
        assert_eq!(loaded.config.connections.len(), 2, "数组追加 connection");
        assert_eq!(loaded.config.grants.rules.len(), 2, "数组追加 rule");
        let room = loaded
            .config
            .grants
            .rooms
            .get("qq:group/123")
            .expect("房间政策在");
        assert_eq!(room.members, vec!["10002"], "grants.rooms 按键覆盖");
        let account = loaded
            .config
            .grants
            .accounts
            .get("qq:10001")
            .expect("账号政策在");
        assert_eq!(
            account.approval_route,
            vec!["qq:group/456"],
            "grants.accounts 按键覆盖"
        );
        assert_eq!(loaded.sources.len(), 2);
        assert_eq!(loaded.sources[0].connections, 1);
        assert_eq!(loaded.sources[1].connections, 1);
        assert_eq!(loaded.sources[0].rules, 1);
        assert_eq!(loaded.sources[1].rules, 1);
    }

    #[test]
    fn drop_in_duplicate_rule_id_fails() {
        let duplicate_rule = r#"
[[grants.rules]]
id = "fixture-grant-1"
subject = "user"
place = "group"
scope = "subscribed"
capability = ["room_read"]
"#;
        let (_dir, loaded) = load_with_fragments(
            "identical-rule",
            &drop_in_base(),
            &[("10-same-rule.toml", duplicate_rule)],
        );
        let error = loaded.expect_err("重复 rule id 必须拒绝");
        assert!(
            error.to_string().contains("duplicate grant rule id"),
            "{error}"
        );
    }

    #[test]
    fn drop_in_different_rules_do_not_set_warning_flag() {
        let unique_rule = r#"
[[grants.rules]]
id = "fixture-grant-4"
subject = "user"
place = "group"
scope = "subscribed"
capability = ["room_write"]
"#;
        let (_dir, loaded) = load_with_fragments(
            "different-rule",
            &drop_in_base(),
            &[("10-other-rule.toml", unique_rule)],
        );
        let loaded = loaded.expect("分片应能加载");
        assert_eq!(loaded.config.grants.rules.len(), 2);
        assert!(!loaded.duplicate_rules, "不同规则不应误报");
    }

    #[test]
    fn drop_in_admin_qqs_are_deduped() {
        let fragment = "[[connections]]\nid = \"qq2\"\nplatform = \"qq\"\nkind = \"onebot11\"\nurl = \"ws://127.0.0.1:3002\"\naccount = \"10002\"\nadmin_qqs = [\"10002\", \"10002\", \"10003\"]\n";
        let (_dir, loaded) = load_with_fragments(
            "admin-dedupe",
            &drop_in_base(),
            &[("10-admin.toml", fragment)],
        );
        let loaded = loaded.expect("分片应能加载");
        let connection = loaded
            .config
            .connections
            .iter()
            .find(|connection| connection.id == "qq2")
            .expect("新连接在");
        assert_eq!(connection.admin_qqs, vec!["10002", "10003"]);
    }

    #[test]
    fn drop_in_ignores_non_toml_and_subdirectories() {
        let dir = TempDir::new("ignore");
        let base_path = dir.path().join("base.toml");
        let config_dir = dir.path().join("config.d");
        std::fs::create_dir_all(&config_dir).expect("创建 config.d");
        write_config(&base_path, &drop_in_base());
        write_config(
            &config_dir.join("10-fragment.toml"),
            "delivery_limit = 20\n",
        );
        write_config(
            &config_dir.join("10-fragment.toml.bak"),
            "delivery_limit = 99\n",
        );
        std::fs::write(config_dir.join("README.md"), "not config").expect("写非 toml");
        let nested = config_dir.join("sub");
        std::fs::create_dir_all(&nested).expect("创建子目录");
        write_config(&nested.join("20-nested.toml"), "delivery_limit = 99\n");

        let loaded = Config::load_fixture(&base_path, Some(&config_dir)).expect("能加载");
        assert_eq!(loaded.config.delivery_limit, 20);
        assert_eq!(loaded.sources.len(), 2, "只加载 base + 一个 .toml 文件");
    }

    #[test]
    fn drop_in_fragments_load_in_filename_order() {
        let base = drop_in_base();
        let (_dir, loaded) = load_with_fragments(
            "order",
            &base,
            &[
                ("30-c.toml", "delivery_limit = 3\n"),
                ("10-a.toml", "delivery_limit = 1\n"),
                ("20-b.toml", "delivery_limit = 2\n"),
            ],
        );
        let loaded = loaded.expect("三个分片应能加载");
        assert_eq!(loaded.config.delivery_limit, 3, "字典序最后一个覆盖");
        let names: Vec<&str> = loaded
            .sources
            .iter()
            .map(|source| source.path.as_str())
            .collect();
        assert!(names[1].contains("10-a.toml"), "{names:?}");
        assert!(names[2].contains("20-b.toml"), "{names:?}");
        assert!(names[3].contains("30-c.toml"), "{names:?}");
    }

    #[test]
    fn drop_in_duplicate_connection_id_names_both_files() {
        let fragment = "[[connections]]\nid = \"qq\"\nplatform = \"qq\"\nkind = \"onebot11\"\nurl = \"ws://127.0.0.1:3002\"\naccount = \"10002\"\n";
        let (_dir, loaded) = load_with_fragments(
            "dup-connection",
            &drop_in_base(),
            &[("10-dup.toml", fragment)],
        );
        match loaded.expect_err("重复 connection id 必须失败") {
            ConfigError::DuplicateAcrossFiles {
                kind,
                id,
                first,
                second,
            } => {
                assert_eq!(kind, "connections");
                assert_eq!(id, "qq");
                assert!(first.contains("base.toml"), "{first}");
                assert!(second.contains("10-dup.toml"), "{second}");
            }
            other => panic!("错误类型不对：{other}"),
        }
    }

    #[test]
    fn drop_in_duplicate_session_id_names_both_files() {
        let fragment = "[[sessions]]\nid = \"main\"\nharness = \"dsh\"\npersona = \"default\"\ntoken = { secret = \"t2\" }\n";
        let (_dir, loaded) =
            load_with_fragments("dup-session", &drop_in_base(), &[("10-dup.toml", fragment)]);
        match loaded.expect_err("重复 session id 必须失败") {
            ConfigError::DuplicateAcrossFiles {
                kind,
                id,
                first,
                second,
            } => {
                assert_eq!(kind, "sessions");
                assert_eq!(id, "main");
                assert!(first.contains("base.toml"), "{first}");
                assert!(second.contains("10-dup.toml"), "{second}");
            }
            other => panic!("错误类型不对：{other}"),
        }
    }

    #[test]
    fn drop_in_syntax_error_names_the_file() {
        let (_dir, loaded) = load_with_fragments(
            "syntax",
            &drop_in_base(),
            &[("10-bad.toml", "delivery_limit =\n")],
        );
        match loaded.expect_err("语法错误必须失败") {
            ConfigError::FileParseFailed { path, .. } => {
                assert!(path.contains("10-bad.toml"), "{path}");
            }
            other => panic!("错误类型不对：{other}"),
        }
    }

    #[test]
    fn drop_in_unknown_grants_field_names_the_file() {
        let (_dir, loaded) = load_with_fragments(
            "unknown-grants",
            &drop_in_base(),
            &[("10-bad.toml", "[grants]\nbogus = true\n")],
        );
        match loaded.expect_err("grants 未知字段必须失败") {
            ConfigError::FileParseFailed { path, .. } => {
                assert!(path.contains("10-bad.toml"), "{path}");
            }
            other => panic!("错误类型不对：{other}"),
        }
    }

    #[test]
    fn console_overlay_is_last_and_survives_base_regeneration() {
        let dir = TempDir::new("console-overlay");
        let base_path = dir.path().join("base.toml");
        let config_dir = dir.path().join("config.d");
        std::fs::create_dir_all(&config_dir).expect("创建 config.d");
        write_config(&base_path, &drop_in_base());
        write_config(
            &config_dir.join(CONSOLE_CONFIG_FILENAME),
            "delivery_limit = 90\n",
        );

        let first = Config::load_fixture(&base_path, Some(&config_dir)).expect("首次加载");
        assert_eq!(first.config.delivery_limit, 90, "90-console 最高优先级");
        assert_eq!(first.sources.len(), 2);
        assert!(
            first.sources[1].path.contains(CONSOLE_CONFIG_FILENAME),
            "{:?}",
            first.sources
        );

        let regenerated = drop_in_base().replace("delivery_limit = 10", "delivery_limit = 20");
        write_config(&base_path, &regenerated);
        let second = Config::load_fixture(&base_path, Some(&config_dir)).expect("重生成后加载");
        assert_eq!(
            second.config.delivery_limit, 90,
            "base 被重新生成后控制台覆盖仍然生效"
        );
    }

    #[test]
    fn store_objects_replace_the_file_sections() {
        let dir = TempDir::new("objects-inject");
        let base_path = dir.path().join("base.toml");
        let config_dir = dir.path().join("config.d");
        std::fs::create_dir_all(&config_dir).expect("创建 config.d");
        write_config(&base_path, &drop_in_base());

        // 库里只有一条会话，id 和文件里的不同：注入之后文件那条必须消失。
        let objects = crate::config_objects::ConfigObjects {
            sessions: vec![crate::config_objects::ConfigObject {
                id: "from-store".to_owned(),
                toml: "[[sessions]]\nid = \"from-store\"\nharness = \"dsh\"\npersona = \"default\"\ntoken = { secret = \"t-main\" }\n"
                    .to_owned(),
            }],
            personas: vec![crate::config_objects::ConfigObject {
                id: "default".to_owned(),
                toml: "[[personas]]\nid = \"default\"\nname = \"默认\"\ntext = \"来自库。\"\n"
                    .to_owned(),
            }],
            harnesses: vec![crate::config_objects::ConfigObject {
                id: "dsh".to_owned(),
                toml: "[[harnesses]]\nid = \"dsh\"\nname = \"DSH\"\n".to_owned(),
            }],
            ..Default::default()
        };
        let loaded =
            Config::load_fixture_with_objects(&base_path, Some(&config_dir), Some(&objects))
                .expect("注入后能加载");
        let ids: Vec<&str> = loaded
            .config
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect();
        assert_eq!(ids, vec!["from-store"], "库里的会话整类替换文件里的");

        // 不传对象时还是文件那一份（首次导入前的行为不变）。
        let file_only = Config::load_fixture_with_objects(&base_path, Some(&config_dir), None)
            .expect("按文件加载");
        assert!(
            file_only
                .config
                .sessions
                .iter()
                .any(|session| session.id == "main"),
            "没有对象时走文件"
        );
    }

    // ---- 密钥来源 ----

    fn config_with_secrets(declarations: &str) -> String {
        format!(
            "[secrets]\n{declarations}\n\n{}",
            config_with(ONE_CONNECTION, ONE_SESSION)
        )
    }

    fn set_secret_file_mode(path: &Path, mode: u32) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(path).expect("读文件权限").permissions();
            permissions.set_mode(mode);
            std::fs::set_permissions(path, permissions).expect("设置文件权限");
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
        }
    }

    #[test]
    fn env_secret_source_resolves_into_the_field() {
        let variable = "ACB_TEST_SECRET_ENV_OK";
        unsafe { std::env::set_var(variable, "s3cret") };
        let text = config_with_secrets(&format!(
            "[[secrets.sources]]\nname = \"t-main\"\nkind = \"env\"\nvariable = \"{variable}\""
        ));
        let config = Config::parse(&text).expect("env 来源能解析");
        assert_eq!(config.session("main").expect("会话在").token, "s3cret");
        unsafe { std::env::remove_var(variable) };
    }

    #[test]
    fn missing_env_secret_is_a_config_error() {
        let text = config_with_secrets(
            "[[secrets.sources]]\nname = \"t-main\"\nkind = \"env\"\nvariable = \"ACB_TEST_SECRET_SURELY_MISSING\"",
        );
        let error = Config::parse(&text).expect_err("环境变量缺失要报错");
        assert!(
            matches!(error, ConfigError::MissingSecretEnv { ref name, .. } if name == "t-main"),
            "{error}"
        );
    }

    #[test]
    fn literal_token_is_rejected() {
        let text = config_with(ONE_CONNECTION, ONE_SESSION)
            .replace("token = { secret = \"t-main\" }", "token = \"literal\"");
        let error = Config::parse(&text).expect_err("字面量 token 要拒绝");
        assert!(
            matches!(error, ConfigError::SecretLiteral { ref field } if field == "sessions[main].token"),
            "{error}"
        );
    }

    #[test]
    fn unknown_secret_reference_is_a_config_error() {
        let error = Config::parse(&config_with(ONE_CONNECTION, ONE_SESSION))
            .expect_err("未声明的来源要报错");
        assert!(
            matches!(error, ConfigError::UnknownSecret { ref name, .. } if name == "t-main"),
            "{error}"
        );
    }

    #[test]
    fn duplicate_secret_source_is_rejected() {
        let text = config_with_secrets(
            "[[secrets.sources]]\nname = \"t-main\"\nvariable = \"ACB_TEST_SECRET_DUP_A\"\n\n\
             [[secrets.sources]]\nname = \"t-main\"\nvariable = \"ACB_TEST_SECRET_DUP_B\"",
        );
        let error = Config::parse(&text).expect_err("同名来源要拒绝");
        assert!(
            matches!(error, ConfigError::DuplicateSecret { ref name } if name == "t-main"),
            "{error}"
        );
    }

    #[test]
    fn raw_file_secret_resolves() {
        let dir = TempDir::new("secret-raw");
        let path = dir.path().join("token");
        std::fs::write(&path, "file-secret\n").expect("写密钥文件");
        set_secret_file_mode(&path, 0o600);
        let text = config_with_secrets(&format!(
            "[[secrets.sources]]\nname = \"t-main\"\nkind = \"file\"\npath = \"{}\"",
            path.display()
        ));
        let config = Config::parse(&text).expect("file 来源能解析");
        assert_eq!(config.session("main").expect("会话在").token, "file-secret");
    }

    #[test]
    fn dotenv_file_uses_a_strict_subset() {
        let dir = TempDir::new("secret-dotenv");
        let path = dir.path().join("acb.env");
        std::fs::write(
            &path,
            "# 注释\n\nACB_OTHER=nope\nACB_TOKEN = dotenv-secret \n",
        )
        .expect("写 dotenv 文件");
        set_secret_file_mode(&path, 0o600);
        let text = config_with_secrets(&format!(
            "[[secrets.sources]]\nname = \"t-main\"\nkind = \"file\"\npath = \"{}\"\n\
             format = \"dotenv\"\nkey = \"ACB_TOKEN\"",
            path.display()
        ));
        let config = Config::parse(&text).expect("dotenv 能解析");
        assert_eq!(
            config.session("main").expect("会话在").token,
            "dotenv-secret"
        );
    }

    #[test]
    fn dotenv_does_not_strip_quotes() {
        let dir = TempDir::new("secret-dotenv-quotes");
        let path = dir.path().join("acb.env");
        std::fs::write(&path, "ACB_TOKEN=\"quoted\"\n").expect("写 dotenv 文件");
        set_secret_file_mode(&path, 0o600);
        let text = config_with_secrets(&format!(
            "[[secrets.sources]]\nname = \"t-main\"\nkind = \"file\"\npath = \"{}\"\n\
             format = \"dotenv\"\nkey = \"ACB_TOKEN\"",
            path.display()
        ));
        let config = Config::parse(&text).expect("dotenv 能解析");
        assert_eq!(
            config.session("main").expect("会话在").token,
            "\"quoted\"",
            "严格子集不处理引号"
        );
    }

    #[test]
    fn relative_secret_path_is_rejected() {
        let text = config_with_secrets(
            "[[secrets.sources]]\nname = \"t-main\"\nkind = \"file\"\npath = \"secrets/token\"",
        );
        let error =
            acb_i18n::with_locale("zh-CN", || Config::parse(&text)).expect_err("相对路径要拒绝");
        assert!(
            matches!(error, ConfigError::BadSecretSource { ref detail, .. } if detail.contains("绝对路径")),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn insecure_secret_file_mode_is_rejected() {
        let dir = TempDir::new("secret-mode");
        let path = dir.path().join("token");
        std::fs::write(&path, "file-secret\n").expect("写密钥文件");
        set_secret_file_mode(&path, 0o644);
        let text = config_with_secrets(&format!(
            "[[secrets.sources]]\nname = \"t-main\"\nkind = \"file\"\npath = \"{}\"",
            path.display()
        ));
        let error = Config::parse(&text).expect_err("权限过宽要拒绝");
        assert!(
            matches!(error, ConfigError::InsecureSecretFile { ref name, .. } if name == "t-main"),
            "{error}"
        );
    }

    #[test]
    fn empty_secret_file_is_rejected() {
        let dir = TempDir::new("secret-empty");
        let path = dir.path().join("token");
        std::fs::write(&path, "\n").expect("写空密钥文件");
        set_secret_file_mode(&path, 0o600);
        let text = config_with_secrets(&format!(
            "[[secrets.sources]]\nname = \"t-main\"\nkind = \"file\"\npath = \"{}\"",
            path.display()
        ));
        let error = Config::parse(&text).expect_err("空值要拒绝");
        assert!(
            matches!(error, ConfigError::EmptySecret { ref name, .. } if name == "t-main"),
            "{error}"
        );
    }

    #[test]
    fn secret_sources_merge_across_fragments() {
        let variable_a = "ACB_TEST_SECRET_FRAGMENT_A";
        let variable_b = "ACB_TEST_SECRET_FRAGMENT_B";
        unsafe {
            std::env::set_var(variable_a, "a");
            std::env::set_var(variable_b, "b");
        }
        let base = config_with_secrets(&format!(
            "[[secrets.sources]]\nname = \"t-main\"\nvariable = \"{variable_a}\""
        ));
        let dir = TempDir::new("secret-fragments");
        let base_path = dir.path().join("base.toml");
        let config_dir = dir.path().join("config.d");
        std::fs::create_dir_all(&config_dir).expect("创建 config.d");
        write_config(&base_path, &base);
        write_config(
            &config_dir.join("10-secrets.toml"),
            &format!("[[secrets.sources]]\nname = \"extra\"\nvariable = \"{variable_b}\"\n"),
        );
        let loaded = Config::load(&base_path, Some(&config_dir)).expect("跨分片合并来源");
        let entries = loaded.secrets.entries();
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["extra", "t-main"]);
        unsafe {
            std::env::remove_var(variable_a);
            std::env::remove_var(variable_b);
        }
    }

    #[test]
    fn duplicate_secret_source_across_fragments_is_rejected() {
        let base = config_with_secrets(
            "[[secrets.sources]]\nname = \"t-main\"\nvariable = \"ACB_TEST_SECRET_DUP_FRAG\"",
        );
        let dir = TempDir::new("secret-fragments-dup");
        let base_path = dir.path().join("base.toml");
        let config_dir = dir.path().join("config.d");
        std::fs::create_dir_all(&config_dir).expect("创建 config.d");
        write_config(&base_path, &base);
        write_config(
            &config_dir.join("10-secrets.toml"),
            "[[secrets.sources]]\nname = \"t-main\"\nvariable = \"ACB_TEST_SECRET_DUP_FRAG_2\"\n",
        );
        let error = Config::load(&base_path, Some(&config_dir)).expect_err("跨文件重名要拒绝");
        assert!(
            matches!(error, ConfigError::DuplicateAcrossFiles { ref kind, .. } if kind == "secrets.sources"),
            "{error}"
        );
    }

    #[test]
    fn console_overlay_must_not_declare_secret_sources() {
        let variable = "ACB_TEST_SECRET_OVERLAY";
        unsafe { std::env::set_var(variable, "v") };
        let base = config_with_secrets(&format!(
            "[[secrets.sources]]\nname = \"t-main\"\nvariable = \"{variable}\""
        ));
        let dir = TempDir::new("secret-overlay");
        let base_path = dir.path().join("base.toml");
        let config_dir = dir.path().join("config.d");
        std::fs::create_dir_all(&config_dir).expect("创建 config.d");
        write_config(&base_path, &base);
        let error = Config::load_with_console_overlay(
            &base_path,
            &config_dir,
            "[[secrets.sources]]\nname = \"extra\"\nvariable = \"NOPE\"\n",
            None,
        )
        .expect_err("覆盖层不能声明来源");
        assert!(matches!(error, ConfigError::ConsoleSecretSource), "{error}");
        unsafe { std::env::remove_var(variable) };
    }
}
