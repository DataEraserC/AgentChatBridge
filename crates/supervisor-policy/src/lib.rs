//! Supervisor 内置策略的纯计算层。
//!
//! 输入是一份冻结的状态快照与策略配置，输出 `fail / hold / rollback`。
//! 本模块不做 I/O、不读时钟、不取随机数；时间全部来自快照，保证同一输入
//! 永远得到同一输出。进程监管、策略错误计数、LKG 读取与动作执行都在调用方。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// 内置策略默认参数。配置缺省时整套使用这些值。
pub const DEFAULT_STARTUP_GRACE_SECS: u64 = 30;
pub const DEFAULT_CONSECUTIVE_FAILURES: u32 = 3;
pub const DEFAULT_WINDOW_FAILURES: u32 = 5;
pub const DEFAULT_WINDOW_SECS: u64 = 600;
pub const DEFAULT_UNHEALTHY_SECS: u64 = 60;
pub const DEFAULT_UNHEALTHY_PROBES: u32 = 3;
pub const DEFAULT_RESTART_BACKOFF_SECS: [u64; 5] = [1, 2, 4, 8, 30];

/// 内置策略配置。字段与 `docs/plan/supervisor-policy-interface.md` 的默认表一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    /// 启动宽限；0 = 无宽限。
    pub startup_grace_secs: u64,
    /// 连续失败阈值；0 = 关闭这条规则。
    pub consecutive_failures: u32,
    /// 窗口累计失败阈值；0 = 关闭这条规则。
    pub window_failures: u32,
    /// 失败窗口长度（秒）；0 = 关闭窗口规则。
    pub window_secs: u64,
    /// 一次即动（回滚/无 LKG 时 fail）的错误类别。
    pub panic_classes: Vec<String>,
    /// panic 类是否立即触发动作；false 时 panic 也走普通累计。
    pub panic_rollback: bool,
    /// provider 解析失败是否计入窗口累计；false 时只走连续失败规则。
    pub provider_parse_uses_window: bool,
    /// 持续不健康时间阈值（秒）；0 = 关闭时间规则。
    pub unhealthy_secs: u64,
    /// 连续不健康探测次数阈值；0 = 关闭次数规则。
    pub unhealthy_probes: u32,
    /// `hold` 重启当前版本的退避阶梯（秒）。空表使用默认阶梯。
    pub restart_backoff_secs: Vec<u64>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            startup_grace_secs: DEFAULT_STARTUP_GRACE_SECS,
            consecutive_failures: DEFAULT_CONSECUTIVE_FAILURES,
            window_failures: DEFAULT_WINDOW_FAILURES,
            window_secs: DEFAULT_WINDOW_SECS,
            panic_classes: vec!["panic".to_owned()],
            panic_rollback: true,
            provider_parse_uses_window: true,
            unhealthy_secs: DEFAULT_UNHEALTHY_SECS,
            unhealthy_probes: DEFAULT_UNHEALTHY_PROBES,
            restart_backoff_secs: DEFAULT_RESTART_BACKOFF_SECS.to_vec(),
        }
    }
}

/// 一次失败记录。用于窗口累计与最近失败类别判断。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureRecord {
    pub at_ms: i64,
    pub class: String,
}

/// 版本标识。决策只用「是否存在 LKG」，内容留给审计。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub id: String,
}

/// 策略输入：一份冻结的状态快照。
///
/// 缺失字段的语义：`None` = 未知/不适用；`0` 次数 = 没有失败；空 `failures`
/// = 没有失败记录。调用方负责在构建快照时保留这个区别。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Snapshot {
    /// supervisor 观测时间（Unix 毫秒）；策略不得另取时间。
    pub observed_at_ms: i64,
    /// 进程启动时间；用于在 `grace_elapsed` 缺失时计算宽限。
    pub startup_at_ms: Option<i64>,
    /// 调用方已判定宽限是否结束；`Some` 优先于按时间计算。
    pub grace_elapsed: Option<bool>,
    /// 连续失败次数，由调用方维护。
    pub consecutive_failures: u32,
    /// 最近的失败记录（按时间升序；窗口规则按 `observed_at_ms` 过滤）。
    pub failures: Vec<FailureRecord>,
    /// 连续不健康探测次数。
    pub consecutive_unhealthy_probes: u32,
    /// 连续不健康从何时开始（Unix 毫秒）。
    pub unhealthy_since_ms: Option<i64>,
    /// LKG 版本；`None` = 没有可回滚版本。
    pub lkg: Option<VersionInfo>,
    /// 连续 `hold` 次数，由调用方维护；用于退避阶梯。
    pub consecutive_holds: u32,
}

/// 三种决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Fail,
    Hold,
    Rollback,
}

impl DecisionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fail => "fail",
            Self::Hold => "hold",
            Self::Rollback => "rollback",
        }
    }
}

/// 策略输出。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub decision: DecisionKind,
    /// 机器可判定、稳定、追加式的代码。
    pub reason_code: String,
    /// 给人读的一句话（技术英文；调用方可按 code 本地化）。
    pub reason: String,
    /// `hold` 的再评估/重启延迟；`fail`/`rollback` 为 `None`。
    pub delay_ms: Option<u64>,
}

pub const REASON_STARTUP_GRACE: &str = "startup_grace";
pub const REASON_PANIC: &str = "panic";
pub const REASON_HEALTH_UNHEALTHY: &str = "health_unhealthy";
pub const REASON_CONSECUTIVE_FAILURES: &str = "consecutive_failures";
pub const REASON_WINDOW_FAILURES: &str = "window_failures";
pub const REASON_NO_RULE: &str = "no_rule_matched";

const PROVIDER_PARSE_CLASS: &str = "provider_parse";

/// 内置策略入口：纯函数。
///
/// 判定优先级：panic 类 → 启动宽限 → 健康不健康 → 连续失败 → 窗口累计 →
/// 默认 `hold`。`rollback` 在没有 LKG 时降级为 `fail`。
pub fn decide(config: &PolicyConfig, snapshot: &Snapshot) -> Decision {
    if config.panic_rollback {
        if let Some(class) = last_failure_class(snapshot) {
            if config.panic_classes.iter().any(|item| item == class) {
                return action(
                    snapshot,
                    REASON_PANIC,
                    format!("panic-class failure: {class}"),
                );
            }
        }
    }

    if !grace_elapsed(config, snapshot) {
        return hold(
            config,
            snapshot,
            REASON_STARTUP_GRACE,
            "startup grace has not elapsed".to_owned(),
        );
    }

    if health_unhealthy(config, snapshot) {
        return action(
            snapshot,
            REASON_HEALTH_UNHEALTHY,
            "health probes have been unhealthy past the threshold".to_owned(),
        );
    }

    if config.consecutive_failures > 0
        && snapshot.consecutive_failures >= config.consecutive_failures
    {
        return action(
            snapshot,
            REASON_CONSECUTIVE_FAILURES,
            format!(
                "consecutive failures reached {}",
                config.consecutive_failures
            ),
        );
    }

    let window_count = window_failure_count(config, snapshot);
    if config.window_failures > 0 && window_count >= config.window_failures as usize {
        return action(
            snapshot,
            REASON_WINDOW_FAILURES,
            format!(
                "failures in the last {}s reached {}",
                config.window_secs, config.window_failures
            ),
        );
    }

    hold(
        config,
        snapshot,
        REASON_NO_RULE,
        "no rule matched; keep current version".to_owned(),
    )
}

fn last_failure_class(snapshot: &Snapshot) -> Option<&str> {
    snapshot.failures.last().map(|record| record.class.as_str())
}

fn grace_elapsed(config: &PolicyConfig, snapshot: &Snapshot) -> bool {
    if let Some(elapsed) = snapshot.grace_elapsed {
        return elapsed;
    }
    if config.startup_grace_secs == 0 {
        return true;
    }
    let Some(start) = snapshot.startup_at_ms else {
        // 不知道启动时间时保守地按「宽限未过」处理，避免过早回滚。
        return false;
    };
    snapshot.observed_at_ms.saturating_sub(start)
        >= (config.startup_grace_secs as i64).saturating_mul(1000)
}

fn health_unhealthy(config: &PolicyConfig, snapshot: &Snapshot) -> bool {
    let by_time = config.unhealthy_secs > 0
        && snapshot.unhealthy_since_ms.is_some_and(|since| {
            snapshot.observed_at_ms.saturating_sub(since)
                >= (config.unhealthy_secs as i64).saturating_mul(1000)
        });
    let by_probes = config.unhealthy_probes > 0
        && snapshot.consecutive_unhealthy_probes >= config.unhealthy_probes;
    by_time || by_probes
}

fn window_failure_count(config: &PolicyConfig, snapshot: &Snapshot) -> usize {
    if config.window_secs == 0 {
        return 0;
    }
    let cutoff = snapshot
        .observed_at_ms
        .saturating_sub((config.window_secs as i64).saturating_mul(1000));
    snapshot
        .failures
        .iter()
        .filter(|record| {
            record.at_ms >= cutoff
                && record.at_ms <= snapshot.observed_at_ms
                && (config.provider_parse_uses_window || record.class != PROVIDER_PARSE_CLASS)
        })
        .count()
}

fn action(snapshot: &Snapshot, code: &str, reason: String) -> Decision {
    let (decision, reason) = match &snapshot.lkg {
        Some(_) => (DecisionKind::Rollback, reason),
        None => (
            DecisionKind::Fail,
            format!("{reason}; no LKG available, fail instead"),
        ),
    };
    Decision {
        decision,
        reason_code: code.to_owned(),
        reason,
        delay_ms: None,
    }
}

fn hold(config: &PolicyConfig, snapshot: &Snapshot, code: &str, reason: String) -> Decision {
    Decision {
        decision: DecisionKind::Hold,
        reason_code: code.to_owned(),
        reason,
        delay_ms: Some(backoff_ms(config, snapshot.consecutive_holds)),
    }
}

fn backoff_ms(config: &PolicyConfig, consecutive_holds: u32) -> u64 {
    let ladder: &[u64] = if config.restart_backoff_secs.is_empty() {
        &DEFAULT_RESTART_BACKOFF_SECS
    } else {
        &config.restart_backoff_secs
    };
    let index = (consecutive_holds as usize).min(ladder.len().saturating_sub(1));
    ladder[index].saturating_mul(1000)
}
