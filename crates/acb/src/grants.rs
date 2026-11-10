//! `acbctl grants`：只读的判定解释、矩阵与差异。
//!
//! 通过 [`PolicySource`] 读取策略快照并调用 kernel 的 [`CorePolicy`]；本期唯一实现是
//! [`ConfigSource`]（base TOML + `config.d`）。`policy.db` 上线后新增 `DbSource`，
//! 命令面不变。所有输出都带 `source` 标识，避免把静态快照判定误当成运行时判定。

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use acb_kernel::grants::{GrantDecision, GrantQuery, GrantRule};
use acb_kernel::{
    AccessTarget, AccessTargetKind, Config, CorePolicy, GrantCapability, GrantSubject,
    GrantsConfig, Place, Policy, RoomId, RoomScope,
};
use serde::{Deserialize, Serialize};

const EXIT_USAGE: u8 = 2;
const EXIT_UNKNOWN_CAPABILITY: u8 = 3;
const EXIT_RULE_NOT_FOUND: u8 = 4;
const EXIT_CONFIG: u8 = 5;
const EXIT_DIFF_CHANGED: u8 = 6;

const SUBJECTS: [GrantSubject; 4] = [
    GrantSubject::System,
    GrantSubject::SystemAdmin,
    GrantSubject::BotAdmin,
    GrantSubject::User,
];

const CAPABILITIES: [GrantCapability; 10] = [
    GrantCapability::RoomRead,
    GrantCapability::RoomWrite,
    GrantCapability::ConnectionRead,
    GrantCapability::Platform,
    GrantCapability::Destructive,
    GrantCapability::Critical,
    GrantCapability::AccountRead,
    GrantCapability::AccountSecret,
    GrantCapability::AccessRequest,
    GrantCapability::Plugin,
];

const MATRIX_JSON_KIND: &str = "acb-grants-matrix";
const DIFF_JSON_KIND: &str = "acb-grants-diff";
const EXPORT_SCHEMA: u32 = 1;

#[derive(Debug)]
struct CliError {
    code: u8,
    message: String,
}

impl CliError {
    fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn usage(message: impl Into<String>) -> Self {
        Self::new(EXIT_USAGE, message)
    }

    fn config(message: impl Into<String>) -> Self {
        Self::new(EXIT_CONFIG, message)
    }
}

pub fn command(args: &[String]) -> Result<ExitCode, String> {
    let Some(subcommand) = args.first() else {
        print_help();
        return Ok(ExitCode::from(EXIT_USAGE));
    };
    let rest = &args[1..];
    let result = match subcommand.as_str() {
        "-h" | "--help" | "help" => {
            print_help();
            Ok(ExitCode::SUCCESS)
        }
        "explain" => explain_command(rest),
        "matrix" => matrix_command(rest),
        "diff" => diff_command(rest),
        other => Err(CliError::usage(format!(
            "unknown grants subcommand: {other}"
        ))),
    };
    match result {
        Ok(code) => Ok(code),
        Err(error) => {
            eprintln!("acbctl grants: {}", error.message);
            Ok(ExitCode::from(error.code))
        }
    }
}

// ---------------------------------------------------------------------------
// 策略源抽象

trait PolicySource {
    fn label(&self) -> &str;
    fn kind(&self) -> &'static str;
    fn grants(&self) -> &GrantsConfig;
    fn subscriptions(&self) -> &[RoomId];
    fn rooms(&self) -> BTreeSet<RoomId>;
    fn decide(
        &self,
        subject: GrantSubject,
        place: Place,
        capability: GrantCapability,
        target: &AccessTarget,
        policy_version: i64,
    ) -> Result<GrantDecision, CliError> {
        let query = GrantQuery {
            subject,
            place,
            capability,
            actor: None,
            target,
            // CLI 只回答「这个目标对这套主体/场所是否成立」，没有 turn 概念；
            // access_request 的发起房间只能由运行时提供，这里按缺省处理（deny）。
            origin: None,
            subscriptions: self.subscriptions(),
            platform_op: None,
            policy_version,
            grants: self.grants(),
        };
        CorePolicy::new()
            .decide(&query)
            .map_err(|error| CliError::config(format!("policy evaluation failed: {error:?}")))
    }
}

struct ConfigSource {
    label: String,
    config: Config,
    subscriptions: Vec<RoomId>,
}

impl ConfigSource {
    fn new(label: String, config: Config) -> Self {
        let subscriptions = subscriptions_of(&config);
        Self {
            label,
            config,
            subscriptions,
        }
    }
}

impl PolicySource for ConfigSource {
    fn label(&self) -> &str {
        &self.label
    }

    fn kind(&self) -> &'static str {
        "config"
    }

    fn grants(&self) -> &GrantsConfig {
        &self.config.grants
    }

    fn subscriptions(&self) -> &[RoomId] {
        &self.subscriptions
    }

    fn rooms(&self) -> BTreeSet<RoomId> {
        collect_rooms(&self.config)
    }
}

fn load_config_source(path: &Path, config_dir: Option<&Path>) -> Result<ConfigSource, CliError> {
    let config_dir = config_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| crate::config_source::default_config_dir(path));
    // 和 `sessions` / `secrets resolve` 走同一个入口：业务对象与策略都以库为准。
    // 只读文件会解释错权限——`[grants]` 自 schema 13 起只是导入源。
    let loaded = crate::config_source::load(path, Some(&config_dir)).map_err(CliError::config)?;
    let label = format!("config:{}+{}", path.display(), config_dir.display());
    Ok(ConfigSource::new(label, loaded.config))
}

fn subscriptions_of(config: &Config) -> Vec<RoomId> {
    let mut rooms = BTreeSet::new();
    for session in &config.sessions {
        for subscription in &session.subscriptions {
            rooms.insert(subscription.room.clone());
        }
    }
    rooms.into_iter().collect()
}

/// 配置里出现过的全部房间：订阅、`grants.rooms` 键、规则 target。
fn collect_rooms(config: &Config) -> BTreeSet<RoomId> {
    let mut rooms = BTreeSet::new();
    for session in &config.sessions {
        for subscription in &session.subscriptions {
            rooms.insert(subscription.room.clone());
        }
    }
    for key in config.grants.rooms.keys() {
        if let Ok(room) = RoomId::parse(key) {
            rooms.insert(room);
        }
    }
    for rule in config.grants.rules.iter().chain(config.grants.deny.iter()) {
        if let Some(target) = &rule.target {
            if target.kind == AccessTargetKind::Room {
                if let Ok(room) = RoomId::parse(&target.id) {
                    rooms.insert(room);
                }
            }
        }
    }
    rooms
}

fn room_scope(room: &RoomId) -> RoomScope {
    let direct = room
        .path
        .first()
        .is_some_and(|segment| matches!(segment.as_str(), "private" | "direct" | "dm" | "friend"));
    if direct {
        RoomScope::Direct
    } else {
        RoomScope::MultiParty
    }
}

// ---------------------------------------------------------------------------
// 参数解析

fn long_flag(arg: &str) -> Option<(&str, Option<&str>)> {
    let rest = arg.strip_prefix("--")?;
    match rest.split_once('=') {
        Some((name, value)) => Some((name, Some(value))),
        None => Some((rest, None)),
    }
}

fn flag_value(
    args: &[String],
    index: &mut usize,
    inline: Option<&str>,
    flag: &str,
) -> Result<String, CliError> {
    if let Some(value) = inline {
        if value.is_empty() {
            return Err(CliError::usage(format!("--{flag} requires a value")));
        }
        return Ok(value.to_string());
    }
    *index += 1;
    args.get(*index)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| CliError::usage(format!("--{flag} requires a value")))
}

fn split_list(value: &str, flag: &str) -> Result<Vec<String>, CliError> {
    let items: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect();
    if items.is_empty() {
        return Err(CliError::usage(format!(
            "--{flag} requires at least one value"
        )));
    }
    Ok(items)
}

fn parse_subject(value: &str) -> Result<GrantSubject, CliError> {
    match value.trim() {
        "system" => Ok(GrantSubject::System),
        "system_admin" => Ok(GrantSubject::SystemAdmin),
        "bot_admin" => Ok(GrantSubject::BotAdmin),
        "user" => Ok(GrantSubject::User),
        other => Err(CliError::usage(format!(
            "unknown subject {other:?}; expected system/system_admin/bot_admin/user"
        ))),
    }
}

fn parse_capability(value: &str) -> Result<GrantCapability, CliError> {
    match value.trim() {
        "room_read" => Ok(GrantCapability::RoomRead),
        "room_write" => Ok(GrantCapability::RoomWrite),
        "connection_read" => Ok(GrantCapability::ConnectionRead),
        "platform" => Ok(GrantCapability::Platform),
        "destructive" => Ok(GrantCapability::Destructive),
        "critical" => Ok(GrantCapability::Critical),
        "account_read" => Ok(GrantCapability::AccountRead),
        "account_secret" => Ok(GrantCapability::AccountSecret),
        "access_request" => Ok(GrantCapability::AccessRequest),
        "plugin" => Ok(GrantCapability::Plugin),
        other => Err(CliError::new(
            EXIT_UNKNOWN_CAPABILITY,
            format!(
                "unknown capability {other:?}; expected room_read/room_write/connection_read/\
                 platform/destructive/critical/account_read/account_secret/access_request/plugin"
            ),
        )),
    }
}

enum PlaceArg {
    Private,
    Group,
    Console,
    None,
    Room(RoomId),
}

impl PlaceArg {
    fn parse(value: &str) -> Result<Self, CliError> {
        if let Some(rest) = value.strip_prefix("room:") {
            let room = RoomId::parse(rest)
                .map_err(|error| CliError::usage(format!("invalid room {rest:?}: {error}")))?;
            return Ok(Self::Room(room));
        }
        match value.trim() {
            "private" => Ok(Self::Private),
            "group" => Ok(Self::Group),
            "console" => Ok(Self::Console),
            "none" => Ok(Self::None),
            other => Err(CliError::usage(format!(
                "unknown place {other:?}; expected private/group/console/none/room:<id>"
            ))),
        }
    }

    fn place(&self) -> Place {
        match self {
            Self::Private => Place::Room(RoomScope::Direct),
            Self::Group => Place::Room(RoomScope::MultiParty),
            Self::Console => Place::Console,
            Self::None => Place::None,
            Self::Room(room) => Place::Room(room_scope(room)),
        }
    }

    fn room(&self) -> Option<&RoomId> {
        match self {
            Self::Room(room) => Some(room),
            _ => None,
        }
    }
}

fn parse_target(value: &str) -> Result<AccessTarget, CliError> {
    if let Some(rest) = value.strip_prefix("room:") {
        return parse_room_target(rest);
    }
    if let Some(rest) = value.strip_prefix("connection:") {
        if rest.trim().is_empty() {
            return Err(CliError::usage("connection target requires an id"));
        }
        return Ok(AccessTarget::connection(rest));
    }
    if let Some(rest) = value.strip_prefix("plugin_tool:") {
        if rest.trim().is_empty() {
            return Err(CliError::usage("plugin_tool target requires an id"));
        }
        return Ok(AccessTarget {
            kind: AccessTargetKind::PluginTool,
            id: rest.to_string(),
            sub: None,
        });
    }
    if let Some(rest) = value.strip_prefix("member:") {
        return parse_scoped_target(rest, AccessTargetKind::Member);
    }
    if let Some(rest) = value.strip_prefix("message:") {
        return parse_scoped_target(rest, AccessTargetKind::Message);
    }
    if let Some(rest) = value.strip_prefix("file:") {
        return parse_scoped_target(rest, AccessTargetKind::File);
    }
    if let Some(rest) = value.strip_prefix("account:") {
        let Some((connection, account)) = rest.split_once(':') else {
            return Err(CliError::usage(
                "account target must be account:<connection>:<account>",
            ));
        };
        if connection.trim().is_empty() || account.trim().is_empty() {
            return Err(CliError::usage(
                "account target must be account:<connection>:<account>",
            ));
        }
        return Ok(AccessTarget::account(connection, account));
    }
    if let Some(rest) = value.strip_prefix("platform_op:") {
        let Some((connection, method)) = rest.split_once(':') else {
            return Err(CliError::usage(
                "platform_op target must be platform_op:<connection>:<method>",
            ));
        };
        if connection.trim().is_empty() || method.trim().is_empty() {
            return Err(CliError::usage(
                "platform_op target must be platform_op:<connection>:<method>",
            ));
        }
        return Ok(AccessTarget::platform_op(connection, method));
    }
    parse_room_target(value)
}

fn parse_room_target(value: &str) -> Result<AccessTarget, CliError> {
    let room = RoomId::parse(value.trim())
        .map_err(|error| CliError::usage(format!("invalid target room {value:?}: {error}")))?;
    Ok(AccessTarget::room(&room))
}

fn parse_scoped_target(value: &str, kind: AccessTargetKind) -> Result<AccessTarget, CliError> {
    let Some((room, id)) = value.rsplit_once(':') else {
        return Err(CliError::usage(format!(
            "{} target must be {}:<room>:<id>",
            kind.as_str(),
            kind.as_str()
        )));
    };
    if id.trim().is_empty() {
        return Err(CliError::usage(format!(
            "{} target requires an id",
            kind.as_str()
        )));
    }
    let room = RoomId::parse(room.trim()).map_err(|error| {
        CliError::usage(format!(
            "invalid room {room:?} in {} target: {error}",
            kind.as_str()
        ))
    })?;
    Ok(AccessTarget {
        kind,
        id: id.to_string(),
        sub: Some(room.to_string()),
    })
}

fn target_spec(target: &AccessTarget) -> String {
    match target.kind {
        AccessTargetKind::Room => format!("room:{}", target.id),
        AccessTargetKind::Connection => format!("connection:{}", target.id),
        AccessTargetKind::Account => format!(
            "account:{}:{}",
            target.sub.as_deref().unwrap_or(""),
            target.id
        ),
        AccessTargetKind::PlatformOp => format!(
            "platform_op:{}:{}",
            target.sub.as_deref().unwrap_or(""),
            target.id
        ),
        AccessTargetKind::PluginTool => format!("plugin_tool:{}", target.id),
        AccessTargetKind::Member | AccessTargetKind::Message | AccessTargetKind::File => format!(
            "{}:{}:{}",
            target.kind.as_str(),
            target.sub.as_deref().unwrap_or(""),
            target.id
        ),
    }
}

fn target_subscribed(target: &AccessTarget, subscriptions: &[RoomId]) -> bool {
    match target.kind {
        AccessTargetKind::Room => subscriptions
            .iter()
            .any(|room| room.to_string() == target.id),
        AccessTargetKind::Connection => subscriptions
            .iter()
            .any(|room| room.connection == target.id),
        AccessTargetKind::Account | AccessTargetKind::PlatformOp => {
            target.sub.as_deref().is_some_and(|connection| {
                subscriptions
                    .iter()
                    .any(|room| room.connection == connection)
            })
        }
        AccessTargetKind::Member | AccessTargetKind::Message | AccessTargetKind::File => {
            target.sub.as_deref().is_some_and(|room| {
                subscriptions
                    .iter()
                    .any(|listed| listed.to_string() == room)
            })
        }
        AccessTargetKind::PluginTool => !subscriptions.is_empty(),
    }
}

// ---------------------------------------------------------------------------
// explain

#[derive(Serialize)]
struct ExplainOutput {
    source: String,
    policy_version: i64,
    query: ExplainQuery,
    decision: ExplainDecision,
    rule_matched: Option<bool>,
}

#[derive(Serialize)]
struct ExplainQuery {
    subject: String,
    place: String,
    capability: String,
    target: String,
    subscribed: bool,
}

#[derive(Serialize)]
struct ExplainDecision {
    allow: bool,
    basis: String,
    rule_id: String,
    reason: String,
}

fn explain_command(args: &[String]) -> Result<ExitCode, CliError> {
    let mut config = PathBuf::from("config.toml");
    let mut config_dir = None;
    let mut policy_version = 0_i64;
    let mut json = false;
    let mut place = None;
    let mut capability = None;
    let mut target = None;
    let mut rule = None;
    let mut positional = Vec::new();

    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some((name, inline)) = long_flag(arg) {
            match name {
                "config" => {
                    config = PathBuf::from(flag_value(args, &mut index, inline, name)?);
                }
                "config-dir" => {
                    config_dir = Some(PathBuf::from(flag_value(args, &mut index, inline, name)?));
                }
                "policy-version" => {
                    policy_version = flag_value(args, &mut index, inline, name)?
                        .parse()
                        .map_err(|_| CliError::usage("--policy-version must be an integer"))?;
                }
                "place" => {
                    place = Some(PlaceArg::parse(&flag_value(
                        args, &mut index, inline, name,
                    )?)?);
                }
                "capability" => {
                    capability = Some(parse_capability(&flag_value(
                        args, &mut index, inline, name,
                    )?)?);
                }
                "target" => {
                    target = Some(parse_target(&flag_value(args, &mut index, inline, name)?)?);
                }
                "rule" => {
                    rule = Some(flag_value(args, &mut index, inline, name)?);
                }
                "json" => {
                    if inline.is_some() {
                        return Err(CliError::usage("--json does not take a value"));
                    }
                    json = true;
                }
                other => {
                    return Err(CliError::usage(format!(
                        "unknown option for grants explain: --{other}"
                    )));
                }
            }
        } else {
            positional.push(arg.clone());
        }
        index += 1;
    }

    if positional.len() != 1 {
        return Err(CliError::usage(
            "usage: acbctl grants explain <subject> --place <place> --capability <cap>",
        ));
    }
    let subject = parse_subject(&positional[0])?;
    let Some(place) = place else {
        return Err(CliError::usage(
            "grants explain requires --place <private|group|console|none|room:<id>>",
        ));
    };
    let Some(capability) = capability else {
        return Err(CliError::usage(
            "grants explain requires --capability <capability>",
        ));
    };
    let target = match target {
        Some(target) => target,
        None => match place.room() {
            Some(room) => AccessTarget::room(room),
            None => {
                return Err(CliError::usage(
                    "grants explain requires --target unless --place is room:<id>",
                ));
            }
        },
    };

    let source = load_config_source(&config, config_dir.as_deref())?;
    if let Some(id) = &rule {
        if !rule_exists(&source.config, id) {
            return Err(CliError::new(
                EXIT_RULE_NOT_FOUND,
                format!("grant rule not found: {id}"),
            ));
        }
    }

    let decision = source.decide(subject, place.place(), capability, &target, policy_version)?;
    let rule_matched = rule.as_ref().map(|id| {
        decision
            .basis
            .split(',')
            .any(|entry| entry == format!("rule:{id}") || entry == format!("deny:{id}"))
    });
    let subscribed = target_subscribed(&target, source.subscriptions());

    if json {
        let output = ExplainOutput {
            source: source.label().to_string(),
            policy_version: decision.policy_version,
            query: ExplainQuery {
                subject: subject.as_str().to_string(),
                place: place.place().as_str().to_string(),
                capability: capability.as_str().to_string(),
                target: target_spec(&target),
                subscribed,
            },
            decision: ExplainDecision {
                allow: decision.allow,
                basis: decision.basis.clone(),
                rule_id: decision.rule_id.clone(),
                reason: decision.reason.clone(),
            },
            rule_matched,
        };
        let text = serde_json::to_string_pretty(&output)
            .map_err(|error| CliError::config(format!("serialize explain output: {error}")))?;
        println!("{text}");
    } else {
        println!("source: {}", source.label());
        println!("policy_version: {}", decision.policy_version);
        println!(
            "query: subject={} place={} capability={} target={} subscribed={}",
            subject.as_str(),
            place.place().as_str(),
            capability.as_str(),
            target_spec(&target),
            subscribed
        );
        println!(
            "decision: {}",
            if decision.allow { "allow" } else { "deny" }
        );
        println!("basis: {}", decision.basis);
        println!("rule_id: {}", decision.rule_id);
        println!("reason: {}", decision.reason);
        if let Some(matched) = rule_matched {
            println!("rule_matched: {matched}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn rule_exists(config: &Config, id: &str) -> bool {
    config
        .grants
        .rules
        .iter()
        .chain(config.grants.deny.iter())
        .any(|rule: &GrantRule| rule.id == id)
}

// ---------------------------------------------------------------------------
// matrix

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlaceFilter {
    Group,
    Private,
    All,
}

impl PlaceFilter {
    fn parse(value: &str) -> Result<Self, CliError> {
        match value.trim() {
            "group" => Ok(Self::Group),
            "private" => Ok(Self::Private),
            "all" => Ok(Self::All),
            other => Err(CliError::usage(format!(
                "unknown place filter {other:?}; expected group/private/all"
            ))),
        }
    }

    fn includes(self, scope: RoomScope) -> bool {
        match self {
            Self::Group => scope == RoomScope::MultiParty,
            Self::Private => scope == RoomScope::Direct,
            Self::All => true,
        }
    }
}

#[derive(Clone)]
struct Selection {
    subjects: Vec<GrantSubject>,
    capabilities: Vec<GrantCapability>,
    rooms: Option<Vec<RoomId>>,
    place: PlaceFilter,
    policy_version: i64,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            subjects: SUBJECTS.to_vec(),
            capabilities: CAPABILITIES.to_vec(),
            rooms: None,
            place: PlaceFilter::Group,
            policy_version: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Tsv,
    Json,
}

impl OutputFormat {
    fn parse(value: &str) -> Result<Self, CliError> {
        match value.trim() {
            "tsv" => Ok(Self::Tsv),
            "json" => Ok(Self::Json),
            other => Err(CliError::usage(format!(
                "unknown format {other:?}; expected tsv/json"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cell {
    source: String,
    subject: String,
    place: String,
    room: String,
    capability: String,
    allow: bool,
    basis: String,
    rule_id: String,
    policy_version: i64,
    subscribed: bool,
}

impl Cell {
    fn key(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.subject, self.place, self.room, self.capability
        )
    }
}

fn matrix_command(args: &[String]) -> Result<ExitCode, CliError> {
    let mut config = PathBuf::from("config.toml");
    let mut config_dir = None;
    let mut selection = Selection::default();
    let mut format = OutputFormat::Tsv;
    let mut positional = Vec::new();

    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some((name, inline)) = long_flag(arg) {
            match name {
                "config" => {
                    config = PathBuf::from(flag_value(args, &mut index, inline, name)?);
                }
                "config-dir" => {
                    config_dir = Some(PathBuf::from(flag_value(args, &mut index, inline, name)?));
                }
                "subject" => {
                    selection.subjects =
                        parse_subject_list(&flag_value(args, &mut index, inline, name)?)?;
                }
                "capability" => {
                    selection.capabilities =
                        parse_capability_list(&flag_value(args, &mut index, inline, name)?)?;
                }
                "room" => {
                    push_rooms(&mut selection, &flag_value(args, &mut index, inline, name)?)?;
                }
                "place" => {
                    selection.place =
                        PlaceFilter::parse(&flag_value(args, &mut index, inline, name)?)?;
                }
                "policy-version" => {
                    selection.policy_version = flag_value(args, &mut index, inline, name)?
                        .parse()
                        .map_err(|_| CliError::usage("--policy-version must be an integer"))?;
                }
                "format" => {
                    format = OutputFormat::parse(&flag_value(args, &mut index, inline, name)?)?;
                }
                other => {
                    return Err(CliError::usage(format!(
                        "unknown option for grants matrix: --{other}"
                    )));
                }
            }
        } else {
            positional.push(arg.clone());
        }
        index += 1;
    }

    if !positional.is_empty() {
        return Err(CliError::usage(
            "grants matrix takes options only; see acbctl grants --help",
        ));
    }

    let source = load_config_source(&config, config_dir.as_deref())?;
    let rows = matrix_rows(&source, &selection)?;
    match format {
        OutputFormat::Tsv => print_matrix_tsv(&rows),
        OutputFormat::Json => print_matrix_json(&source, &rows)?,
    }
    Ok(ExitCode::SUCCESS)
}

fn parse_subject_list(value: &str) -> Result<Vec<GrantSubject>, CliError> {
    split_list(value, "subject")?
        .iter()
        .map(|item| parse_subject(item))
        .collect()
}

fn parse_capability_list(value: &str) -> Result<Vec<GrantCapability>, CliError> {
    split_list(value, "capability")?
        .iter()
        .map(|item| parse_capability(item))
        .collect()
}

fn push_rooms(selection: &mut Selection, value: &str) -> Result<(), CliError> {
    let mut rooms = selection.rooms.take().unwrap_or_default();
    for item in split_list(value, "room")? {
        let room = RoomId::parse(&item)
            .map_err(|error| CliError::usage(format!("invalid room {item:?}: {error}")))?;
        rooms.push(room);
    }
    selection.rooms = Some(rooms);
    Ok(())
}

fn matrix_rooms(source: &dyn PolicySource, selection: &Selection) -> Result<Vec<RoomId>, CliError> {
    let all_rooms = source.rooms();
    match &selection.rooms {
        Some(requested) => {
            for room in requested {
                if !all_rooms.contains(room) {
                    return Err(CliError::new(
                        EXIT_RULE_NOT_FOUND,
                        format!("room not found in source: {room}"),
                    ));
                }
            }
            let mut rooms: Vec<RoomId> = requested.to_vec();
            rooms.sort();
            rooms.dedup();
            Ok(rooms)
        }
        None => Ok(all_rooms
            .into_iter()
            .filter(|room| selection.place.includes(room_scope(room)))
            .collect()),
    }
}

fn matrix_rows(source: &dyn PolicySource, selection: &Selection) -> Result<Vec<Cell>, CliError> {
    let rooms = matrix_rooms(source, selection)?;
    cells_for(source, &rooms, selection)
}

fn cells_for(
    source: &dyn PolicySource,
    rooms: &[RoomId],
    selection: &Selection,
) -> Result<Vec<Cell>, CliError> {
    let mut cells =
        Vec::with_capacity(rooms.len() * selection.subjects.len() * selection.capabilities.len());
    for room in rooms {
        for subject in &selection.subjects {
            for capability in &selection.capabilities {
                cells.push(evaluate_cell(
                    source,
                    *subject,
                    Place::Room(room_scope(room)),
                    room,
                    *capability,
                    selection.policy_version,
                )?);
            }
        }
    }
    Ok(cells)
}

fn evaluate_cell(
    source: &dyn PolicySource,
    subject: GrantSubject,
    place: Place,
    room: &RoomId,
    capability: GrantCapability,
    policy_version: i64,
) -> Result<Cell, CliError> {
    let target = AccessTarget::room(room);
    let decision = source.decide(subject, place, capability, &target, policy_version)?;
    Ok(Cell {
        source: source.label().to_string(),
        subject: subject.as_str().to_string(),
        place: place.as_str().to_string(),
        room: room.to_string(),
        capability: capability.as_str().to_string(),
        allow: decision.allow,
        basis: decision.basis,
        rule_id: decision.rule_id,
        policy_version: decision.policy_version,
        subscribed: source.subscriptions().contains(room),
    })
}

fn print_matrix_tsv(rows: &[Cell]) {
    println!(
        "source\tsubject\tplace\troom\tcapability\teffect\tbasis\trule_id\tpolicy_version\tsubscribed"
    );
    for row in rows {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            row.source,
            row.subject,
            row.place,
            row.room,
            row.capability,
            if row.allow { "allow" } else { "deny" },
            row.basis,
            row.rule_id,
            row.policy_version,
            row.subscribed
        );
    }
}

fn print_matrix_json(source: &dyn PolicySource, rows: &[Cell]) -> Result<(), CliError> {
    #[derive(Serialize)]
    struct MatrixExport<'a> {
        schema: u32,
        kind: &'static str,
        source: &'a str,
        source_kind: &'a str,
        policy_version: i64,
        rows: &'a [Cell],
    }
    let export = MatrixExport {
        schema: EXPORT_SCHEMA,
        kind: MATRIX_JSON_KIND,
        source: source.label(),
        source_kind: source.kind(),
        policy_version: rows.first().map_or(0, |row| row.policy_version),
        rows,
    };
    let text = serde_json::to_string_pretty(&export)
        .map_err(|error| CliError::config(format!("serialize matrix output: {error}")))?;
    println!("{text}");
    Ok(())
}

// ---------------------------------------------------------------------------
// diff

enum InputKind {
    Config,
    Matrix,
}

fn classify_input(path: &Path) -> Result<InputKind, CliError> {
    let extension = path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("toml") => Ok(InputKind::Config),
        Some("json" | "tsv") => Ok(InputKind::Matrix),
        _ => Err(CliError::usage(format!(
            "unsupported diff input {}; expected .toml config or .json/.tsv matrix export",
            path.display()
        ))),
    }
}

fn diff_command(args: &[String]) -> Result<ExitCode, CliError> {
    let mut config_dir_a = None;
    let mut config_dir_b = None;
    let mut selection = Selection::default();
    let mut format = OutputFormat::Tsv;
    let mut positional = Vec::new();

    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some((name, inline)) = long_flag(arg) {
            match name {
                "config-dir-a" => {
                    config_dir_a = Some(PathBuf::from(flag_value(args, &mut index, inline, name)?));
                }
                "config-dir-b" => {
                    config_dir_b = Some(PathBuf::from(flag_value(args, &mut index, inline, name)?));
                }
                "subject" => {
                    selection.subjects =
                        parse_subject_list(&flag_value(args, &mut index, inline, name)?)?;
                }
                "capability" => {
                    selection.capabilities =
                        parse_capability_list(&flag_value(args, &mut index, inline, name)?)?;
                }
                "room" => {
                    push_rooms(&mut selection, &flag_value(args, &mut index, inline, name)?)?;
                }
                "place" => {
                    selection.place =
                        PlaceFilter::parse(&flag_value(args, &mut index, inline, name)?)?;
                }
                "policy-version" => {
                    selection.policy_version = flag_value(args, &mut index, inline, name)?
                        .parse()
                        .map_err(|_| CliError::usage("--policy-version must be an integer"))?;
                }
                "format" => {
                    format = OutputFormat::parse(&flag_value(args, &mut index, inline, name)?)?;
                }
                other => {
                    return Err(CliError::usage(format!(
                        "unknown option for grants diff: --{other}"
                    )));
                }
            }
        } else {
            positional.push(arg.clone());
        }
        index += 1;
    }

    if positional.len() != 2 {
        return Err(CliError::usage(
            "usage: acbctl grants diff <a.toml|a.json|a.tsv> <b.toml|b.json|b.tsv>",
        ));
    }

    let before_path = PathBuf::from(&positional[0]);
    let after_path = PathBuf::from(&positional[1]);
    let before = load_cells(
        &before_path,
        classify_input(&before_path)?,
        config_dir_a.as_deref(),
        &selection,
    )?;
    let after = load_cells(
        &after_path,
        classify_input(&after_path)?,
        config_dir_b.as_deref(),
        &selection,
    )?;
    let (cells, changes) = compare_cells(&before, &after);
    match format {
        OutputFormat::Tsv => print_diff_tsv(cells, &changes),
        OutputFormat::Json => print_diff_json(cells, &changes)?,
    }
    Ok(if changes.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_DIFF_CHANGED)
    })
}

fn load_cells(
    path: &Path,
    kind: InputKind,
    config_dir: Option<&Path>,
    selection: &Selection,
) -> Result<BTreeMap<String, Cell>, CliError> {
    match kind {
        InputKind::Config => {
            let source = load_config_source(path, config_dir)?;
            let rooms = matrix_rooms(&source, selection)?;
            let cells = cells_for(&source, &rooms, selection)?;
            Ok(cells.into_iter().map(|cell| (cell.key(), cell)).collect())
        }
        InputKind::Matrix => {
            let cells = load_matrix_export(path)?;
            Ok(filter_cells(cells, selection)
                .into_iter()
                .map(|cell| (cell.key(), cell))
                .collect())
        }
    }
}

#[derive(Deserialize)]
struct MatrixDocument {
    kind: Option<String>,
    rows: Vec<Cell>,
}

fn load_matrix_export(path: &Path) -> Result<Vec<Cell>, CliError> {
    let text = fs::read_to_string(path)
        .map_err(|error| CliError::config(format!("read {}: {error}", path.display())))?;
    if path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("tsv"))
    {
        return parse_matrix_tsv(&text, path);
    }
    let document: MatrixDocument = serde_json::from_str(&text)
        .map_err(|error| CliError::config(format!("parse {}: {error}", path.display())))?;
    if document.kind.as_deref() != Some(MATRIX_JSON_KIND) {
        return Err(CliError::config(format!(
            "{} is not an acbctl grants matrix export (kind = {MATRIX_JSON_KIND})",
            path.display()
        )));
    }
    Ok(document.rows)
}

fn parse_matrix_tsv(text: &str, path: &Path) -> Result<Vec<Cell>, CliError> {
    let mut lines = text.lines().filter(|line| {
        let trimmed = line.trim();
        !trimmed.is_empty() && !trimmed.starts_with('#')
    });
    let Some(header) = lines.next() else {
        return Err(CliError::config(format!(
            "{} has no TSV header",
            path.display()
        )));
    };
    let expected = "source\tsubject\tplace\troom\tcapability\teffect\tbasis\trule_id\tpolicy_version\tsubscribed";
    if header.trim() != expected {
        return Err(CliError::config(format!(
            "{} is not an acbctl grants matrix TSV export",
            path.display()
        )));
    }
    let mut rows = Vec::new();
    for (index, line) in lines.enumerate() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 10 {
            return Err(CliError::config(format!(
                "{} line {}: expected 10 TSV fields, got {}",
                path.display(),
                index + 2,
                fields.len()
            )));
        }
        let allow = match fields[5] {
            "allow" => true,
            "deny" => false,
            other => {
                return Err(CliError::config(format!(
                    "{} line {}: effect must be allow/deny, got {other:?}",
                    path.display(),
                    index + 2
                )));
            }
        };
        let policy_version = fields[8].parse::<i64>().map_err(|_| {
            CliError::config(format!(
                "{} line {}: policy_version is not an integer",
                path.display(),
                index + 2
            ))
        })?;
        let subscribed = match fields[9] {
            "true" => true,
            "false" => false,
            other => {
                return Err(CliError::config(format!(
                    "{} line {}: subscribed must be true/false, got {other:?}",
                    path.display(),
                    index + 2
                )));
            }
        };
        rows.push(Cell {
            source: fields[0].to_string(),
            subject: fields[1].to_string(),
            place: fields[2].to_string(),
            room: fields[3].to_string(),
            capability: fields[4].to_string(),
            allow,
            basis: fields[6].to_string(),
            rule_id: fields[7].to_string(),
            policy_version,
            subscribed,
        });
    }
    Ok(rows)
}

fn filter_cells(cells: Vec<Cell>, selection: &Selection) -> Vec<Cell> {
    let subjects: BTreeSet<&str> = selection
        .subjects
        .iter()
        .map(|subject| subject.as_str())
        .collect();
    let capabilities: BTreeSet<&str> = selection
        .capabilities
        .iter()
        .map(|capability| capability.as_str())
        .collect();
    let rooms: Option<BTreeSet<String>> = selection
        .rooms
        .as_ref()
        .map(|rooms| rooms.iter().map(ToString::to_string).collect());
    cells
        .into_iter()
        .filter(|cell| subjects.contains(cell.subject.as_str()))
        .filter(|cell| capabilities.contains(cell.capability.as_str()))
        .filter(|cell| match cell.place.as_str() {
            "group" => selection.place.includes(RoomScope::MultiParty),
            "private" => selection.place.includes(RoomScope::Direct),
            _ => selection.place == PlaceFilter::All,
        })
        .filter(|cell| {
            rooms
                .as_ref()
                .is_none_or(|rooms| rooms.contains(&cell.room))
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
struct CellSummary {
    source: String,
    allow: bool,
    basis: String,
    rule_id: String,
}

#[derive(Debug, Clone, Serialize)]
struct Change {
    subject: String,
    place: String,
    room: String,
    capability: String,
    before: Option<CellSummary>,
    after: Option<CellSummary>,
}

fn compare_cells(
    before: &BTreeMap<String, Cell>,
    after: &BTreeMap<String, Cell>,
) -> (usize, Vec<Change>) {
    let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    let total = keys.len();
    let mut changes = Vec::new();
    for key in &keys {
        let before_cell = before.get(*key);
        let after_cell = after.get(*key);
        let changed = match (before_cell, after_cell) {
            (Some(left), Some(right)) => {
                left.allow != right.allow
                    || left.basis != right.basis
                    || left.rule_id != right.rule_id
            }
            _ => true,
        };
        if !changed {
            continue;
        }
        let sample = before_cell.or(after_cell).expect("key came from one side");
        changes.push(Change {
            subject: sample.subject.clone(),
            place: sample.place.clone(),
            room: sample.room.clone(),
            capability: sample.capability.clone(),
            before: before_cell.map(summary),
            after: after_cell.map(summary),
        });
    }
    (total, changes)
}

fn summary(cell: &Cell) -> CellSummary {
    CellSummary {
        source: cell.source.clone(),
        allow: cell.allow,
        basis: cell.basis.clone(),
        rule_id: cell.rule_id.clone(),
    }
}

fn print_diff_tsv(cells: usize, changes: &[Change]) {
    println!("# cells={cells} changed={}", changes.len());
    println!(
        "subject\tplace\troom\tcapability\tbefore_source\tbefore\tbefore_basis\tbefore_rule_id\tafter_source\tafter\tafter_basis\tafter_rule_id"
    );
    for change in changes {
        let (before_source, before_effect, before_basis, before_rule) = match &change.before {
            Some(cell) => (
                cell.source.as_str(),
                if cell.allow { "allow" } else { "deny" },
                cell.basis.as_str(),
                cell.rule_id.as_str(),
            ),
            None => ("<missing>", "<missing>", "<missing>", "<missing>"),
        };
        let (after_source, after_effect, after_basis, after_rule) = match &change.after {
            Some(cell) => (
                cell.source.as_str(),
                if cell.allow { "allow" } else { "deny" },
                cell.basis.as_str(),
                cell.rule_id.as_str(),
            ),
            None => ("<missing>", "<missing>", "<missing>", "<missing>"),
        };
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            change.subject,
            change.place,
            change.room,
            change.capability,
            before_source,
            before_effect,
            before_basis,
            before_rule,
            after_source,
            after_effect,
            after_basis,
            after_rule
        );
    }
}

fn print_diff_json(cells: usize, changes: &[Change]) -> Result<(), CliError> {
    #[derive(Serialize)]
    struct DiffSummary {
        cells: usize,
        changed: usize,
    }

    #[derive(Serialize)]
    struct DiffExport<'a> {
        schema: u32,
        kind: &'static str,
        summary: DiffSummary,
        changes: &'a [Change],
    }

    let export = DiffExport {
        schema: EXPORT_SCHEMA,
        kind: DIFF_JSON_KIND,
        summary: DiffSummary {
            cells,
            changed: changes.len(),
        },
        changes,
    };
    let text = serde_json::to_string_pretty(&export)
        .map_err(|error| CliError::config(format!("serialize diff output: {error}")))?;
    println!("{text}");
    Ok(())
}

// ---------------------------------------------------------------------------
// help

fn print_help() {
    println!(
        "Usage:
  acbctl grants explain <subject> --place <private|group|console|none|room:ID> \\
      --capability <cap> [--target SPEC] [--rule ID] \\
      [--config PATH] [--config-dir DIR] [--policy-version N] [--json]
  acbctl grants matrix [--config PATH] [--config-dir DIR] \\
      [--subject a,b,...] [--room ROOM]... [--capability a,b,...] \\
      [--place group|private|all] [--policy-version N] [--format tsv|json]
  acbctl grants diff <a.toml|a.json|a.tsv> <b.toml|b.json|b.tsv> \\
      [--subject a,b,...] [--room ROOM]... [--capability a,b,...] \\
      [--place group|private|all] [--policy-version N] [--format tsv|json]

Read-only: the current source is config (base TOML + config.d), evaluated with the
same core policy as the bridge. Output always carries a source label; it is a
static snapshot decision, not the live bridge decision. Static policy_version
defaults to 0. Future policy.db sources will add implementations, not commands.

Exit codes: 0 ok / diff unchanged, 2 usage, 3 unknown capability,
4 rule or room not found, 5 config parse failure, 6 diff changed."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config(extra_subscriptions: &str, extra_grants: &str) -> Config {
        let text = format!(
            r#"
[[connections]]
id = "t"
platform = "t"
kind = "onebot11"
url = "ws://127.0.0.1:3001"
account = "1"

[[personas]]
id = "p"
name = "P"
text = "x"

[[harnesses]]
id = "h"
name = "H"

[[sessions]]
id = "s"
harness = "h"
persona = "p"
token = {{ secret = "tok" }}

[[sessions.subscriptions]]
connection = "t"
room = "group/1"
{extra_subscriptions}
{extra_grants}
"#
        );
        Config::parse_fixture(&text).expect("测试配置应能解析")
    }

    fn source_with(grants: GrantsConfig) -> ConfigSource {
        let mut config = sample_config("", "");
        config.grants = grants;
        ConfigSource::new("config:test+test.d".to_string(), config)
    }

    fn room(text: &str) -> RoomId {
        RoomId::parse(text).expect("测试房间")
    }

    fn plugin_rule(id: &str) -> GrantRule {
        GrantRule {
            id: id.to_string(),
            subject: GrantSubject::SystemAdmin,
            place: Place::Room(RoomScope::MultiParty),
            scope: acb_kernel::grants::GrantScope::Room,
            target: Some(AccessTarget::room(&room("t:group/1"))),
            capability: vec![GrantCapability::Plugin],
        }
    }

    fn deny_rule(id: &str) -> GrantRule {
        GrantRule {
            id: id.to_string(),
            subject: GrantSubject::User,
            place: Place::Room(RoomScope::MultiParty),
            scope: acb_kernel::grants::GrantScope::Room,
            target: Some(AccessTarget::room(&room("t:group/1"))),
            capability: vec![GrantCapability::RoomRead],
        }
    }

    #[test]
    fn explain_allow_from_base_subscription() {
        let source = source_with(GrantsConfig::default());
        let cell = evaluate_cell(
            &source,
            GrantSubject::User,
            Place::Room(RoomScope::MultiParty),
            &room("t:group/1"),
            GrantCapability::RoomRead,
            9,
        )
        .expect("求值");
        assert!(cell.allow);
        assert_eq!(cell.basis, "base:subscription");
        assert_eq!(cell.policy_version, 9);
        assert!(cell.subscribed);
        assert_eq!(cell.source, "config:test+test.d");
    }

    #[test]
    fn explain_allow_from_rule() {
        let mut grants = GrantsConfig::default();
        grants.rules.push(plugin_rule("group-1-plugin"));
        let source = source_with(grants);
        let cell = evaluate_cell(
            &source,
            GrantSubject::SystemAdmin,
            Place::Room(RoomScope::MultiParty),
            &room("t:group/1"),
            GrantCapability::Plugin,
            9,
        )
        .expect("求值");
        assert!(cell.allow);
        assert_eq!(cell.basis, "rule:group-1-plugin");
        assert_eq!(cell.rule_id, "rule:group-1-plugin");
    }

    #[test]
    fn explain_deny_from_explicit_deny() {
        let mut grants = GrantsConfig::default();
        grants.deny.push(deny_rule("deny-room-read"));
        let source = source_with(grants);
        let cell = evaluate_cell(
            &source,
            GrantSubject::User,
            Place::Room(RoomScope::MultiParty),
            &room("t:group/1"),
            GrantCapability::RoomRead,
            9,
        )
        .expect("求值");
        assert!(!cell.allow);
        assert!(cell.basis.contains("deny:deny-room-read"));
        assert_eq!(cell.rule_id, "deny:deny-room-read");
    }

    #[test]
    fn unknown_capability_has_its_own_exit_code() {
        let error = parse_capability("not_a_capability").expect_err("未知能力");
        assert_eq!(error.code, EXIT_UNKNOWN_CAPABILITY);
    }

    #[test]
    fn matrix_covers_all_group_rooms() {
        let extra_subscriptions = r#"
[[sessions.subscriptions]]
connection = "t"
room = "group/2"

[[sessions.subscriptions]]
connection = "t"
room = "private/9"
"#;
        let source = ConfigSource::new(
            "config:test+test.d".to_string(),
            sample_config(extra_subscriptions, ""),
        );
        let selection = Selection {
            subjects: vec![GrantSubject::User],
            capabilities: vec![GrantCapability::RoomRead],
            ..Selection::default()
        };
        let rows = matrix_rows(&source, &selection).expect("矩阵");
        let rooms: BTreeSet<&str> = rows.iter().map(|cell| cell.room.as_str()).collect();
        assert_eq!(rooms.len(), 2);
        assert!(rooms.contains("t:group/1"));
        assert!(rooms.contains("t:group/2"));
        assert!(!rooms.iter().any(|room| room.starts_with("t:private/")));
        assert!(rows.iter().all(|cell| cell.place == "group"));
        assert!(rows.iter().all(|cell| cell.source == "config:test+test.d"));
    }

    #[test]
    fn diff_without_changes() {
        let source = source_with(GrantsConfig::default());
        let selection = Selection::default();
        let cells = cells_for(
            &source,
            &matrix_rooms(&source, &selection).expect("房间"),
            &selection,
        )
        .expect("矩阵");
        let before: BTreeMap<String, Cell> = cells
            .iter()
            .cloned()
            .map(|cell| (cell.key(), cell))
            .collect();
        let after = before.clone();
        let (count, changes) = compare_cells(&before, &after);
        assert!(count > 0);
        assert!(changes.is_empty());
    }

    #[test]
    fn diff_with_changes() {
        let before = source_with(GrantsConfig::default());
        let mut grants = GrantsConfig::default();
        grants.rules.push(plugin_rule("group-1-plugin"));
        let after = source_with(grants);
        let selection = Selection::default();
        let before_cells = cells_for(
            &before,
            &matrix_rooms(&before, &selection).expect("房间"),
            &selection,
        )
        .expect("矩阵");
        let after_cells = cells_for(
            &after,
            &matrix_rooms(&after, &selection).expect("房间"),
            &selection,
        )
        .expect("矩阵");
        let before: BTreeMap<String, Cell> = before_cells
            .into_iter()
            .map(|cell| (cell.key(), cell))
            .collect();
        let after: BTreeMap<String, Cell> = after_cells
            .into_iter()
            .map(|cell| (cell.key(), cell))
            .collect();
        let (count, changes) = compare_cells(&before, &after);
        assert!(count > 0);
        assert!(!changes.is_empty());
        assert!(changes.iter().any(|change| {
            change
                .after
                .as_ref()
                .is_some_and(|cell| cell.basis == "rule:group-1-plugin")
        }));
    }
}
