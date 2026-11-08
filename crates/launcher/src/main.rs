//! `acb-launcher`：systemd 的入口进程。
//!
//! systemd 的 `ExecStart` 是静态的，而自动回滚要在运行期换版本。launcher 做三件事：
//! 1. 读 `state_dir/current.json` 决定启动哪个命令（没有指针就写配置里的当前版本）；
//! 2. 用 `acb-supervisor` 的策略循环监督这个子进程；
//! 3. 策略要求回滚时，把 `current.json` 换成 LKG、停掉子进程，并以约定的
//!    `EXIT_ROLLBACK`（75）退出。systemd 拉起后 launcher 读到新指针，自然换版本。
//!
//! launcher 不用 `exec` 替换自己：`exec` 之后就没有进程能停子进程、按契约退出了。
//! `spawn + wait` 才是这里需要的语义。
//!
//! Windows 侧不实现 systemd 通知，但退出码与 `current.json` 逻辑保持平台无关。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use acb::profile;
use acb_supervisor::adopted::AdoptedRunner;
use acb_supervisor::component_runtime::ComponentManager;
use acb_supervisor::instance::{self, DetectOutcome};
use acb_supervisor::upgrade::{
    ActiveUpgrade, Capabilities, Mailbox, UPGRADE_SCHEMA, UpgradeResult, UpgradeTarget,
    detect_restart_owner, is_terminal_status, validate_request,
};
use acb_supervisor::{
    ActivationWriter, CommandSpec, EXIT_GIVE_UP, EXIT_OK, EXIT_ROLLBACK, Parts, Probe,
    ProcessRunner, Projection, ReconcileOutcome, RollbackMode, Runner, Supervisor, SystemClock,
    SystemSleeper, build_policy, load_current, now_ms, proctree,
};

/// launcher 停机总预算：unit 的 `TimeoutStopSec=45s`，留 5s 给 systemd 收尾。
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(40);

fn main() -> ExitCode {
    let (path, external_requested) = match config_path() {
        Some(path) => (path, true),
        None => {
            // 没有 --config：acbl 自己渲染并维护 state/supervisor/supervisor.toml。
            let state_dir = acb::profile::default_state_dir();
            let source = if std::env::var_os("ACB_STATE_DIR").is_some() {
                "ACB_STATE_DIR"
            } else {
                "XDG default"
            };
            match acb::supervisor_owner::ensure_owned_config(&state_dir, source) {
                Ok(path) => (path, false),
                Err(error) => {
                    eprintln!("acb-launcher: cannot render self-owned config: {error}");
                    return ExitCode::from(EXIT_GIVE_UP as u8);
                }
            }
        }
    };
    let config_text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!(
                "acb-launcher: cannot read config {}: {error}",
                path.display()
            );
            return ExitCode::from(EXIT_GIVE_UP as u8);
        }
    };
    let config_sha = acb::fsutil::sha256_hex(config_text.as_bytes());
    let (mut config, components) = match acb_supervisor::load_supervisor_config(&path) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("acb-launcher: {error}");
            return ExitCode::from(EXIT_GIVE_UP as u8);
        }
    };

    // unit 用 ACB_STATE_DIR 与 CLI/acbctl 对齐；显式环境变量优先于配置文件。
    if let Some(dir) = std::env::var_os("ACB_STATE_DIR").filter(|value| !value.is_empty()) {
        config.state_dir = dir.to_string_lossy().into_owned();
    }

    let state_dir = PathBuf::from(&config.state_dir);
    let owned_path = acb::supervisor_owner::owned_config_path(&state_dir);
    let components_dir = acb_supervisor::component_state::state_root(&state_dir).join("components");
    if external_requested && path != owned_path {
        eprintln!(
            "acb-launcher: using external config {} (migration mode): \
             this file stays read-only; component state is written under {} \
             and is not written back to it",
            path.display(),
            components_dir.display()
        );
        match acb::supervisor_owner::seed_if_missing(&state_dir, &path, &config, &config_text) {
            Ok(acb::supervisor_owner::SeedOutcome::Existing) => {}
            Ok(acb::supervisor_owner::SeedOutcome::Created { path }) => {
                eprintln!("acb-launcher: seeded self-owned config {}", path.display());
            }
            Ok(acb::supervisor_owner::SeedOutcome::Skipped { differences }) => {
                eprintln!(
                    "acb-launcher: WARN not seeding self-owned config: external config differs:"
                );
                for difference in differences {
                    eprintln!("acb-launcher: WARN - {difference}");
                }
            }
            Err(error) => {
                eprintln!("acb-launcher: WARN cannot seed self-owned config: {error}");
            }
        }
    } else {
        eprintln!("acb-launcher: using self-owned config {}", path.display());
    }
    if let Err(error) = std::fs::create_dir_all(&state_dir) {
        eprintln!(
            "acb-launcher: cannot create state dir {}: {error}",
            state_dir.display()
        );
        return ExitCode::from(EXIT_GIVE_UP as u8);
    }
    let mailbox = Mailbox::new(&state_dir);
    if let Err(error) = mailbox.ensure() {
        eprintln!("acb-launcher: cannot create upgrade mailbox: {error}");
        return ExitCode::from(EXIT_GIVE_UP as u8);
    }
    let configured = CommandSpec {
        program: config.program.clone(),
        args: config.args.clone(),
        generation: config.generation.clone(),
    };
    // 启动先对账：崩溃留下的 journal 要么补完投影，要么回退投影；不猜版本。
    let writer = match ActivationWriter::acquire(&state_dir) {
        Ok(writer) => writer,
        Err(error) => {
            eprintln!("acb-launcher: cannot lock current.json writer: {error}");
            return ExitCode::from(EXIT_GIVE_UP as u8);
        }
    };
    match writer.reconcile() {
        Ok(ReconcileOutcome::Nothing) => {}
        Ok(outcome) => eprintln!("acb-launcher: pointer reconcile: {outcome:?}"),
        Err(error) => {
            eprintln!("acb-launcher: {error}");
            return ExitCode::from(EXIT_GIVE_UP as u8);
        }
    }
    // TODO(B-12): 在这里比较 payload 声明的 `acb_contract` 与本 acbl 的支持区间；
    // A-8 不实现最低版本校验，只保留这个调用点，避免与契约分支的改动冲突。
    let current = match load_current(&state_dir) {
        Ok(Some(current)) => {
            if current != configured {
                eprintln!(
                    "acb-launcher: current.json selects {} (config says {})",
                    current.id(),
                    configured.id()
                );
            }
            current
        }
        Ok(None) => {
            // 首次安装：没有指针不是损坏，写入配置版本作为唯一真值。
            let projection = config.generation.as_ref().and_then(Projection::discover);
            if let Err(error) = writer.commit(&configured, projection, "acb-launcher", now_ms()) {
                eprintln!("acb-launcher: cannot write current.json: {error}");
                return ExitCode::from(EXIT_GIVE_UP as u8);
            }
            configured
        }
        Err(error) => {
            // 损坏的指针不能静默换成配置版本；留在原地等人工处理。
            eprintln!("acb-launcher: {error}");
            return ExitCode::from(EXIT_GIVE_UP as u8);
        }
    };
    config.program = current.program.clone();
    config.args = current.args.clone();
    // generation 元数据同样以 current.json 为准；否则重启后 LKG 提升/回滚会记错代。
    config.generation = current.generation.clone();
    // 契约校验必须在 supervisor 拉起 bridge 之前：不兼容就以 78 终止，
    // 不出现「旧 bridge 已停、新 bridge 未起」的半启动。
    if let Some(error) = payload_contract_error(&current) {
        eprintln!("acb-launcher: refusing to start generation: {error}");
        return ExitCode::from(EXIT_GIVE_UP as u8);
    }
    // systemd 重启由 launcher 请求；策略本身不直接起 LKG。
    config.rollback_mode = RollbackMode::External;

    let policy = match build_policy(&config) {
        Ok(policy) => policy,
        Err(error) => {
            eprintln!("acb-launcher: {error}");
            return ExitCode::from(EXIT_GIVE_UP as u8);
        }
    };
    let mut probe: Box<dyn Probe> = acb_supervisor::build_probe(&config);
    // acbl 不拥有 bridge：启动前先确认有没有可验证的旧实例。有就接管，
    // 无法确认身份但有健康响应就报错，绝不再起一个去抢同一个端口。
    let instance_path = state_dir.join(instance::INSTANCE_FILE);
    let instance_path_for_state = instance_path.clone();
    let healthy = probe.healthy();
    let runner: Box<dyn Runner> = match instance::detect(&instance_path, &current, healthy) {
        DetectOutcome::Vacant => Box::new(ProcessRunner::with_instance_file(instance_path)),
        DetectOutcome::Adopt(record) => {
            eprintln!(
                "acb-launcher: adopting existing bridge pid {} ({})",
                record.pid, record.instance_id
            );
            Box::new(AdoptedRunner::with_instance_file(*record, instance_path))
        }
        DetectOutcome::Conflict(reason) => {
            instance::write_conflict(&state_dir, &reason);
            eprintln!("acb-launcher: refusing to start: {reason}");
            return ExitCode::from(EXIT_GIVE_UP as u8);
        }
    };
    let mut supervisor = match Supervisor::new(
        config,
        Parts {
            clock: Box::new(SystemClock),
            runner,
            probe,
            sleeper: Box::new(SystemSleeper),
            policy,
        },
    ) {
        Ok(supervisor) => supervisor,
        Err(error) => {
            eprintln!("acb-launcher: {error}");
            return ExitCode::from(EXIT_GIVE_UP as u8);
        }
    };

    // 首次 start 已由 Supervisor::new 完成；写者锁覆盖的是
    // 对账 → 探测 → 决定接管/拉起 → 首次 start 整个临界区，两个 acbl 不会同时
    // 判定“没有 bridge”。之后必须释放：升级提交/回滚会重新 flock 同一路径，
    // 同一进程跨生命周期持有会自锁阻塞。
    drop(writer);

    // P2：组件状态由内核自己落盘；enabled 默认 false，因此升级后现场一个组件
    // 都不会起。初始化失败只告警，不影响 bridge 启动。
    let instance_id = instance::read_record(&instance_path_for_state)
        .ok()
        .flatten()
        .map(|record| record.instance_id);
    let mut component_manager = match ComponentManager::new(
        &state_dir,
        &components,
        &config_sha,
        instance_id.as_deref(),
        now_ms(),
    ) {
        Ok(manager) => {
            if manager.enabled_count() > 0 {
                eprintln!(
                    "acb-launcher: {} enabled component(s) will be supervised",
                    manager.enabled_count()
                );
            }
            Some(manager)
        }
        Err(error) => {
            eprintln!("acb-launcher: WARN cannot initialize component manager: {error}");
            None
        }
    };

    // 启动能力声明：ACB 只在 restart_owner != none 时接受升级请求。
    if let Err(error) = announce_capabilities(&mailbox, &current, &state_dir) {
        eprintln!("acb-launcher: cannot write capabilities.json: {error}");
    }
    resume_active(&mailbox, &current, &state_dir);

    let mut notifier = Notifier::new();
    notifier.ready();
    eprintln!("acb-launcher: started {}", current.id());
    shutdown::install();

    while !supervisor.stopped() {
        if shutdown::requested() {
            // unit 用 KillMode=process，cgroup 不会带走子进程，必须自己收敛：
            // 组件 → bridge → 快照里剩下的非组进程（dsh scope 等）。
            eprintln!("acb-launcher: stop requested; converging descendants");
            converge_shutdown(&mut component_manager, &mut supervisor);
            notifier.stopping();
            return ExitCode::from(EXIT_OK as u8);
        }
        supervisor.tick();
        if let Err(error) = maybe_handle_upgrade(&mut supervisor, &mailbox, &state_dir) {
            eprintln!("acb-launcher: upgrade handling failed: {error}");
        }
        observe_upgrade(&supervisor, &mailbox, &state_dir);
        if let Some(manager) = component_manager.as_mut() {
            manager.tick(now_ms());
        }
        notifier.watchdog();
        thread::sleep(Duration::from_millis(100));
    }

    // 升级 75 / 回滚 / 正常失败都走这里：先收敛组件与残留后代，避免 systemd
    // 重拉后旧组件还活着而新 launcher 又起一份。bridge 可能已由 supervisor
    // 停掉，`stop_child_within` 对空 child 是 no-op。
    converge_shutdown(&mut component_manager, &mut supervisor);

    if let Some(reason) = supervisor.failed() {
        eprintln!("acb-launcher: stopped: {reason}");
    }
    let code = supervisor.exit_code().unwrap_or(EXIT_OK);
    finalize_active(&mailbox, code);
    notifier.stopping();
    ExitCode::from(code as u8)
}

/// 停机的唯一收敛入口：组件 → bridge → 快照里剩下的后代。
///
/// 所有阶段共享一个 deadline。快照必须在任何停止动作之前拍：父进程一死，
/// 后代会被 reparent，之后再也无法按进程树找回（dsh 的 `dsh-subprocess-*.scope`
/// 就是这种情况）。超时后逐条 SIGKILL 并写日志。
fn converge_shutdown(
    component_manager: &mut Option<ComponentManager>,
    supervisor: &mut Supervisor,
) {
    let deadline = Instant::now() + SHUTDOWN_BUDGET;
    let mut roots = vec![std::process::id()];
    if let Some(pid) = supervisor.child_pid() {
        roots.push(pid);
    }
    // 后代树之外再反查 unit cgroup：组件崩掉后 sidecar 会被 reparent 到 systemd，
    // 但不会离开 unit cgroup（见 proctree 模块注释）。
    let snapshot = proctree::Snapshot::capture_unit(&roots);
    // `leave_running` 的 oneshot 是使用者明确要求保留的，不能兜底误杀。
    let protect = component_manager
        .as_ref()
        .map(|manager| snapshot.subtrees(&manager.leave_running_pids()))
        .unwrap_or_default();

    // 组件先按逆依赖序停，再停 bridge；否则依赖 bridge 的组件会先看到一个
    // 已经消失的 endpoint。
    if let Some(manager) = component_manager.as_mut() {
        manager.stop_all_within(now_ms(), deadline);
    }
    if let Err(error) = supervisor.stop_child_within(deadline) {
        eprintln!("acb-launcher: cannot stop bridge: {error}");
    }
    snapshot.terminate_leftovers(&protect, deadline, "launcher shutdown");
}

fn config_path() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--config" {
            return args.next().map(PathBuf::from);
        }
    }
    None
}

/// `<profile>/generations/<id>/root` 往上三层就是 profile 根。
fn profile_root_for(current: &CommandSpec) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ACB_PROFILE_DIR").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let payload = Path::new(current.generation.as_ref()?.path.as_str());
    payload.parent()?.parent()?.parent().map(Path::to_path_buf)
}

/// 启动前的契约校验。返回 `Some(可诊断原因)` 表示必须 78 放弃启动。
/// 没有 generation 的配置式启动（旧部署）不校验；缺契约文件默认过渡放行并告警，
/// 开启 `ACB_CONTRACT_STRICT` 后按 `contract_unreadable` 拒绝（78）。
fn payload_contract_error(current: &CommandSpec) -> Option<String> {
    let generation = current.generation.as_ref()?;
    match acb::contract::verify_with(
        Path::new(&generation.path),
        acb::contract::configured_missing_policy(),
    ) {
        Ok(acb::contract::State::Declared(_)) => None,
        Ok(acb::contract::State::Legacy) => {
            eprintln!(
                "WARNING: generation {} has no {}; treating it as acb_contract={}.",
                generation.id,
                acb::contract::CONTRACT_FILE,
                acb::contract::CONTRACT_MIN
            );
            eprintln!(
                "WARNING: TRANSITIONAL fallback only - the next minor acbl release will \
                 HARD-REJECT such payloads. Ship {} or set {}=1 to enforce strict mode now.",
                acb::contract::CONTRACT_FILE,
                acb::contract::STRICT_ENV
            );
            None
        }
        Err(violation) => Some(format!("generation {}: {violation}", generation.id)),
    }
}

fn announce_capabilities(
    mailbox: &Mailbox,
    current: &CommandSpec,
    state_dir: &Path,
) -> Result<(), String> {
    let capabilities = Capabilities {
        schema: UPGRADE_SCHEMA,
        restart_owner: detect_restart_owner(),
        launcher_pid: std::process::id(),
        profile_dir: profile_root_for(current)
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default(),
        state_dir: state_dir.to_string_lossy().into_owned(),
        strategies: vec!["store".to_string(), "copy".to_string()],
        updated_at_ms: now_ms(),
    };
    mailbox.write_capabilities(&capabilities)
}

/// 重启后恢复观察：`restarting` 且 current 已是目标代 → 进入 `observing`；
/// 若 LKG 已经就是目标代，直升 `stable`。
fn resume_active(mailbox: &Mailbox, current: &CommandSpec, state_dir: &Path) {
    let Some(active) = mailbox.read_active() else {
        return;
    };
    if active.status == "restarting" {
        let same_target = current
            .generation
            .as_ref()
            .map(|generation| generation.id.as_str())
            == active.target_generation_id.as_deref();
        if !same_target {
            finish_request(
                mailbox,
                &active,
                "aborted",
                "restart_did_not_switch",
                "launcher restarted but current.json still selects another generation",
            );
            return;
        }
        if let Err(error) = mailbox.set_active_status("observing", now_ms()) {
            eprintln!("acb-launcher: cannot mark upgrade observing: {error}");
            return;
        }
    }
    if active.status == "restarting" || active.status == "observing" {
        if lkg_generation(state_dir).as_deref() == active.target_generation_id.as_deref() {
            finish_request(
                mailbox,
                &active,
                "stable",
                "stable_window_passed",
                "target generation already holds LKG",
            );
        }
    }
}

fn observe_upgrade(supervisor: &Supervisor, mailbox: &Mailbox, state_dir: &Path) {
    let Some(active) = mailbox.read_active() else {
        return;
    };
    if active.status != "observing" {
        return;
    }
    let promoted = supervisor
        .lkg()
        .and_then(|spec| spec.generation.as_ref())
        .map(|generation| generation.id.as_str())
        == active.target_generation_id.as_deref()
        || lkg_generation(state_dir).as_deref() == active.target_generation_id.as_deref();
    if promoted {
        finish_request(
            mailbox,
            &active,
            "stable",
            "stable_window_passed",
            "new generation passed the stable window and became LKG",
        );
    }
}

/// 升级请求的唯一执行入口：认领 → 校验 → stage → 停旧 → 提交 → 退出 75。
fn maybe_handle_upgrade(
    supervisor: &mut Supervisor,
    mailbox: &Mailbox,
    state_dir: &Path,
) -> Result<(), String> {
    let now = now_ms();
    let Some((request, _path)) = mailbox.claim_next(now)? else {
        return Ok(());
    };
    let Some(active) = mailbox.read_active() else {
        return Err("claimed upgrade request without active.json".to_string());
    };
    let current = supervisor.current().clone();

    if let Err(error) = validate_request(&request) {
        finish_request(mailbox, &active, "rejected", "invalid_request", &error);
        return Ok(());
    }
    if let Some(expected) = &request.expected_current_id {
        let actual = current
            .generation
            .as_ref()
            .map(|generation| generation.id.as_str());
        if actual != Some(expected.as_str()) {
            finish_request(
                mailbox,
                &active,
                "rejected",
                "stale_current",
                &format!(
                    "current generation is {}, request expected {expected}",
                    actual.unwrap_or("-")
                ),
            );
            return Ok(());
        }
    }
    let Some(profile_root) = profile_root_for(&current) else {
        finish_request(
            mailbox,
            &active,
            "rejected",
            "profile_dir_unknown",
            "set ACB_PROFILE_DIR or use a generation-backed current.json",
        );
        return Ok(());
    };

    let target_id = match &request.target {
        UpgradeTarget::Generation { id } => {
            let payload = profile_root.join("generations").join(id).join("root");
            if !payload.is_dir() {
                finish_request(
                    mailbox,
                    &active,
                    "rejected",
                    "target_missing",
                    &format!("generation payload not found: {}", payload.display()),
                );
                return Ok(());
            }
            id.clone()
        }
        UpgradeTarget::Payload { path, strategy } => {
            match profile::add_payload(&profile_root, Path::new(path), strategy) {
                Ok(generation) => generation.id,
                Err(error) => {
                    finish_request(mailbox, &active, "aborted", "stage_failed", error.message());
                    // 契约/布局被拒是配置级终态：统一 78，不再拉起任何版本。
                    if error.is_rejected() {
                        return give_up_on_rejection(supervisor, &error);
                    }
                    return Ok(());
                }
            }
        }
    };

    // 停旧之前把目标代的解析与可执行文件校验做完：任何会失败的准备动作
    // 都发生在服务还活着的时候，构建/产物不可用不能打断正在跑的版本。
    if let Err(error) = profile::validate_generation(&profile_root, state_dir, &target_id) {
        finish_request(
            mailbox,
            &active,
            "aborted",
            "target_invalid",
            error.message(),
        );
        if error.is_rejected() {
            return give_up_on_rejection(supervisor, &error);
        }
        return Ok(());
    }

    mailbox.set_active_status("staged", now_ms())?;

    if let Err(error) = supervisor.stop_child() {
        let _ = supervisor.restart_current();
        finish_request(mailbox, &active, "aborted", "stop_failed", &error);
        return Ok(());
    }

    // 提交点：profile::activate_generation 复用同一个单写者
    //（锁 + journal + current.json + 投影），launcher 不再拼指针或调 acbctl。
    if let Err(error) = profile::activate_generation(&profile_root, state_dir, &target_id) {
        let _ = supervisor.restart_current();
        finish_request(
            mailbox,
            &active,
            "aborted",
            "commit_failed",
            error.message(),
        );
        return Ok(());
    }

    let exit_code = committed_restart_exit(mailbox, &active, &target_id);
    eprintln!(
        "acb-launcher: upgrade {} committed -> {target_id}; exiting {exit_code} for restart",
        active.request_id
    );
    supervisor.request_exit(exit_code);
    Ok(())
}

/// 配置级拒绝的终态：停止当前子进程并按统一约定 78 收场，不走 LKG 回滚。
///
/// 78 与 `ACB_EXIT_GIVE_UP` 同义（配置问题、不再拉起），和 `profile add/switch`
/// 的拒绝退出码保持一致，调用方只需判断「是否 78」就知道是配置被拒而非用法错误。
fn give_up_on_rejection(
    supervisor: &mut acb_supervisor::Supervisor,
    error: &profile::Failure,
) -> Result<(), String> {
    eprintln!("acb-launcher: contract/config rejected: {error}");
    supervisor.shutdown();
    supervisor.request_exit(EXIT_GIVE_UP);
    Ok(())
}

/// 指针提交成功后的收尾：写状态、写结果失败只降级为日志。
///
/// 此时旧进程已停、指针已生效；退出码必须只由「该退出 75」这条语义决定，
/// 否则写盘失败会让 launcher 提前返回，没有任何机制要求 systemd 重拉。
fn committed_restart_exit(mailbox: &Mailbox, active: &ActiveUpgrade, target_id: &str) -> i32 {
    if let Err(error) = mailbox.mark_restarting(target_id, now_ms()) {
        eprintln!("acb-launcher: cannot mark upgrade restarting: {error}");
    }
    let mut result = UpgradeResult::new(
        &active.request_id,
        "restarting",
        "pointer_committed",
        now_ms(),
    );
    result.target_generation_id = Some(target_id.to_string());
    if let Err(error) = mailbox.write_result(&result) {
        eprintln!("acb-launcher: cannot write upgrade result: {error}");
    }
    EXIT_ROLLBACK
}

fn finish_request(
    mailbox: &Mailbox,
    active: &ActiveUpgrade,
    status: &str,
    reason_code: &str,
    reason: &str,
) {
    let mut result = UpgradeResult::new(&active.request_id, status, reason_code, now_ms());
    result.reason = reason.to_string();
    result.target_generation_id = active.target_generation_id.clone();
    if let Err(error) = mailbox.write_result(&result) {
        eprintln!("acb-launcher: cannot write upgrade result: {error}");
    }
    let _ = mailbox.archive_request(&active.request_id, true);
    let _ = mailbox.archive_request(&active.request_id, false);
    if let Err(error) = mailbox.clear_active() {
        eprintln!("acb-launcher: cannot clear active upgrade: {error}");
    }
}

fn finalize_active(mailbox: &Mailbox, exit_code: i32) {
    let Some(active) = mailbox.read_active() else {
        return;
    };
    if is_terminal_status(&active.status) {
        return;
    }
    if exit_code == EXIT_ROLLBACK {
        // `restarting` 是升级成功的半状态，下一轮 launcher 会继续观察。
        if active.status == "restarting" {
            return;
        }
        finish_request(
            mailbox,
            &active,
            "rolled_back",
            "rolled_back_to_lkg",
            "supervisor rolled back to LKG; upgrade request is closed as failed",
        );
    } else if exit_code == EXIT_GIVE_UP {
        finish_request(
            mailbox,
            &active,
            "failed",
            "gave_up",
            "supervisor gave up (78); no automatic restart",
        );
    }
}

fn lkg_generation(state_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(state_dir.join("lkg.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("command")?
        .get("generation")?
        .get("id")?
        .as_str()
        .map(str::to_string)
}

/// SIGTERM/SIGINT → 正常停止（退出 0）。handler 只写原子位，不做任何 I/O。
#[cfg(unix)]
mod shutdown {
    use std::sync::atomic::{AtomicBool, Ordering};

    static REQUESTED: AtomicBool = AtomicBool::new(false);

    extern "C" fn mark(_signal: libc::c_int) {
        REQUESTED.store(true, Ordering::SeqCst);
    }

    pub fn install() {
        // SAFETY: `signal` 只登记一个 async-signal-safe 的 handler；进程生命周期内不撤销。
        unsafe {
            libc::signal(libc::SIGTERM, mark as *const () as libc::sighandler_t);
            libc::signal(libc::SIGINT, mark as *const () as libc::sighandler_t);
        }
    }

    pub fn requested() -> bool {
        REQUESTED.load(Ordering::SeqCst)
    }
}

#[cfg(not(unix))]
mod shutdown {
    pub fn install() {}
    pub fn requested() -> bool {
        false
    }
}

/// systemd `Type=notify` 的最小实现。没有 `NOTIFY_SOCKET` 时全部是空操作。
struct Notifier {
    watchdog_every: Option<Duration>,
    last_watchdog: Instant,
}

impl Notifier {
    fn new() -> Self {
        // systemd 把 WatchdogSec 换算成毫秒放在 WATCHDOG_USEC；按一半间隔心跳。
        let watchdog_every = std::env::var("WATCHDOG_USEC")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|micros| *micros > 0)
            .map(|micros| Duration::from_micros(micros / 2));
        Self {
            watchdog_every,
            last_watchdog: Instant::now(),
        }
    }

    fn ready(&self) {
        notify("READY=1");
    }

    fn stopping(&self) {
        notify("STOPPING=1");
    }

    fn watchdog(&mut self) {
        let Some(every) = self.watchdog_every else {
            return;
        };
        if self.last_watchdog.elapsed() >= every {
            self.last_watchdog = Instant::now();
            notify("WATCHDOG=1");
        }
    }
}

fn notify(message: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixDatagram;
        let Some(path) = std::env::var_os("NOTIFY_SOCKET").map(PathBuf::from) else {
            return;
        };
        let text = path.to_string_lossy();
        if text.starts_with('@') {
            // std 还没有稳定的 abstract-namespace API；systemd 默认用文件系统 socket。
            eprintln!("acb-launcher: abstract NOTIFY_SOCKET is not supported: {text}");
            return;
        }
        let Ok(socket) = UnixDatagram::unbound() else {
            return;
        };
        let _ = socket.send_to(message.as_bytes(), &path);
    }
    #[cfg(not(unix))]
    {
        let _ = message;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acb_supervisor::upgrade::UpgradeRequest;

    fn temp_state_dir(name: &str) -> PathBuf {
        // 优先落在 XDG_DATA_HOME 下，测试永远碰不到真实的 profile/state。
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!(
            "acb-launcher-test-{}-{name}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp state dir");
        dir
    }

    /// 指针提交后的收尾写盘失败，也必须返回 75：旧进程已停、指针已生效，
    /// 不退出 75 就没有人要求 systemd 重拉。
    #[test]
    fn committed_upgrade_still_exits_75_when_state_writes_fail() {
        let state_dir = temp_state_dir("write-fail");
        let mailbox = Mailbox::new(&state_dir);
        mailbox.ensure().expect("ensure mailbox");
        mailbox
            .write_capabilities(&Capabilities {
                schema: UPGRADE_SCHEMA,
                restart_owner: "acbctl-run".to_string(),
                launcher_pid: std::process::id(),
                profile_dir: state_dir.join("profile").to_string_lossy().into_owned(),
                state_dir: state_dir.to_string_lossy().into_owned(),
                strategies: vec!["store".to_string()],
                updated_at_ms: now_ms(),
            })
            .expect("write capabilities");
        let request = UpgradeRequest::new(
            "req-write-fail",
            UpgradeTarget::Generation {
                id: "gen-test".to_string(),
            },
            now_ms(),
        );
        mailbox.submit(request).expect("submit upgrade request");
        assert!(
            mailbox
                .claim_next(now_ms())
                .expect("claim upgrade request")
                .is_some(),
            "request should be claimed"
        );
        let active = mailbox.read_active().expect("active upgrade");

        // 把 requests/ 换成普通文件：ensure() 会在 create_dir_all 上失败，
        // 状态和结果都写不进去；与权限无关，root 下也成立。
        std::fs::remove_dir_all(mailbox.requests_dir()).expect("remove requests dir");
        std::fs::write(mailbox.requests_dir(), b"not a directory").expect("replace with file");

        assert!(
            mailbox.set_active_status("probe", now_ms()).is_err(),
            "state write should fail"
        );
        assert!(
            mailbox
                .write_result(&UpgradeResult::new("probe", "probe", "probe", now_ms()))
                .is_err(),
            "result write should fail"
        );

        assert_eq!(
            committed_restart_exit(&mailbox, &active, "gen-test"),
            EXIT_ROLLBACK
        );

        let _ = std::fs::remove_file(mailbox.requests_dir());
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// payload 目标在 claim 阶段生成不出代 id；提交后必须把它补进 active.json，
    /// 否则 75 重拉后 `resume_active` 拿 current.json 与 `None` 比对必然不等，
    /// 成功切换会被误判成 `restart_did_not_switch`。
    #[test]
    fn payload_upgrade_records_staged_generation_across_restart() {
        // 独立 data home，避免碰到真实 profile/state。
        let data_home = temp_state_dir("payload-upgrade");
        let state_dir = data_home.join("acb");
        std::fs::create_dir_all(&state_dir).expect("create state dir");
        let mailbox = Mailbox::new(&state_dir);
        mailbox.ensure().expect("ensure mailbox");
        mailbox
            .write_capabilities(&Capabilities {
                schema: UPGRADE_SCHEMA,
                restart_owner: "acbctl-run".to_string(),
                launcher_pid: std::process::id(),
                profile_dir: state_dir.join("profile").to_string_lossy().into_owned(),
                state_dir: state_dir.to_string_lossy().into_owned(),
                strategies: vec!["store".to_string()],
                updated_at_ms: now_ms(),
            })
            .expect("write capabilities");
        let request = UpgradeRequest::new(
            "req-payload",
            UpgradeTarget::Payload {
                path: "/nix/store/payload-test".to_string(),
                strategy: "store".to_string(),
            },
            now_ms(),
        );
        match mailbox.submit(request).expect("submit upgrade request") {
            acb_supervisor::upgrade::SubmitOutcome::Queued { .. } => {}
            other => panic!("expected queued request, got {other:?}"),
        }
        assert!(
            mailbox
                .claim_next(now_ms())
                .expect("claim upgrade request")
                .is_some(),
            "request should be claimed"
        );
        let active = mailbox.read_active().expect("active upgrade");
        assert_eq!(
            active.target_generation_id, None,
            "payload 目标在 claim 时还没有代 id"
        );

        assert_eq!(
            committed_restart_exit(&mailbox, &active, "gen-staged"),
            EXIT_ROLLBACK
        );
        let active = mailbox.read_active().expect("active after commit");
        assert_eq!(active.status, "restarting");
        assert_eq!(
            active.target_generation_id.as_deref(),
            Some("gen-staged"),
            "提交后必须记录实际生成的代"
        );

        // 模拟 systemd 收到 75 后的重拉：current.json 已指向新代。
        let current = CommandSpec {
            program: state_dir
                .join("gen-staged/bin/acb-bridge")
                .to_string_lossy()
                .into_owned(),
            args: vec!["--config".to_string(), "/tmp/config.toml".to_string()],
            generation: Some(acb_supervisor::GenerationRef {
                id: "gen-staged".to_string(),
                path: state_dir.join("gen-staged").to_string_lossy().into_owned(),
                config_sha256: None,
            }),
        };
        resume_active(&mailbox, &current, &state_dir);

        let result = mailbox.read_result("req-payload").expect("upgrade result");
        assert_ne!(result.status, "aborted", "成功切换不能记成 aborted");
        assert_ne!(result.reason_code, "restart_did_not_switch");
        assert_eq!(
            mailbox.read_active().expect("active upgrade").status,
            "observing",
            "应当进入观察期等待 LKG"
        );
        let _ = std::fs::remove_dir_all(&data_home);
    }

    /// 启动读到不兼容契约的 generation 时，必须在拉起 bridge 之前就给出可诊断原因。
    #[test]
    fn startup_rejects_generation_with_incompatible_contract() {
        let payload = temp_state_dir("payload-contract").join("root");
        std::fs::create_dir_all(payload.join("bin")).expect("payload bin");
        std::fs::write(payload.join("bin/acb-bridge"), b"#!/bin/sh\n").expect("bridge");
        std::fs::write(
            payload.join("acb-contract.json"),
            r#"{"schema":1,"contract":2,"min_acbl":"0.1.0","layout":[],
                "commands":[{"name":"bridge","program":"bin/acb-bridge","args":["--config"]}]}"#,
        )
        .expect("contract file");
        let current = CommandSpec {
            program: payload
                .join("bin/acb-bridge")
                .to_string_lossy()
                .into_owned(),
            args: vec!["--config".to_string(), "/tmp/config.toml".to_string()],
            generation: Some(acb_supervisor::GenerationRef {
                id: "gen-test".to_string(),
                path: payload.to_string_lossy().into_owned(),
                config_sha256: None,
            }),
        };
        let error = payload_contract_error(&current).expect("contract 2 must be rejected");
        assert!(error.contains("contract_incompatible"), "{error}");
        assert!(error.contains("gen-test"), "{error}");
        let _ = std::fs::remove_dir_all(payload.parent().expect("payload parent"));
    }
}
