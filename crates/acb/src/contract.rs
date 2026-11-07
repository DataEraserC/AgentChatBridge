//! acbl ↔ payload 契约：payload 根目录的 `acb-contract.json`。
//!
//! acbl 与 payload 是两个独立分发的单元，接口不能靠“刚好同一版仓库”对齐：
//! payload 声明自己遵守的契约版本、能接受的最低 acbl 版本，以及**自己实际包含**的
//! 布局与命令集合；acbl 只校验 payload 声明过的东西，不再要求已经剥离到 acbl 自身
//! 的 launcher / supervisor / unit 模板。
//!
//! v1 定稿规则（只增不改）：
//!
//! - `schema` 是**本文件自身**的格式版本；字段改名/删除/语义变化必须升。
//! - `contract` 是 acbl 与 payload 的接口版本，acbl 支持
//!   [`CONTRACT_MIN`]`..=`[`CONTRACT_MAX`]；payload 声明单个值。
//! - 新增**可选**字段或可选文件不升 `contract`；删除、改名、语义变化必须升。
//! - `min_acbl` 是 payload 需要的最低 acbl 版本，点分数字，与 [`acbl_version`] 比较。
//! - `layout` / `commands` 是 payload 自述；acbl 校验声明的条目确实存在，
//!   但不会因为 payload 没声明某个可选文件而拒绝它。
//!
//! 过渡规则：payload 没有 `acb-contract.json` 时按 [`CONTRACT_MIN`] 处理（旧 payload
//! 仍可启动），调用方应打印醒目告警。删除契约文件就能落回这条旁路，所以真正的防线
//! 是严格模式：设置环境变量 [`STRICT_ENV`] 后，缺文件按 `contract_unreadable` 拒绝。
//! 严格模式默认关闭，等 payload 打包普遍带上契约文件后再翻默认。

use std::cmp::Ordering;
use std::fmt;
use std::path::Path;

use serde::Deserialize;

/// payload 根目录下的契约声明文件名。
pub const CONTRACT_FILE: &str = "acb-contract.json";
/// 契约文件自身的格式版本（不是接口版本）。
pub const FILE_SCHEMA: u32 = 1;
/// acbl 支持的最小 / 最大接口版本。
pub const CONTRACT_MIN: u32 = 1;
pub const CONTRACT_MAX: u32 = 1;
/// 契约 v1 规定的 bridge 入口；acbl 只用这一条命令启动 payload。
pub const BRIDGE_PROGRAM: &str = "bin/acb-bridge";
pub const BRIDGE_CONFIG_FLAG: &str = "--config";

#[derive(Debug, Clone, Deserialize)]
pub struct Contract {
    pub schema: u32,
    pub contract: u32,
    pub min_acbl: String,
    #[serde(default)]
    pub layout: Vec<String>,
    #[serde(default)]
    pub commands: Vec<ContractCommand>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContractCommand {
    pub name: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// 校验结果：payload 声明了契约，或按过渡规则当作旧 payload。
#[derive(Debug, Clone)]
pub enum State {
    Declared(Contract),
    Legacy,
}

impl State {
    pub fn is_legacy(&self) -> bool {
        matches!(self, Self::Legacy)
    }
}

/// 可诊断的失败原因。`code` 是机器可判定的原因码，`detail` 给人看。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub code: &'static str,
    pub detail: String,
}

impl Violation {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "reason_code={} {}", self.code, self.detail)
    }
}

/// acbl 自身的版本。acbl 的三个二进制（launcher / acbctl / supervisor）同属一个
/// workspace，`env!("CARGO_PKG_VERSION")` 是**编译期常量**，不是运行时读二进制。
/// 前提：三个二进制按同一版本一起发布；如果将来拆版，这里的比较方式必须改。
pub fn acbl_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 缺契约文件时的策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingContract {
    /// 过渡行为：按契约 v1 放行（默认）。
    AssumeV1,
    /// 严格模式：缺文件即 `contract_unreadable`，激活路径退出 78。
    Reject,
}

/// 严格模式开关。任意非空且不是 `0/false/no/off` 的值都视为开启（失败朝严格一侧倒）。
pub const STRICT_ENV: &str = "ACB_CONTRACT_STRICT";

/// 从环境变量读取缺文件策略；默认 [`MissingContract::AssumeV1`]。
pub fn configured_missing_policy() -> MissingContract {
    let value = std::env::var_os(STRICT_ENV).map(|value| value.to_string_lossy().into_owned());
    missing_policy_from(value.as_deref())
}

/// 解析严格模式取值，单独抽出来方便测试。
pub fn missing_policy_from(value: Option<&str>) -> MissingContract {
    match value.map(|value| value.trim().to_ascii_lowercase()) {
        None => MissingContract::AssumeV1,
        Some(value) if value.is_empty() => MissingContract::AssumeV1,
        Some(value) if matches!(value.as_str(), "0" | "false" | "no" | "off") => {
            MissingContract::AssumeV1
        }
        Some(_) => MissingContract::Reject,
    }
}

/// acbl 是否支持某个 payload 声明的接口版本。
pub fn supports(contract: u32) -> bool {
    (CONTRACT_MIN..=CONTRACT_MAX).contains(&contract)
}

/// payload 声明的文件；不存在返回 `Ok(None)`，不当作错误。
pub fn load(payload_root: &Path) -> Result<Option<Contract>, Violation> {
    let path = payload_root.join(CONTRACT_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Violation::new(
                "contract_unreadable",
                format!("read {}: {error}", path.display()),
            ));
        }
    };
    let contract: Contract = serde_json::from_str(&text).map_err(|error| {
        Violation::new(
            "contract_unreadable",
            format!("parse {}: {error}", path.display()),
        )
    })?;
    Ok(Some(contract))
}

/// 默认策略（过渡放行）下的校验，等价于 `verify_with(.., AssumeV1)`。
pub fn verify(payload_root: &Path) -> Result<State, Violation> {
    verify_with(payload_root, MissingContract::AssumeV1)
}

/// 校验 payload 的契约声明；缺文件时按 `missing` 策略处理。
/// 旧 payload 走 [`MissingContract::AssumeV1`] 时返回 [`State::Legacy`]。
pub fn verify_with(
    payload_root: &Path,
    missing_contract: MissingContract,
) -> Result<State, Violation> {
    let Some(contract) = load(payload_root)? else {
        return match missing_contract {
            MissingContract::AssumeV1 => Ok(State::Legacy),
            MissingContract::Reject => Err(Violation::new(
                "contract_unreadable",
                format!(
                    "{} is missing from payload {}; strict contract mode ({STRICT_ENV}) \
                     requires an explicit declaration",
                    CONTRACT_FILE,
                    payload_root.display()
                ),
            )),
        };
    };
    if contract.schema != FILE_SCHEMA {
        return Err(Violation::new(
            "contract_schema_unsupported",
            format!(
                "{} declares schema {}, acbl accepts {}",
                CONTRACT_FILE, contract.schema, FILE_SCHEMA
            ),
        ));
    }
    if !supports(contract.contract) {
        return Err(Violation::new(
            "contract_incompatible",
            format!(
                "payload requires acb_contract {}, acbl supports {}..={}",
                contract.contract, CONTRACT_MIN, CONTRACT_MAX
            ),
        ));
    }
    match version_cmp(&contract.min_acbl, acbl_version()) {
        Some(Ordering::Greater) => {
            return Err(Violation::new(
                "acbl_too_old",
                format!(
                    "payload requires acbl >= {}, running acbl is {}",
                    contract.min_acbl,
                    acbl_version()
                ),
            ));
        }
        Some(_) => {}
        None => {
            return Err(Violation::new(
                "contract_malformed",
                format!("min_acbl is not a dotted numeric version: {}", contract.min_acbl),
            ));
        }
    }

    let mut missing = Vec::new();
    for relative in &contract.layout {
        if !safe_relative(relative) {
            return Err(Violation::new(
                "contract_malformed",
                format!("layout entry must be a relative path inside the payload: {relative}"),
            ));
        }
        if !payload_root.join(relative).exists() {
            missing.push(relative.clone());
        }
    }
    if !missing.is_empty() {
        return Err(Violation::new(
            "payload_missing_path",
            format!(
                "payload {} declares missing layout entries: {}",
                payload_root.display(),
                missing.join(", ")
            ),
        ));
    }

    let Some(bridge) = contract.commands.iter().find(|command| command.name == "bridge") else {
        return Err(Violation::new(
            "payload_missing_command",
            format!(
                "{} declares no `bridge` command (payload root {})",
                CONTRACT_FILE,
                payload_root.display()
            ),
        ));
    };
    if bridge.program != BRIDGE_PROGRAM
        || bridge.args.first().map(String::as_str) != Some(BRIDGE_CONFIG_FLAG)
    {
        return Err(Violation::new(
            "payload_bad_command",
            format!(
                "bridge command must be `{BRIDGE_PROGRAM} {BRIDGE_CONFIG_FLAG} <config.toml>`, got `{} {}`",
                bridge.program,
                bridge.args.join(" ")
            ),
        ));
    }
    if !payload_root.join(&bridge.program).is_file() {
        return Err(Violation::new(
            "payload_missing_path",
            format!(
                "bridge program is missing from payload: {}",
                payload_root.join(&bridge.program).display()
            ),
        ));
    }
    for command in &contract.commands {
        if !safe_relative(&command.program) {
            return Err(Violation::new(
                "contract_malformed",
                format!(
                    "command {} program must be a relative path inside the payload: {}",
                    command.name, command.program
                ),
            ));
        }
        if !payload_root.join(&command.program).is_file() {
            return Err(Violation::new(
                "payload_missing_path",
                format!(
                    "command {} program is missing from payload: {}",
                    command.name,
                    payload_root.join(&command.program).display()
                ),
            ));
        }
    }
    Ok(State::Declared(contract))
}

/// 只允许 payload 内的相对路径；`/abs` 与 `..` 一律视为格式错误。
fn safe_relative(relative: &str) -> bool {
    let path = Path::new(relative);
    !relative.trim().is_empty()
        && path.is_relative()
        && !path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
}

/// 点分数字版本比较；任一侧不是纯数字（除 `-` / `+` 后缀）时返回 `None`。
fn version_cmp(left: &str, right: &str) -> Option<Ordering> {
    let parse = |value: &str| -> Option<Vec<u64>> {
        value
            .split('.')
            .map(|part| {
                let number = part
                    .split(['-', '+'])
                    .next()
                    .unwrap_or(part);
                if number.is_empty() {
                    return Some(0);
                }
                number.parse::<u64>().ok()
            })
            .collect()
    };
    let left = parse(left)?;
    let right = parse(right)?;
    let length = left.len().max(right.len());
    for index in 0..length {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        match a.cmp(&b) {
            Ordering::Equal => {}
            other => return Some(other),
        }
    }
    Some(Ordering::Equal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_payload(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acb-contract-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).expect("create payload");
        std::fs::write(dir.join("bin/acb-bridge"), b"#!/bin/sh\n").expect("bridge");
        std::fs::create_dir_all(dir.join("harnesses")).expect("harnesses");
        dir
    }

    fn write_contract(dir: &Path, body: &str) {
        std::fs::write(dir.join(CONTRACT_FILE), body).expect("contract file");
    }

    #[test]
    fn missing_contract_is_legacy() {
        let dir = temp_payload("legacy");
        assert!(matches!(verify(&dir).expect("legacy payload"), State::Legacy));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strict_mode_rejects_missing_contract() {
        let dir = temp_payload("strict");
        let violation = verify_with(&dir, MissingContract::Reject)
            .expect_err("strict mode must reject a contract-less payload");
        assert_eq!(violation.code, "contract_unreadable");
        assert!(violation.detail.contains("ACB_CONTRACT_STRICT"), "{}", violation.detail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strict_switch_parsing_defaults_to_permissive() {
        assert_eq!(missing_policy_from(None), MissingContract::AssumeV1);
        assert_eq!(missing_policy_from(Some("")), MissingContract::AssumeV1);
        assert_eq!(missing_policy_from(Some("0")), MissingContract::AssumeV1);
        assert_eq!(missing_policy_from(Some("false")), MissingContract::AssumeV1);
        assert_eq!(missing_policy_from(Some("OFF")), MissingContract::AssumeV1);
        assert_eq!(missing_policy_from(Some("1")), MissingContract::Reject);
        assert_eq!(missing_policy_from(Some("yes")), MissingContract::Reject);
        // 写错的值朝严格一侧倒，避免误以为“已开启”却仍放行。
        assert_eq!(missing_policy_from(Some("of")), MissingContract::Reject);
    }

    #[test]
    fn incompatible_contract_is_rejected_with_its_own_code() {
        let dir = temp_payload("contract");
        write_contract(
            &dir,
            r#"{"schema":1,"contract":2,"min_acbl":"0.1.0",
                "layout":[],"commands":[{"name":"bridge","program":"bin/acb-bridge","args":["--config"]}]}"#,
        );
        let violation = verify(&dir).expect_err("contract 2 must be rejected");
        assert_eq!(violation.code, "contract_incompatible");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn newer_min_acbl_is_rejected_as_acbl_too_old() {
        let dir = temp_payload("min-acbl");
        write_contract(
            &dir,
            r#"{"schema":1,"contract":1,"min_acbl":"99.0.0",
                "layout":[],"commands":[{"name":"bridge","program":"bin/acb-bridge","args":["--config"]}]}"#,
        );
        let violation = verify(&dir).expect_err("min_acbl 99 must be rejected");
        assert_eq!(violation.code, "acbl_too_old");
        assert!(violation.detail.contains("99.0.0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn declared_layout_must_exist() {
        let dir = temp_payload("layout");
        write_contract(
            &dir,
            r#"{"schema":1,"contract":1,"min_acbl":"0.1.0",
                "layout":["harnesses/shared/tools.mjs"],
                "commands":[{"name":"bridge","program":"bin/acb-bridge","args":["--config"]}]}"#,
        );
        let violation = verify(&dir).expect_err("missing declared layout must fail");
        assert_eq!(violation.code, "payload_missing_path");
        assert!(violation.detail.contains("harnesses/shared/tools.mjs"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bridge_command_shape_is_enforced() {
        let dir = temp_payload("command");
        write_contract(
            &dir,
            r#"{"schema":1,"contract":1,"min_acbl":"0.1.0",
                "layout":[],"commands":[{"name":"bridge","program":"bin/acb-bridge","args":[]}]}"#,
        );
        let violation = verify(&dir).expect_err("bridge without --config must fail");
        assert_eq!(violation.code, "payload_bad_command");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compatible_contract_passes() {
        let dir = temp_payload("ok");
        write_contract(
            &dir,
            r#"{"schema":1,"contract":1,"min_acbl":"0.1.0",
                "layout":["bin","harnesses"],
                "commands":[{"name":"bridge","program":"bin/acb-bridge","args":["--config"]}]}"#,
        );
        assert!(matches!(verify(&dir).expect("compatible payload"), State::Declared(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_compare_handles_short_and_suffixed_versions() {
        assert_eq!(version_cmp("0.1", "0.1.0"), Some(Ordering::Equal));
        assert_eq!(version_cmp("0.2.0", "0.10.0"), Some(Ordering::Less));
        assert_eq!(version_cmp("1.0.0-rc1", "1.0.0"), Some(Ordering::Equal));
        assert_eq!(version_cmp("abc", "1.0.0"), None);
    }
}
