use acb_supervisor_policy::{
    DEFAULT_CONSECUTIVE_FAILURES, DEFAULT_RESTART_BACKOFF_SECS, DEFAULT_STARTUP_GRACE_SECS,
    DEFAULT_UNHEALTHY_PROBES, DEFAULT_UNHEALTHY_SECS, DEFAULT_WINDOW_FAILURES, DEFAULT_WINDOW_SECS,
    DecisionKind, FailureRecord, PolicyConfig, REASON_CONSECUTIVE_FAILURES,
    REASON_HEALTH_UNHEALTHY, REASON_NO_RULE, REASON_PANIC, REASON_STARTUP_GRACE,
    REASON_WINDOW_FAILURES, Snapshot, VersionInfo, decide,
};

fn snapshot() -> Snapshot {
    Snapshot {
        observed_at_ms: 10_000,
        startup_at_ms: Some(9_000),
        grace_elapsed: Some(true),
        consecutive_failures: 0,
        failures: Vec::new(),
        consecutive_unhealthy_probes: 0,
        unhealthy_since_ms: None,
        lkg: Some(VersionInfo {
            id: "lkg-1".to_owned(),
        }),
        consecutive_holds: 0,
    }
}

fn failure(at_ms: i64, class: &str) -> FailureRecord {
    FailureRecord {
        at_ms,
        class: class.to_owned(),
    }
}

struct Case {
    name: &'static str,
    config: PolicyConfig,
    snapshot: Snapshot,
    decision: DecisionKind,
    reason_code: &'static str,
    delay_ms: Option<u64>,
}

#[test]
fn decision_table_covers_documented_rules() {
    let window_config = PolicyConfig {
        window_failures: 3,
        window_secs: 600,
        ..PolicyConfig::default()
    };

    let mut grace_failure = snapshot();
    grace_failure.grace_elapsed = Some(false);
    grace_failure.consecutive_failures = 3;

    let mut panic_in_grace = snapshot();
    panic_in_grace.grace_elapsed = Some(false);
    panic_in_grace.failures = vec![failure(9_500, "panic")];
    panic_in_grace.consecutive_failures = 1;

    let mut panic_no_lkg = panic_in_grace.clone();
    panic_no_lkg.lkg = None;

    let mut consecutive_no_lkg = snapshot();
    consecutive_no_lkg.consecutive_failures = 3;
    consecutive_no_lkg.lkg = None;

    let mut consecutive_rollback = snapshot();
    consecutive_rollback.consecutive_failures = 3;

    let mut window_trigger = snapshot();
    window_trigger.observed_at_ms = 1_000_000;
    window_trigger.consecutive_failures = 1;
    window_trigger.failures = vec![
        failure(999_000, "internal"),
        failure(999_500, "internal"),
        failure(999_900, "internal"),
    ];

    let mut window_expired = snapshot();
    window_expired.observed_at_ms = 1_000_000;
    window_expired.failures = vec![
        failure(1_000, "internal"),
        failure(2_000, "internal"),
        failure(3_000, "internal"),
    ];

    let mut health_by_time = snapshot();
    health_by_time.unhealthy_since_ms = Some(9_000);

    let mut health_by_probes = snapshot();
    health_by_probes.consecutive_unhealthy_probes = 3;

    let mut provider_parse_window = snapshot();
    provider_parse_window.observed_at_ms = 1_000_000;
    provider_parse_window.consecutive_failures = 1;
    provider_parse_window.failures = vec![
        failure(999_000, "provider_parse"),
        failure(999_500, "provider_parse"),
        failure(999_900, "provider_parse"),
    ];

    let cases = vec![
        Case {
            name: "grace period failure stays hold",
            config: PolicyConfig::default(),
            snapshot: grace_failure,
            decision: DecisionKind::Hold,
            reason_code: REASON_STARTUP_GRACE,
            delay_ms: Some(1_000),
        },
        Case {
            name: "panic inside grace still rolls back when LKG exists",
            config: PolicyConfig::default(),
            snapshot: panic_in_grace,
            decision: DecisionKind::Rollback,
            reason_code: REASON_PANIC,
            delay_ms: None,
        },
        Case {
            name: "panic inside grace fails when LKG is missing",
            config: PolicyConfig::default(),
            snapshot: panic_no_lkg,
            decision: DecisionKind::Fail,
            reason_code: REASON_PANIC,
            delay_ms: None,
        },
        Case {
            name: "consecutive threshold fails without LKG",
            config: PolicyConfig::default(),
            snapshot: consecutive_no_lkg,
            decision: DecisionKind::Fail,
            reason_code: REASON_CONSECUTIVE_FAILURES,
            delay_ms: None,
        },
        Case {
            name: "consecutive threshold rolls back with LKG",
            config: PolicyConfig::default(),
            snapshot: consecutive_rollback,
            decision: DecisionKind::Rollback,
            reason_code: REASON_CONSECUTIVE_FAILURES,
            delay_ms: None,
        },
        Case {
            name: "window count triggers",
            config: window_config.clone(),
            snapshot: window_trigger,
            decision: DecisionKind::Rollback,
            reason_code: REASON_WINDOW_FAILURES,
            delay_ms: None,
        },
        Case {
            name: "expired window does not trigger",
            config: window_config.clone(),
            snapshot: window_expired,
            decision: DecisionKind::Hold,
            reason_code: REASON_NO_RULE,
            delay_ms: Some(1_000),
        },
        Case {
            name: "health time threshold triggers",
            config: PolicyConfig {
                unhealthy_secs: 1,
                ..PolicyConfig::default()
            },
            snapshot: health_by_time,
            decision: DecisionKind::Rollback,
            reason_code: REASON_HEALTH_UNHEALTHY,
            delay_ms: None,
        },
        Case {
            name: "health probe threshold triggers",
            config: PolicyConfig::default(),
            snapshot: health_by_probes,
            decision: DecisionKind::Rollback,
            reason_code: REASON_HEALTH_UNHEALTHY,
            delay_ms: None,
        },
        Case {
            name: "provider parse counts toward window by default",
            config: window_config.clone(),
            snapshot: provider_parse_window.clone(),
            decision: DecisionKind::Rollback,
            reason_code: REASON_WINDOW_FAILURES,
            delay_ms: None,
        },
        Case {
            name: "provider parse excluded from window when configured",
            config: PolicyConfig {
                provider_parse_uses_window: false,
                ..window_config.clone()
            },
            snapshot: provider_parse_window,
            decision: DecisionKind::Hold,
            reason_code: REASON_NO_RULE,
            delay_ms: Some(1_000),
        },
    ];

    for case in cases {
        let got = decide(&case.config, &case.snapshot);
        assert_eq!(got.decision, case.decision, "{}", case.name);
        assert_eq!(got.reason_code, case.reason_code, "{}", case.name);
        assert_eq!(got.delay_ms, case.delay_ms, "{}", case.name);
    }
}

#[test]
fn startup_grace_is_computed_from_config_when_not_precomputed() {
    let mut snapshot = snapshot();
    snapshot.grace_elapsed = None;
    snapshot.startup_at_ms = Some(9_900);
    snapshot.consecutive_failures = 3;

    let config = PolicyConfig {
        startup_grace_secs: 30,
        ..PolicyConfig::default()
    };
    let got = decide(&config, &snapshot);
    assert_eq!(got.decision, DecisionKind::Hold);
    assert_eq!(got.reason_code, REASON_STARTUP_GRACE);
}

#[test]
fn hold_backoff_ladder_advances_and_caps() {
    let config = PolicyConfig::default();
    for (consecutive_holds, expected_ms) in [
        (0, 1_000),
        (1, 2_000),
        (2, 4_000),
        (3, 8_000),
        (4, 30_000),
        (5, 30_000),
        (10, 30_000),
    ] {
        let mut snapshot = snapshot();
        snapshot.consecutive_holds = consecutive_holds;
        let got = decide(&config, &snapshot);
        assert_eq!(got.decision, DecisionKind::Hold);
        assert_eq!(got.delay_ms, Some(expected_ms));
    }
}

#[test]
fn same_snapshot_produces_identical_decision() {
    let config = PolicyConfig {
        window_failures: 3,
        ..PolicyConfig::default()
    };
    let mut snapshot = snapshot();
    snapshot.consecutive_failures = 1;
    snapshot.failures = vec![
        failure(9_500, "internal"),
        failure(9_700, "internal"),
        failure(9_900, "internal"),
    ];

    let first = decide(&config, &snapshot);
    let second = decide(&config, &snapshot);
    assert_eq!(first, second);
}

#[test]
fn defaults_match_documented_table() {
    let config = PolicyConfig::default();
    assert_eq!(config.startup_grace_secs, DEFAULT_STARTUP_GRACE_SECS);
    assert_eq!(config.consecutive_failures, DEFAULT_CONSECUTIVE_FAILURES);
    assert_eq!(config.window_failures, DEFAULT_WINDOW_FAILURES);
    assert_eq!(config.window_secs, DEFAULT_WINDOW_SECS);
    assert_eq!(config.unhealthy_secs, DEFAULT_UNHEALTHY_SECS);
    assert_eq!(config.unhealthy_probes, DEFAULT_UNHEALTHY_PROBES);
    assert_eq!(config.restart_backoff_secs, DEFAULT_RESTART_BACKOFF_SECS);
    assert_eq!(config.panic_classes, vec!["panic".to_owned()]);
    assert!(config.panic_rollback);
    assert!(config.provider_parse_uses_window);
}
