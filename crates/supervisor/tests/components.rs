//! `[[component]]` 解析与校验：P1 只做配置层，不 spawn、不写状态。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use acb_supervisor::component::{
    ComponentError, ComponentSource, ComponentType, LogTarget, PresetRegistry, PresetSource,
    ReadinessKind, RestartPolicy, parse_config_file, resolve_components,
};
use acb_supervisor::{load_components, load_config, load_config_file};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "acb-components-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, text).expect("write config");
    path
}

fn parse(
    text: &str,
    presets: &PresetRegistry,
) -> Result<
    (
        acb_supervisor::ConfigFile,
        acb_supervisor::ComponentTable,
    ),
    ComponentError,
> {
    parse_config_file(text, presets)
}

fn error_code(text: &str, presets: &PresetRegistry) -> &'static str {
    parse(text, presets).expect_err("配置应被拒绝").code()
}

const SUPERVISOR: &str = "[supervisor]\nprogram = \"/bin/acb-bridge\"\n";

const PRESETS: &str = "
schema = 1
[[preset]]
id = \"dsh_nix\"
component = \"dsh\"
type = \"simple\"
source = \"nix\"
script = \"dsh/dsh_nix.sh\"
[preset.readiness]
kind = \"tcp\"
port = 3080

[[preset]]
id = \"dsh_npm\"
component = \"dsh\"
type = \"simple\"
source = \"npm\"
script = \"dsh/dsh_npm.sh\"
[preset.readiness]
kind = \"tcp\"
port = 3080

[[preset]]
id = \"snowluma_oneshot\"
component = \"snowluma\"
type = \"oneshot\"
source = \"container\"
script = \"snowluma/oneshot.sh\"
[preset.readiness]
kind = \"tcp\"
port = 53001
";

#[test]
fn minimal_component_uses_contract_defaults() {
    let text = format!(
        "{SUPERVISOR}
[[component]]
name = \"dsh\"
command = [\"/bin/dsh\"]
[component.readiness]
kind = \"tcp\"
port = 3080
"
    );
    let registry = PresetRegistry::empty();
    let (_file, table) = parse(&text, &registry).expect("合法最小配置");

    assert_eq!(table.len(), 1);
    let component = &table.components[0];
    assert_eq!(component.name, "dsh");
    assert_eq!(component.kind, ComponentType::Simple);
    assert_eq!(component.source, ComponentSource::Custom);
    assert!(!component.enabled, "自定义组件默认不启用");
    assert_eq!(component.readiness.kind, ReadinessKind::Tcp);
    assert_eq!(component.readiness.host, "127.0.0.1");
    assert_eq!(component.readiness.port, Some(3080));
    assert_eq!(component.readiness.interval_secs, 5, "探针 5s");
    assert_eq!(component.readiness.failures_threshold, 5, "5s × 5");
    assert_eq!(component.readiness.success_threshold, 3);
    assert_eq!(component.readiness.startup_grace_secs, 60);
    assert_eq!(component.restart.policy, RestartPolicy::OnFailure);
    assert_eq!(component.restart.max_attempts, 5);
    assert_eq!(component.restart.backoff_secs, vec![1, 2, 4, 8, 30]);
    assert_eq!(component.restart.backoff_cap_secs, 60);
    assert_eq!(component.stop_timeout_secs, 45);
    assert_eq!(component.log.target, LogTarget::File);
}

#[test]
fn preset_defaults_are_inherited_from_sibling_manifest() {
    let dir = TempDir::new("preset-sibling");
    write(dir.path(), "presets.toml", PRESETS);
    let config = write(
        dir.path(),
        "supervisor.toml",
        &format!(
            "{SUPERVISOR}
[[component]]
name = \"dsh\"
preset = \"dsh_nix\"
enabled = true
"
        ),
    );

    let table = load_components(&config).expect("预设引用可解析");
    assert_eq!(table.len(), 1);
    let component = &table.components[0];
    assert_eq!(component.kind, ComponentType::Simple, "type 继承预设");
    assert_eq!(
        component.source,
        ComponentSource::Preset(PresetSource::Nix)
    );
    assert_eq!(component.readiness.kind, ReadinessKind::Tcp);
    assert_eq!(component.readiness.port, Some(3080), "探针继承预设");
    assert!(component.enabled);
}

#[test]
fn unknown_field_is_rejected_loudly() {
    let text = format!(
        "{SUPERVISOR}
[[component]]
name = \"dsh\"
soruce = \"nix\"
command = [\"/bin/dsh\"]
"
    );
    let registry = PresetRegistry::empty();
    let code = error_code(&text, &registry);
    assert_eq!(code, "component_unknown_field");
}

#[test]
fn unknown_top_level_table_is_rejected() {
    let text = format!(
        "{SUPERVISOR}
[[components]]
name = \"dsh\"
command = [\"/bin/dsh\"]
"
    );
    let registry = PresetRegistry::empty();
    let code = error_code(&text, &registry);
    assert_eq!(code, "component_unknown_field");
}

#[test]
fn bridge_name_is_reserved() {
    let text = format!(
        "{SUPERVISOR}
[[component]]
name = \"bridge\"
command = [\"/bin/bridge\"]
"
    );
    let registry = PresetRegistry::empty();
    assert_eq!(error_code(&text, &registry), "component_reserved_name");
}

#[test]
fn oneshot_requires_explicit_non_process_readiness() {
    let registry = PresetRegistry::empty();

    let missing = format!(
        "{SUPERVISOR}
[[component]]
name = \"snowluma\"
type = \"oneshot\"
command = [\"/bin/compose\"]
"
    );
    assert_eq!(
        error_code(&missing, &registry),
        "component_readiness_required"
    );

    let process = format!(
        "{SUPERVISOR}
[[component]]
name = \"snowluma\"
type = \"oneshot\"
command = [\"/bin/compose\"]
[component.readiness]
kind = \"process\"
"
    );
    assert_eq!(
        error_code(&process, &registry),
        "component_readiness_required"
    );
}

#[test]
fn oneshot_cannot_inherit_readiness_from_preset() {
    let registry = PresetRegistry::from_toml(PRESETS).expect("presets");
    let text = format!(
        "{SUPERVISOR}
[[component]]
name = \"snowluma\"
preset = \"snowluma_oneshot\"
"
    );
    assert_eq!(
        error_code(&text, &registry),
        "component_readiness_required"
    );
}

#[test]
fn oneshot_enabled_without_file_is_rejected() {
    let text = format!(
        "{SUPERVISOR}
[[component]]
name = \"snowluma\"
type = \"oneshot\"
command = [\"/bin/compose\"]
enabled = true
compose_bin = [\"podman-compose\"]
[component.readiness]
kind = \"tcp\"
port = 53001
"
    );
    let registry = PresetRegistry::empty();
    assert_eq!(
        error_code(&text, &registry),
        "component_oneshot_file_missing"
    );
}

#[test]
fn oneshot_enabled_without_compose_bin_is_rejected() {
    let text = format!(
        "{SUPERVISOR}
[[component]]
name = \"snowluma\"
type = \"oneshot\"
command = [\"/bin/compose\"]
enabled = true
file = \"/tmp/snowluma-compose.yml\"
[component.readiness]
kind = \"tcp\"
port = 53001
"
    );
    let registry = PresetRegistry::empty();
    assert_eq!(
        error_code(&text, &registry),
        "component_oneshot_compose_bin_missing"
    );
}

#[test]
fn dependency_to_undeclared_target_is_rejected() {
    let text = format!(
        "{SUPERVISOR}
[[component]]
name = \"drivers\"
command = [\"/bin/drivers\"]
[component.depends]
after = [\"nope\"]
"
    );
    let registry = PresetRegistry::empty();
    assert_eq!(
        error_code(&text, &registry),
        "component_dependency_unknown"
    );
}

#[test]
fn bridge_and_stack_are_valid_implicit_targets() {
    let text = format!(
        "{SUPERVISOR}
[[component]]
name = \"drivers\"
command = [\"/bin/drivers\"]
[component.depends]
requires = [\"bridge\"]
part_of = [\"bridge\", \"stack\"]
"
    );
    let registry = PresetRegistry::empty();
    let (_file, table) = parse(&text, &registry).expect("bridge / stack 是隐式目标");
    assert_eq!(table.len(), 1);
}

#[test]
fn preset_reference_must_exist() {
    let text = format!(
        "{SUPERVISOR}
[[component]]
name = \"dsh\"
preset = \"dsh_nix\"
"
    );
    let registry = PresetRegistry::empty();
    assert_eq!(
        error_code(&text, &registry),
        "component_preset_unknown"
    );
}

#[test]
fn multiple_enabled_sources_are_rejected() {
    let registry = PresetRegistry::from_toml(PRESETS).expect("presets");
    let text = format!(
        "{SUPERVISOR}
[[component]]
name = \"dsh\"
preset = \"dsh_nix\"
enabled = true

[[component]]
name = \"dsh\"
preset = \"dsh_npm\"
enabled = true
"
    );
    assert_eq!(
        error_code(&text, &registry),
        "component_multiple_sources"
    );
}

#[test]
fn duplicate_same_source_is_rejected() {
    let registry = PresetRegistry::from_toml(PRESETS).expect("presets");
    let text = format!(
        "{SUPERVISOR}
[[component]]
name = \"dsh\"
preset = \"dsh_nix\"

[[component]]
name = \"dsh\"
preset = \"dsh_nix\"
"
    );
    assert_eq!(
        error_code(&text, &registry),
        "component_duplicate_name"
    );
}

#[test]
fn dependency_cycle_is_rejected() {
    let text = format!(
        "{SUPERVISOR}
[[component]]
name = \"a\"
command = [\"/bin/a\"]
[component.depends]
after = [\"b\"]

[[component]]
name = \"b\"
command = [\"/bin/b\"]
[component.depends]
after = [\"a\"]
"
    );
    let registry = PresetRegistry::empty();
    assert_eq!(
        error_code(&text, &registry),
        "component_dependency_cycle"
    );
}

#[test]
fn preset_after_is_merged_and_exit_readiness_is_inherited() {
    let presets = "
schema = 1
[[preset]]
id = \"dsh_nix\"
component = \"dsh\"
type = \"simple\"
source = \"nix\"
script = \"dsh/dsh_nix.sh\"
[preset.readiness]
kind = \"tcp\"
port = 3080

[[preset]]
id = \"dsh_presets_acb\"
component = \"dsh-presets\"
type = \"oneshot\"
source = \"acb\"
script = \"dsh-presets/dsh_presets_acb.sh\"
after = [\"dsh\"]
[preset.readiness]
kind = \"exit\"
";
    let registry = PresetRegistry::from_toml(presets).expect("presets");
    let text = format!(
        "{SUPERVISOR}
[[component]]
name = \"dsh\"
preset = \"dsh_nix\"
enabled = true

[[component]]
name = \"dsh-presets\"
preset = \"dsh_presets_acb\"
enabled = true
depends.after = [\"bridge\"]
"
    );
    let (_file, table) = parse(&text, &registry).expect("脚本预设 oneshot 可启用");
    let component = table
        .components
        .iter()
        .find(|component| component.name == "dsh-presets")
        .expect("dsh-presets");

    assert_eq!(component.kind, ComponentType::Oneshot);
    assert!(component.enabled, "脚本预设 oneshot 不需要 compose file");
    assert_eq!(
        component.readiness.kind,
        ReadinessKind::Exit,
        "预设的 exit 探针可被 oneshot 继承"
    );
    assert_eq!(
        component.depends.after,
        vec!["bridge".to_string(), "dsh".to_string()],
        "预设 after 合并到组件依赖里（声明在前）"
    );
}

#[test]
fn exit_readiness_is_rejected_for_simple_components() {
    let text = format!(
        "{SUPERVISOR}
[[component]]
name = \"dsh\"
command = [\"/bin/dsh\"]
[component.readiness]
kind = \"exit\"
"
    );
    let registry = PresetRegistry::empty();
    assert_eq!(
        error_code(&text, &registry),
        "component_readiness_invalid"
    );
}

#[test]
fn empty_component_table_keeps_legacy_behaviour() {
    let dir = TempDir::new("empty-regression");
    let config = write(
        dir.path(),
        "supervisor.toml",
        "[supervisor]\nprogram = \"/bin/acb-bridge\"\nstate_dir = \"state/supervisor\"\n[supervisor.policy]\nprovider = \"acb.supervisor.default\"\n",
    );

    let file = load_config_file(&config).expect("旧配置仍可解析");
    assert!(file.components.is_empty(), "没有 [[component]] 就没有组件");

    let table = load_components(&config).expect("组件表为空");
    assert!(table.is_empty());

    let supervisor = load_config(&config).expect("旧入口行为不变");
    assert_eq!(supervisor.program, "/bin/acb-bridge");
    assert_eq!(supervisor.state_dir, "state/supervisor");
    assert_eq!(supervisor.probe_interval_secs, 5);
    assert_eq!(supervisor.stable_window_secs, 60);

    let resolved = resolve_components(&[], &PresetRegistry::empty()).expect("空表合法");
    assert!(resolved.is_empty());
}
