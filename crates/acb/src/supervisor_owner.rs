//! `supervisor.toml` 的所有权：由 acbl 自己渲染并维护。
//!
//! 迁移期 unit 仍可能传 `--config /nix/store/...`。那种外部配置只读，acbl 不改它；
//! 缺失自有配置时，本模块按同一套派生规则生成一份，并与外部配置做**语义等价**
//! 比较：只有等价才落盘，避免下一次不带 `--config` 启动时行为漂移。
//!
//! 所有派生值都写日志（值 + 来源），不静默采用默认。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use acb_supervisor::component::{ComponentTable, PresetRegistry, parse_config_file};
use acb_supervisor::{Config, ConfigFile, GenerationRef, load_current};

const SUPERVISOR_TEMPLATE: &str = include_str!("../../../deploy/run/supervisor.toml.tmpl");
/// 组件清单模板：结构归仓库，`env` 从旧配置继承（部署专属，不能进仓库）。
const COMPONENTS_TEMPLATE: &str =
    include_str!("../../../deploy/run/supervisor.components.toml.tmpl");
const OWNED_FILE: &str = "supervisor.toml";

/// 组件声明从这一行开始；重渲染时必须原样保留，不能被 supervisor 段覆盖。
const COMPONENT_HEADER: &str = "[[component]]";

pub fn owned_config_path(state_dir: &Path) -> PathBuf {
    state_dir.join(OWNED_FILE)
}

/// 首次以外部 `--config` 启动时的播种结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedOutcome {
    /// 自有配置已经存在，不动它。
    Existing,
    /// 已写入与外部配置语义等价的自有配置。
    Created { path: PathBuf },
    /// 与外部配置存在语义差异；只告警，不写、不失败。
    Skipped { differences: Vec<String> },
}

/// 渲染自有配置文本。
///
/// `existing` 用于保留已存在的 `[[component]]` 声明（外部配置播种时也传它）。
/// `state_dir_source` 只用于日志，说明这个 state_dir 从哪来。
pub fn render_owned(
    state_dir: &Path,
    state_dir_source: &str,
    external: Option<&Config>,
    existing: Option<&str>,
) -> Result<String, String> {
    let current = load_current(state_dir)?;
    let profile_root = crate::profile::root();
    let meta = crate::profile::active_meta()?;
    let meta_generation = meta.as_ref().map(|meta| meta.generation_ref(&profile_root));

    let (program, args, program_source) = match &current {
        Some(spec) => (spec.program.clone(), spec.args.clone(), "current.json"),
        None => derive_from_profile(meta.as_ref(), &profile_root)?,
    };
    let (generation, generation_source) =
        match current.as_ref().and_then(|spec| spec.generation.clone()) {
            Some(generation) => (Some(generation), program_source),
            None => (
                meta_generation,
                if meta.is_some() {
                    "active profile"
                } else {
                    "none"
                },
            ),
        };

    eprintln!("acb-config: program={program} (from {program_source})");
    eprintln!(
        "acb-config: args={} (from {program_source})",
        toml_array(&args)
    );
    eprintln!(
        "acb-config: state_dir={} (from {state_dir_source})",
        state_dir.display()
    );
    match &generation {
        Some(generation) => eprintln!(
            "acb-config: generation={} path={} (from {generation_source})",
            generation.id, generation.path
        ),
        None => eprintln!("acb-config: generation=none (no current.json and no active profile)"),
    }

    let bridge_config = bridge_config_from_args(&args).or_else(default_bridge_config);
    let (health_url, health_source) = derive_health_url(bridge_config.as_deref(), external);
    match &health_url {
        Some(url) => eprintln!("acb-config: health_url={url} (from {health_source})"),
        None => eprintln!("acb-config: health_url=<none> ({health_source})"),
    }

    let health_token_file = external.and_then(|config| config.health_token_file.clone());
    match &health_token_file {
        Some(path) => eprintln!(
            "acb-config: health_token_file={path} (from external config; explicit, cannot derive)"
        ),
        None => eprintln!(
            "acb-config: health_token_file=<auto> \
             ($XDG_RUNTIME_DIR/acb-health.token when present)"
        ),
    }

    if let Some(suffix) = component_suffix(existing) {
        eprintln!(
            "acb-config: preserving {} [[component]] declaration(s) from existing config",
            suffix.matches(COMPONENT_HEADER).count()
        );
    }
    // 预设表可能不存在（测试环境、精简安装）：拿不到就按「全都没有」处理，
    // 组件整段不渲染，配置仍是合法的。
    let available_presets = load_available_presets(state_dir);
    let inputs = RenderInputs {
        presets: &available_presets,
        program: &program,
        args: &args,
        health_url: health_url.as_deref(),
        health_token_file: health_token_file.as_deref(),
        state_dir,
        generation: generation.as_ref(),
    };
    Ok(render_text(&inputs, existing))
}

/// 读出本机已注册的预设 id。
///
/// 找不到 presets.toml 时返回空表：宁可少渲染几个组件，也不要渲染出一份
/// 解析不了的配置（`component_preset_unknown` 会让整条链启动失败）。
fn load_available_presets(state_dir: &Path) -> Vec<String> {
    // 与 supervisor 自己找 presets 的规则保持一致：先看配置同目录，再看安装前缀。
    let candidates = [
        state_dir.join("presets.toml"),
        state_dir.join("../presets.toml"),
    ];
    for candidate in candidates {
        if !candidate.exists() {
            continue;
        }
        if let Ok(registry) = acb_supervisor::component::PresetRegistry::load(&candidate) {
            return registry.iter().map(|preset| preset.id.clone()).collect();
        }
    }
    Vec::new()
}

struct RenderInputs<'a> {
    /// 已注册的预设 id。缺失的组件整段不渲染——否则配置解析失败。
    presets: &'a [String],
    program: &'a str,
    args: &'a [String],
    health_url: Option<&'a str>,
    health_token_file: Option<&'a str>,
    state_dir: &'a Path,
    generation: Option<&'a GenerationRef>,
}

/// 从已有配置里抽某个组件的 `env = { ... }` 行。
///
/// **为什么不进仓库**：这些 env 里是 `/nix/store/<hash>/bin/...` 和
/// `/home/<user>/...`——机器专属，换台机器或换一代就失效。模板只该放
/// 「哪些组件、什么类型、怎么探活」，不该放「装在哪」。
fn inherited_env(existing: Option<&str>, component: &str) -> String {
    let Some(existing) = existing else {
        return String::new();
    };
    let mut current: Option<String> = None;
    for line in existing.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("name = ") {
            current = Some(rest.trim().trim_matches('"').to_owned());
            continue;
        }
        if current.as_deref() == Some(component) && trimmed.starts_with("env = ") {
            return line.trim_end().to_owned();
        }
        // 下一个组件开始了，别再往后找。
        if trimmed.starts_with("[[component]]") {
            current = None;
        }
    }
    String::new()
}

/// 模板认识的组件名。已有配置里**不在这份清单里**的组件会原样追加——
/// 运维自己加的组件不能被模板渲染悄悄抹掉。
const TEMPLATE_COMPONENTS: &[&str] = &[
    "dsh",
    "dsh-presets",
    "drivers",
    "opencode",
    "snowluma",
    "acp",
];

/// 从渲染结果里删掉某个组件的整段（含它的 `[component.*]` 子表）。
///
/// 按「段」切而不是按行删：一个组件可能带 `[component.process]`、
/// `[component.readiness]` 等子表，只删 `name =` 那行会留下孤儿子表。
fn drop_component_block(text: &str, component: &str) -> String {
    let mut blocks: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("[[component]]") && !current.is_empty() {
            blocks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        blocks.push(current);
    }

    let keep: Vec<String> = blocks
        .into_iter()
        .filter(|block| {
            let is_component = block.trim_start().starts_with("[[component]]");
            if !is_component {
                // 不是组件段（模板头部/注释）原样留。
                return true;
            }
            let name = block
                .lines()
                .find_map(|line| line.trim_start().strip_prefix("name = "))
                .map(|rest| rest.trim().trim_matches('"').to_owned());
            name.as_deref() != Some(component)
        })
        .collect();
    keep.concat()
}

/// 抽出已有配置里模板不认识的 `[[component]]` 段，原样保留。
fn unknown_component_blocks(existing: Option<&str>) -> Vec<String> {
    let Some(existing) = existing else {
        return Vec::new();
    };
    let mut blocks: Vec<String> = Vec::new();
    let mut current: Option<(String, String)> = None;
    for line in existing.split_inclusive('\n') {
        if line.trim_start().starts_with("[[component]]") {
            if let Some((name, block)) = current.take() {
                if !TEMPLATE_COMPONENTS.contains(&name.as_str()) {
                    blocks.push(block);
                }
            }
            current = Some((String::new(), line.to_owned()));
            continue;
        }
        if let Some((name, block)) = current.as_mut() {
            if name.is_empty()
                && let Some(rest) = line.trim_start().strip_prefix("name = ")
            {
                *name = rest.trim().trim_matches('"').to_owned();
            }
            block.push_str(line);
        }
    }
    if let Some((name, block)) = current
        && !TEMPLATE_COMPONENTS.contains(&name.as_str())
    {
        blocks.push(block);
    }
    blocks
}

/// 组件 → 它依赖的预设 id。
///
/// 预设不在就用不了这个组件（`component_preset_unknown` 会让整份配置解析失败）。
/// 所以渲染前必须问一句「这个预设装了吗」——测试环境、精简安装都可能没有。
const COMPONENT_PRESETS: &[(&str, &str)] = &[
    ("dsh", "dsh_nix"),
    ("dsh-presets", "dsh_presets_acb"),
    ("drivers", "drivers_acb"),
    ("opencode", "opencode_nix"),
    ("snowluma", "snowluma_container"),
    ("acp", "acp_driver_acb"),
];

/// 渲染组件清单：把每个 `@ENV_<name>@` 换成本机已有配置里的 env 行。
///
/// `available` 是**已注册的预设 id**；预设缺失的组件整段不输出——否则渲染出的
/// 配置会解析失败，把整条链拦死。
fn render_components(existing: Option<&str>, available: &[String]) -> String {
    let names: Vec<&str> = COMPONENT_PRESETS
        .iter()
        .filter(|(_, preset)| available.iter().any(|id| id == preset))
        .map(|(name, _)| *name)
        .collect();
    let mut out = COMPONENTS_TEMPLATE.to_owned();
    for name in &names {
        let env = inherited_env(existing, name);
        out = out.replace(&format!("@ENV_{name}@"), &env);
    }
    // 占位换空会留下空白行；收一下，免得每次渲染都产生 diff。
    let mut text: String = out
        .lines()
        .filter(|line| !line.trim().is_empty() || !line.is_empty())
        .map(|line| format!("{line}\n"))
        .collect();
    // 预设缺失的组件整段去掉：留着会让配置解析失败。
    for (name, _) in COMPONENT_PRESETS {
        if names.contains(&name) {
            continue;
        }
        text = drop_component_block(&text, name);
    }
    // 运维自己加的组件原样追加：模板只该管它认识的那几个。
    for block in unknown_component_blocks(existing) {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push('\n');
        text.push_str(block.trim_end());
        text.push('\n');
    }
    text
}

/// 纯渲染：同样的输入必须逐字节一致，已存在的 `[[component]]` 段原样保留。
fn render_text(inputs: &RenderInputs<'_>, existing: Option<&str>) -> String {
    let health_line = match inputs.health_url {
        Some(url) => format!("health_url = {}", toml_string(url)),
        None => String::new(),
    };
    let health_token_section = match inputs.health_token_file {
        Some(path) => format!("health_token_file = {}\n", toml_string(path)),
        None => String::new(),
    };

    let mut values = BTreeMap::new();
    values.insert(
        "SUPERVISOR_PROGRAM".to_string(),
        toml_escape(inputs.program),
    );
    values.insert("SUPERVISOR_ARGS".to_string(), toml_array(inputs.args));
    values.insert("HEALTH_URL_LINE".to_string(), health_line);
    values.insert("HEALTH_TOKEN_SECTION".to_string(), health_token_section);
    values.insert(
        "RUN_STATE_DIR".to_string(),
        toml_escape(&inputs.state_dir.to_string_lossy()),
    );
    values.insert(
        "GENERATION_SECTION".to_string(),
        generation_section(inputs.generation),
    );

    let mut text = substitute(SUPERVISOR_TEMPLATE, &values)
        .expect("supervisor template placeholders are validated by this module");
    // 组件清单现在也归仓库管；只有 `env` 从旧配置继承。
    text.push('\n');
    text.push_str(&render_components(existing, inputs.presets));
    text
}

/// 保证 `state_dir/supervisor.toml` 存在并与派生结果一致；返回其路径。
pub fn ensure_owned_config(state_dir: &Path, state_dir_source: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(state_dir)
        .map_err(|error| format!("create {}: {error}", state_dir.display()))?;
    let path = owned_config_path(state_dir);
    let existing = read_optional(&path)?;
    let text = render_owned(state_dir, state_dir_source, None, existing.as_deref())?;
    if write_config_if_changed(&path, &text)? {
        eprintln!("acb-config: wrote self-owned config {}", path.display());
    } else {
        eprintln!("acb-config: self-owned config unchanged {}", path.display());
    }
    Ok(path)
}

/// 外部 `--config` 正在使用时：若自有配置缺失，尝试播种一份语义等价的副本。
///
/// `external_text` 里的 `[[component]]` 声明会被原样带进自有配置。
pub fn seed_if_missing(
    state_dir: &Path,
    external_path: &Path,
    external: &Config,
    external_text: &str,
) -> Result<SeedOutcome, String> {
    let path = owned_config_path(state_dir);
    if path.exists() {
        return Ok(SeedOutcome::Existing);
    }

    let text = render_owned(
        state_dir,
        &format!("external config {}", external_path.display()),
        Some(external),
        Some(external_text),
    )?;
    let presets = acb_supervisor::component::load_presets_for(external_path)
        .map_err(|error| error.to_string())?;
    let (owned_file, _table) = parse_owned(&text, &presets, state_dir)?;
    let differences = differences(&owned_file.supervisor, external);
    if !differences.is_empty() {
        return Ok(SeedOutcome::Skipped { differences });
    }

    fs::create_dir_all(state_dir)
        .map_err(|error| format!("create {}: {error}", state_dir.display()))?;
    crate::fsutil::write_atomic(&path, text.as_bytes())?;
    eprintln!(
        "acb-config: seeded self-owned config {} (semantically equivalent to {})",
        path.display(),
        external_path.display()
    );
    Ok(SeedOutcome::Created { path })
}

fn parse_owned(
    text: &str,
    presets: &PresetRegistry,
    base: &std::path::Path,
) -> Result<(ConfigFile, ComponentTable), String> {
    parse_config_file(text, presets, base).map_err(|error| error.to_string())
}

/// 外部配置与自有配置的语义差异；空表表示等价。
///
/// 有意忽略 `generation`（自有配置会补上 current.json 的版本身份）和
/// `rollback_mode`（launcher 运行期强制 External）。
pub fn differences(owned: &Config, external: &Config) -> Vec<String> {
    let mut differences = Vec::new();
    let mut differ = |field: &str| differences.push(field.to_string());

    if canonical(Path::new(&owned.program)) != canonical(Path::new(&external.program)) {
        differ(&format!(
            "program (owned {} vs external {})",
            owned.program, external.program
        ));
    }
    if owned.args != external.args {
        differ(&format!(
            "args (owned {:?} vs external {:?})",
            owned.args, external.args
        ));
    }
    if owned.health_url != external.health_url {
        differ(&format!(
            "health_url (owned {:?} vs external {:?})",
            owned.health_url, external.health_url
        ));
    }
    if owned.health_token_file != external.health_token_file {
        differ(&format!(
            "health_token_file (owned {:?} vs external {:?})",
            owned.health_token_file, external.health_token_file
        ));
    }
    if canonical(Path::new(&owned.state_dir)) != canonical(Path::new(&external.state_dir)) {
        differ(&format!(
            "state_dir (owned {} vs external {})",
            owned.state_dir, external.state_dir
        ));
    }
    if owned.probe_interval_secs != external.probe_interval_secs {
        differ("probe_interval_secs");
    }
    if owned.stable_window_secs != external.stable_window_secs {
        differ("stable_window_secs");
    }
    if owned.lkg_update != external.lkg_update {
        differ("lkg_update");
    }
    if owned.rollback != external.rollback {
        differ("rollback");
    }
    if external.lkg.is_some() {
        differ("external declares supervisor.lkg; owned config does not render it");
    }
    if owned.policy.provider != external.policy.provider {
        differ("policy.provider");
    }
    if owned.policy.command != external.policy.command {
        differ("policy.command");
    }
    if owned.policy.decision_timeout_ms != external.policy.decision_timeout_ms {
        differ("policy.decision_timeout_ms");
    }
    if owned.policy.policy_error_fail_after != external.policy.policy_error_fail_after {
        differ("policy.policy_error_fail_after");
    }
    if owned.policy.on_policy_error != external.policy.on_policy_error {
        differ("policy.on_policy_error");
    }
    if owned.policy.options != external.policy.options {
        differ("policy.* (startup_grace/window/unhealthy/backoff)");
    }
    differences
}

fn derive_from_profile(
    meta: Option<&crate::profile::Meta>,
    profile_root: &Path,
) -> Result<(String, Vec<String>, &'static str), String> {
    let (bin_dir, source) = match meta {
        Some(meta) => (
            meta.payload_path(profile_root).join("bin"),
            "active profile",
        ),
        None => {
            let fallback = std::env::var_os("ACB_UNIT_BIN_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from);
            (
                crate::profile::resolve_bin_dir(fallback.as_deref())?,
                "ACB_UNIT_BIN_DIR",
            )
        }
    };
    let program = bin_dir.join("acb-bridge").to_string_lossy().into_owned();
    let args = match default_bridge_config() {
        Some(path) => {
            eprintln!(
                "acb-config: bridge config {} derived from profile layout / ACB_UNIT_CONFIG_FILE",
                path.display()
            );
            vec!["--config".to_string(), path.to_string_lossy().into_owned()]
        }
        None => {
            eprintln!("acb-config: no known bridge config; starting acb-bridge without --config");
            Vec::new()
        }
    };
    Ok((program, args, source))
}

fn default_bridge_config() -> Option<PathBuf> {
    let explicit = std::env::var_os("ACB_UNIT_CONFIG_FILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    if explicit.is_some() {
        return explicit;
    }
    let profile = crate::profile::root();
    let config_dir = profile.parent()?.join("config");
    // 与 nix-config 的两种合法布局对齐：启用通知时是 config.active.toml，
    // 否则是手写的 config.toml。
    let active = config_dir.join("config.active.toml");
    if active.is_file() {
        return Some(active);
    }
    let plain = config_dir.join("config.toml");
    plain.is_file().then_some(plain)
}

fn bridge_config_from_args(args: &[String]) -> Option<PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--config" {
            return iter.next().map(PathBuf::from);
        }
        if let Some(value) = arg.strip_prefix("--config=") {
            return (!value.is_empty()).then(|| PathBuf::from(value));
        }
    }
    None
}

/// 从 bridge 配置的 `[console] listen` 推导 `http://host:port/healthz`。
fn derive_health_url(
    bridge_config: Option<&Path>,
    external: Option<&Config>,
) -> (Option<String>, String) {
    if let Some(path) = bridge_config {
        match read_console_listen(path) {
            Some(listen) => match listen_to_health_url(&listen) {
                Some(url) => {
                    return (
                        Some(url),
                        format!("{} [console].listen = {:?}", path.display(), listen),
                    );
                }
                None => eprintln!(
                    "acb-config: cannot parse [console].listen {:?} in {}",
                    listen,
                    path.display()
                ),
            },
            None => eprintln!(
                "acb-config: {} has no readable [console].listen",
                path.display()
            ),
        }
    }
    if let Some(url) = external.and_then(|config| config.health_url.clone()) {
        return (
            Some(url),
            "external config health_url (console listen unavailable)".to_string(),
        );
    }
    if let Some(port) = std::env::var_os("ACB_UNIT_CONSOLE_PORT").filter(|value| !value.is_empty())
    {
        let port = port.to_string_lossy();
        return (
            Some(format!("http://127.0.0.1:{port}/healthz")),
            "ACB_UNIT_CONSOLE_PORT".to_string(),
        );
    }
    (
        None,
        "no console listen, external health_url, or port".to_string(),
    )
}

fn read_console_listen(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value
        .get("console")?
        .get("listen")?
        .as_str()
        .map(str::to_owned)
}

fn listen_to_health_url(listen: &str) -> Option<String> {
    let (host, port) = listen.rsplit_once(':')?;
    let port: u16 = port.trim().parse().ok()?;
    if port == 0 {
        return None;
    }
    let host = normalize_host(host.trim());
    Some(format!("http://{host}:{port}/healthz"))
}

fn normalize_host(host: &str) -> String {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare {
        // 监听通配地址时探针只能走回环。
        "0.0.0.0" | "::" => "127.0.0.1".to_string(),
        other => other.to_string(),
    }
}

fn generation_section(generation: Option<&GenerationRef>) -> String {
    let Some(generation) = generation else {
        return String::new();
    };
    let mut section = format!(
        "\n[supervisor.generation]\nid = {}\npath = {}\n",
        toml_string(&generation.id),
        toml_string(&generation.path)
    );
    if let Some(config) = &generation.config_sha256 {
        section.push_str(&format!("config_sha256 = {}\n", toml_string(config)));
    }
    section
}

fn component_suffix(existing: Option<&str>) -> Option<&str> {
    let existing = existing?;
    let mut offset = 0;
    for line in existing.split_inclusive('\n') {
        if line.trim_start().starts_with(COMPONENT_HEADER) {
            return Some(&existing[offset..]);
        }
        offset += line.len();
    }
    None
}

fn write_config_if_changed(path: &Path, text: &str) -> Result<bool, String> {
    match fs::read_to_string(path) {
        Ok(existing) if existing == text => Ok(false),
        Ok(_) => {
            crate::fsutil::write_atomic(path, text.as_bytes())?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::fsutil::write_atomic(path, text.as_bytes())?;
            Ok(true)
        }
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

fn read_optional(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn substitute(template: &str, values: &BTreeMap<String, String>) -> Result<String, String> {
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('@') {
        output.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        let Some(end) = tail.find('@') else {
            return Err("supervisor template: unclosed placeholder".to_string());
        };
        let key = &tail[..end];
        let value = values
            .get(key)
            .ok_or_else(|| format!("supervisor template: unknown placeholder @{key}@"))?;
        output.push_str(value);
        rest = &tail[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn toml_array(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|value| toml_string(value)).collect();
    format!("[{}]", items.join(", "))
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", toml_escape(value))
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "acb-owner-{label}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn render_text_is_idempotent_and_preserves_components() {
        let args = vec![
            "--config".to_string(),
            "/home/user/.local/share/acb/config/config.active.toml".to_string(),
        ];
        let generation = GenerationRef {
            id: "gen-1".to_string(),
            path: "/home/user/.local/share/acb/profile/generations/gen-1/root".to_string(),
            config_sha256: None,
        };
        let state_dir = Path::new("/home/user/.local/share/acb/state/supervisor");
        // 测试里的「已装预设」：把模板认识的都算上，这样组件段会被渲染出来，
        // 才能验证「模板内组件不重复 / 模板外组件保留」。
        let presets: Vec<String> = COMPONENT_PRESETS
            .iter()
            .map(|(_, preset)| preset.to_string())
            .collect();
        let inputs = RenderInputs {
            presets: &presets,
            program: "/bin/acb-bridge",
            args: &args,
            health_url: Some("http://127.0.0.1:3100/healthz"),
            health_token_file: None,
            state_dir,
            generation: Some(&generation),
        };

        let first = render_text(&inputs, None);
        assert!(first.contains("program = \"/bin/acb-bridge\""), "{first}");
        assert!(
            first.contains("health_url = \"http://127.0.0.1:3100/healthz\""),
            "{first}"
        );
        assert_eq!(first, render_text(&inputs, None), "二次渲染必须逐字节一致");

        // 模板认识的组件：清单由模板决定，不该因为旧配置里有一份就重复出现。
        assert_eq!(
            first.matches("name = \"dsh\"").count(),
            1,
            "模板内组件只出现一次"
        );

        // 运维自己加的组件（模板不认识的名字）：必须原样保留。
        // 这条是「模板接管组件清单」的安全底线——不能把别人的配置抹掉。
        let edited =
            format!("{first}\n[[component]]\nname = \"my-extra\"\ncommand = [\"/bin/extra\"]\n");
        let third = render_text(&inputs, Some(&edited));
        assert!(
            third.contains("name = \"my-extra\""),
            "模板外的组件必须保留：{third}"
        );
        assert_eq!(
            third.matches("name = \"my-extra\"").count(),
            1,
            "不能重复追加"
        );
        assert_eq!(
            third,
            render_text(&inputs, Some(&edited)),
            "二次渲染仍要幂等"
        );
    }

    #[test]
    fn write_config_if_changed_skips_identical_content() {
        let dir = temp_dir("unchanged");
        let path = dir.join("supervisor.toml");
        assert!(write_config_if_changed(&path, "text\n").unwrap());
        assert!(!write_config_if_changed(&path, "text\n").unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "text\n");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wildcard_listen_maps_to_loopback_health_url() {
        assert_eq!(
            listen_to_health_url("0.0.0.0:3100").as_deref(),
            Some("http://127.0.0.1:3100/healthz")
        );
        assert_eq!(
            listen_to_health_url("[::]:3100").as_deref(),
            Some("http://127.0.0.1:3100/healthz")
        );
        assert_eq!(
            listen_to_health_url("127.0.0.1:7700").as_deref(),
            Some("http://127.0.0.1:7700/healthz")
        );
        assert_eq!(listen_to_health_url("not-an-address"), None);
    }

    #[test]
    fn component_suffix_is_preserved_verbatim() {
        let existing = "# header\n[supervisor]\nprogram = \"/bin/old\"\n\n[[component]]\n\
                        name = \"dsh\"\ncommand = [\"/bin/dsh\"]\n";
        let suffix = component_suffix(Some(existing)).expect("suffix");
        assert!(suffix.starts_with(COMPONENT_HEADER));
        assert!(suffix.contains("name = \"dsh\""));
        assert!(!suffix.contains("[supervisor]"));
    }

    #[test]
    fn equivalent_configs_have_no_differences() {
        let dir = temp_dir("equiv");
        let profile = dir.join("profile");
        let payload = profile.join("generations").join("gen-1").join("root");
        let bin = payload.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("acb-bridge"), b"").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("generations/gen-1/root", profile.join("current")).unwrap();

        let external_path = dir.join("external.toml");
        let owned_path = dir.join("owned.toml");
        let external = format!(
            r#"
[supervisor]
program = "{}/current/bin/acb-bridge"
args = ["--config", "/home/user/.local/share/acb/config/config.active.toml"]
health_url = "http://127.0.0.1:3100/healthz"
state_dir = "{}/state/supervisor"
"#,
            profile.display(),
            dir.display()
        );
        let owned = format!(
            r#"
[supervisor]
program = "{}/acb-bridge"
args = ["--config", "/home/user/.local/share/acb/config/config.active.toml"]
health_url = "http://127.0.0.1:3100/healthz"
state_dir = "{}/state/supervisor"
probe_interval_secs = 5
stable_window_secs = 60
lkg_update = "on_stable"
rollback = "lkg"
rollback_mode = "external"

[supervisor.policy]
provider = "acb.supervisor.default"
decision_timeout_ms = 1000
policy_error_fail_after = 3
on_policy_error = "hold"
startup_grace_secs = 30
consecutive_failures = 3
window_failures = 5
window_secs = 600
panic_classes = ["panic"]
panic_rollback = true
provider_parse_uses_window = true
unhealthy_secs = 60
unhealthy_probes = 3
restart_backoff_secs = [1, 2, 4, 8, 30]
"#,
            bin.display(),
            dir.display()
        );
        fs::write(&external_path, external).unwrap();
        fs::write(&owned_path, owned).unwrap();
        let external = acb_supervisor::load_config(&external_path).unwrap();
        let owned = acb_supervisor::load_config(&owned_path).unwrap();
        assert_eq!(differences(&owned, &external), Vec::<String>::new());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn real_differences_are_reported() {
        let external = r#"
[supervisor]
program = "/bin/acb-bridge"
health_url = "http://127.0.0.1:3100/healthz"
state_dir = "/state"
"#;
        let owned = r#"
[supervisor]
program = "/bin/acb-bridge"
health_url = "http://127.0.0.1:9999/healthz"
state_dir = "/state"
"#;
        let dir = temp_dir("diff");
        let external_path = dir.join("external.toml");
        let owned_path = dir.join("owned.toml");
        fs::write(&external_path, external).unwrap();
        fs::write(&owned_path, owned).unwrap();
        let external = acb_supervisor::load_config(&external_path).unwrap();
        let owned = acb_supervisor::load_config(&owned_path).unwrap();
        let differences = differences(&owned, &external);
        assert!(
            differences
                .iter()
                .any(|difference| difference.contains("health_url")),
            "{differences:?}"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
