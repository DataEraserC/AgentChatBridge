//! 模型失败惩罚：窗口内同一 class 的失败累计到阈值，就把该模型从下发链里摘掉一段时间。
//!
//! 为什么不复用 [`crate::deps`]：那是依赖面板的状态机，阈值与窗口来自 `[deps]`
//! 配置，语义是「这条依赖现在是不是红的」。惩罚要回答的是另一个问题——「这一轮该
//! 把哪个模型从链里跳过」。两者共用计数会让面板的一次抖动变成整条链被跳过
//! （2026-11-09 那个「一次抖动整条依赖标红」的老问题）。所以这里独立计数、独立阈值。
//!
//! 状态分两半：
//! - **策略**（[`FallbackPolicy`]）落库，全局默认 + 每会话覆盖，见 `storage`
//!   的 `ModelFallbackPolicy`；（本模块只定义形状与校验。）
//! - **惩罚**（[`ModelPenalty`]）只在内存，重启即清空。惩罚是「此刻别用它」的短期
//!   判断，不是业务对象，没必要跨重启。
//!
//! 惩罚记录是**全局 + 会话**两层：失败发生在哪个会话、按哪份策略生效，就记到哪一层
//! （会话有自己的策略 → 记到该会话；否则记到全局）。查询某个会话时两层都看：全局
//! 惩罚说明这个模型在这台桥上就是坏的，不该因为某个会话自己有策略就当没看见。
//!
//! 到期即回归，没有清扫线程：判定只看 `until > now`，过期的条目在下一次读写同一
//! 模型时顺手丢掉。

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

/// 失败阈值上限。再高就等于「永远不罚」，那还不如把 `enabled` 关掉。
pub const MAX_FAILURE_THRESHOLD: u64 = 1000;
/// 统计窗口上限（7 天）。防止有人把窗口配成天文数字后内存里攒一堆失败记录。
pub const MAX_FAILURE_WINDOW_SECS: u64 = 7 * 24 * 60 * 60;
/// 惩罚时长上限（7 天）。惩罚本该是短期的；要长期停用某个模型应该直接从链里删掉。
pub const MAX_PENALTY_SECS: u64 = 7 * 24 * 60 * 60;

/// 模型失败惩罚策略。全局一份，每个会话可以覆盖一份。
///
/// `enabled = false`、或三个数值里有 0 时，这条策略不生效（等价于不惩罚）：
/// 0 是「这一维度关闭」，不是「立刻惩罚」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FallbackPolicy {
    /// 总开关。关掉时其余字段只作为草稿保留。
    pub enabled: bool,
    /// 窗口内同一 class 失败几次才罚。0 = 不罚。
    pub failure_threshold: u64,
    /// 统计窗口（秒）。0 = 关闭。
    pub failure_window_secs: u64,
    /// 惩罚时长（秒）。0 = 关闭。
    pub penalty_secs: u64,
}

impl FallbackPolicy {
    /// 从控制台/HTTP 那种「可能是负数」的输入构造。负数是非法值，这里拒绝，
    /// 一个字节都不往库里写（同 `replace_model_fallbacks` 的「先判再写」）。
    pub fn from_parts(
        enabled: bool,
        failure_threshold: i64,
        failure_window_secs: i64,
        penalty_secs: i64,
    ) -> Result<Self, String> {
        if failure_threshold < 0 {
            return Err(format!("失败阈值不能是负数：{failure_threshold}"));
        }
        if failure_window_secs < 0 {
            return Err(format!("统计窗口不能是负数：{failure_window_secs} 秒"));
        }
        if penalty_secs < 0 {
            return Err(format!("惩罚时长不能是负数：{penalty_secs} 秒"));
        }
        let policy = Self {
            enabled,
            failure_threshold: failure_threshold as u64,
            failure_window_secs: failure_window_secs as u64,
            penalty_secs: penalty_secs as u64,
        };
        policy.validate()?;
        Ok(policy)
    }

    /// 全量校验。写库前调用；不合法时返回能直接给人看的原因。
    pub fn validate(&self) -> Result<(), String> {
        if self.failure_threshold > MAX_FAILURE_THRESHOLD {
            return Err(format!(
                "失败阈值太大：{}，最多 {MAX_FAILURE_THRESHOLD}",
                self.failure_threshold
            ));
        }
        if self.failure_window_secs > MAX_FAILURE_WINDOW_SECS {
            return Err(format!(
                "统计窗口太大：{} 秒，最多 {MAX_FAILURE_WINDOW_SECS} 秒",
                self.failure_window_secs
            ));
        }
        if self.penalty_secs > MAX_PENALTY_SECS {
            return Err(format!(
                "惩罚时长太大：{} 秒，最多 {MAX_PENALTY_SECS} 秒",
                self.penalty_secs
            ));
        }
        // 开了惩罚却把窗口/时长设成 0：配置自相矛盾，会静默地永远不生效——
        // 与其让人事后查「为什么没罚」，不如写库前就拒绝。
        if self.enabled && self.failure_threshold > 0 {
            if self.failure_window_secs == 0 {
                return Err(
                    "启用了失败惩罚，但统计窗口是 0 秒：窗口为 0 永远累计不到阈值，\
                     请把 failure_window_secs 设成大于 0，或把 enabled 关掉"
                        .to_owned(),
                );
            }
            if self.penalty_secs == 0 {
                return Err("启用了失败惩罚，但惩罚时长是 0 秒：罚 0 秒等于不罚，\
                     请把 penalty_secs 设成大于 0，或把 enabled 关掉"
                    .to_owned());
            }
        }
        Ok(())
    }

    /// 这条策略会不会真的产生惩罚。任一维度为 0 或总开关关掉 = 不产生。
    pub fn is_active(&self) -> bool {
        self.enabled
            && self.failure_threshold > 0
            && self.failure_window_secs > 0
            && self.penalty_secs > 0
    }
}

/// 一次查询的结论：实际能下发的链 + 被跳过的模型。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FallbackPlan {
    /// 实际下发给驱动的链（顺序与库里的链一致，只少了被跳过的项）。
    pub models: Vec<String>,
    /// 被跳过的模型，按链里的顺序；每项带 class 与剩余秒数。
    pub skipped: Vec<SkippedModel>,
}

/// 因为惩罚期没结束而被跳过的模型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedModel {
    pub model: String,
    /// 触发这次惩罚的失败分类（harness 上报的 class，原样保留）。
    pub class: String,
    /// 还剩多少秒出惩罚期；向上取整，最小 1。
    pub remaining_secs: u64,
}

/// 惩罚记在哪一层。库里是「全局 / 每会话」，这里跟着分。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PenaltyKey {
    /// `None` = 全局层；`Some(id)` = 该会话自己的层。
    session: Option<String>,
    model: String,
}

#[derive(Debug, Clone)]
struct FailureTick {
    at: i64,
    class: String,
}

#[derive(Debug, Clone)]
struct Penalty {
    until: i64,
    class: String,
}

#[derive(Debug, Default)]
struct Inner {
    /// 每个 (层, 模型) 窗口内的失败流水。
    failures: BTreeMap<PenaltyKey, VecDeque<FailureTick>>,
    /// 生效中的惩罚。过期的条目可以留着，判定只看 `until`。
    penalties: BTreeMap<PenaltyKey, Penalty>,
}

/// 内存态的惩罚表。克隆共享同一份状态，读写都在自带的小锁里（不占 `Bridge` 的大锁）。
#[derive(Debug, Clone, Default)]
pub struct ModelPenalty {
    inner: std::sync::Arc<Mutex<Inner>>,
}

impl ModelPenalty {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 记一次失败（带时间戳与 class）。同一 class 在窗口内累计到阈值就进入惩罚期。
    ///
    /// `scope = None` 记到全局层，`Some(session)` 记到该会话自己的层——调用方按
    /// 「这个会话有没有自己的策略」决定，见 `engine::note_model_failure`。
    pub fn record(
        &self,
        scope: Option<&str>,
        model: &str,
        class: &str,
        policy: &FallbackPolicy,
        now: i64,
    ) {
        // 策略没生效时连流水都不记：关掉惩罚后不该悄悄攒一堆数据等着哪天打开。
        if !policy.is_active() || class.trim().is_empty() {
            return;
        }
        let key = PenaltyKey {
            session: scope.map(ToOwned::to_owned),
            model: model.to_owned(),
        };
        let window_ms = (policy.failure_window_secs as i64).saturating_mul(1000);
        let penalty_ms = (policy.penalty_secs as i64).saturating_mul(1000);
        let mut inner = self.lock();
        let ticks = inner.failures.entry(key.clone()).or_default();
        while let Some(front) = ticks.front() {
            if now.saturating_sub(front.at) > window_ms {
                ticks.pop_front();
            } else {
                break;
            }
        }
        ticks.push_back(FailureTick {
            at: now,
            class: class.to_owned(),
        });
        let count = ticks.iter().filter(|tick| tick.class == class).count() as u64;
        if count < policy.failure_threshold {
            return;
        }
        // 再失败一次就把惩罚往后顺延：还在报错的模型不该在惩罚中途被放回来。
        inner.penalties.insert(
            key,
            Penalty {
                until: now.saturating_add(penalty_ms),
                class: class.to_owned(),
            },
        );
    }

    /// 这个模型此刻是不是还在惩罚期。返回 (class, 剩余秒数)。
    ///
    /// 全局层的惩罚对所有会话生效；`scope = Some(id)` 时再并上该会话自己的层。
    pub fn check(&self, scope: Option<&str>, model: &str, now: i64) -> Option<(String, u64)> {
        let mut inner = self.lock();
        let mut keys = vec![PenaltyKey {
            session: None,
            model: model.to_owned(),
        }];
        if let Some(session) = scope {
            keys.push(PenaltyKey {
                session: Some(session.to_owned()),
                model: model.to_owned(),
            });
        }
        let mut found: Option<(String, u64)> = None;
        for key in keys {
            let Some(penalty) = inner.penalties.get(&key) else {
                continue;
            };
            if penalty.until <= now {
                // 到期即回归：顺手把它删掉，不需要外部清扫。
                inner.penalties.remove(&key);
                continue;
            }
            let remaining = ((penalty.until - now) as u64).div_ceil(1000).max(1);
            let replace = match &found {
                Some((_, current)) => remaining > *current,
                None => true,
            };
            if replace {
                found = Some((penalty.class.clone(), remaining));
            }
        }
        found
    }

    /// 在给定链上过滤掉仍在惩罚期的模型，保持链的原有顺序。
    ///
    /// 只过滤，不重排、不去重、不补项：链的顺序与内容由库决定（会话级链覆盖全局），
    /// 惩罚只负责「这几项此刻别用」。
    pub fn plan(&self, scope: Option<&str>, chain: &[String], now: i64) -> FallbackPlan {
        let mut plan = FallbackPlan::default();
        for model in chain {
            match self.check(scope, model, now) {
                Some((class, remaining_secs)) => plan.skipped.push(SkippedModel {
                    model: model.clone(),
                    class,
                    remaining_secs,
                }),
                None => plan.models.push(model.clone()),
            }
        }
        plan
    }

    /// 现在还有多少条惩罚在生效（控制台/测试用；顺便清掉过期的）。
    pub fn active_penalties(&self, now: i64) -> Vec<(Option<String>, String, String, u64)> {
        let mut inner = self.lock();
        let entries: Vec<(PenaltyKey, Penalty)> = inner
            .penalties
            .iter()
            .map(|(key, penalty)| (key.clone(), penalty.clone()))
            .collect();
        let mut out = Vec::new();
        for (key, penalty) in entries {
            if penalty.until <= now {
                inner.penalties.remove(&key);
                continue;
            }
            let remaining = ((penalty.until - now) as u64).div_ceil(1000).max(1);
            out.push((key.session, key.model, penalty.class, remaining));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(threshold: u64, window_secs: u64, penalty_secs: u64) -> FallbackPolicy {
        FallbackPolicy {
            enabled: true,
            failure_threshold: threshold,
            failure_window_secs: window_secs,
            penalty_secs: penalty_secs,
        }
    }

    fn chain() -> Vec<String> {
        vec!["trae/b".to_owned(), "trae/c".to_owned()]
    }

    #[test]
    fn threshold_is_counted_within_the_window_only() {
        let penalty = ModelPenalty::new();
        let policy = policy(2, 60, 300);
        penalty.record(Some("main"), "trae/b", "rate_limited", &policy, 0);
        assert!(
            penalty.check(Some("main"), "trae/b", 1_000).is_none(),
            "一次失败不够阈值"
        );
        // 第二次在窗口内：进入惩罚期。
        penalty.record(Some("main"), "trae/b", "rate_limited", &policy, 10_000);
        let (class, remaining) = penalty
            .check(Some("main"), "trae/b", 10_000)
            .expect("到阈值就该罚");
        assert_eq!(class, "rate_limited");
        assert_eq!(remaining, 300, "剩余秒数 = 惩罚时长");
    }

    #[test]
    fn different_classes_do_not_add_up() {
        let penalty = ModelPenalty::new();
        let policy = policy(2, 60, 300);
        penalty.record(Some("main"), "trae/b", "rate_limited", &policy, 0);
        penalty.record(Some("main"), "trae/b", "content_blocked", &policy, 1_000);
        assert!(
            penalty.check(Some("main"), "trae/b", 2_000).is_none(),
            "不同 class 各算各的，不能凑成 2 次"
        );
    }

    #[test]
    fn expired_penalty_returns_without_any_sweep() {
        let penalty = ModelPenalty::new();
        let policy = policy(1, 60, 300);
        penalty.record(None, "trae/b", "overloaded", &policy, 0);
        let plan = penalty.plan(None, &chain(), 0);
        assert_eq!(plan.models, vec!["trae/c".to_owned()], "惩罚期内该模型消失");
        assert_eq!(plan.skipped.len(), 1);

        let plan = penalty.plan(None, &chain(), 300_000);
        assert_eq!(plan.models, chain(), "到期那一刻自动回归，不需要外部清扫");
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn negative_policy_values_are_rejected() {
        let error = FallbackPolicy::from_parts(true, -1, 60, 300).expect_err("负数阈值要拒绝");
        assert!(error.contains("负数"), "要说清是负数，实际：{error}");
        let error = FallbackPolicy::from_parts(true, 3, -1, 300).expect_err("负数窗口要拒绝");
        assert!(error.contains("负数"), "实际：{error}");
        let error = FallbackPolicy::from_parts(true, 3, 60, -1).expect_err("负数时长要拒绝");
        assert!(error.contains("负数"), "实际：{error}");
        // 0 是合法的「关闭」，不能把 0 当负数拒绝。
        FallbackPolicy::from_parts(false, 0, 0, 0).expect("全 0 = 关闭，是合法配置");
    }

    #[test]
    fn contradictory_or_oversized_policies_are_rejected() {
        let error = FallbackPolicy::from_parts(true, 3, 0, 300).expect_err("窗口 0 要拒绝");
        assert!(error.contains("统计窗口"), "实际：{error}");
        let error = FallbackPolicy::from_parts(true, 3, 60, 0).expect_err("时长 0 要拒绝");
        assert!(error.contains("惩罚时长"), "实际：{error}");
        let error = FallbackPolicy::from_parts(true, (MAX_FAILURE_THRESHOLD + 1) as i64, 60, 300)
            .expect_err("阈值过大要拒绝");
        assert!(error.contains("阈值太大"), "实际：{error}");
        let error = FallbackPolicy::from_parts(true, 3, (MAX_FAILURE_WINDOW_SECS + 1) as i64, 300)
            .expect_err("窗口过大要拒绝");
        assert!(error.contains("窗口太大"), "实际：{error}");
        let error = FallbackPolicy::from_parts(true, 3, 60, (MAX_PENALTY_SECS + 1) as i64)
            .expect_err("时长过大要拒绝");
        assert!(error.contains("惩罚时长太大"), "实际：{error}");
        // 关掉总开关后，同样的数值只是草稿，不该拒绝。
        FallbackPolicy::from_parts(false, 3, 0, 0).expect("关掉时不做交叉校验");
    }
}
