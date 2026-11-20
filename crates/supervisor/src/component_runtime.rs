//! 组件进程管理内核（P2 第二半，契约 §5.3 / §5.8 / §5.9 / §5.10）。
//!
//! 只由 launcher 驱动：每个 tick 调 [`ComponentManager::tick`]，退出时调
//! [`ComponentManager::stop_all`]。`enabled = false` 的组件永远不 spawn。
//!
//! 进程隔离：每个组件通过 `process_group(0)` 建独立进程组；停止时按
//! `kill_mode` 对整个组发 TERM，超时后 KILL。日志统一追加到
//! `<state>/logs/<name>.log`，不把 `env` / `env_file` 的值写进日志。

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::ExitInfo;
use crate::component::{
    ComponentTable, ComponentType, KillMode, LogTarget, ReadinessKind, ResolvedComponent,
    RestartPolicy, StopAction,
};
use crate::component_state::{self, ComponentState, LOGS_DIR, STATE_SCHEMA};

const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const DEFAULT_BACKOFF_SECS: [u64; 5] = [1, 2, 4, 8, 30];

/// 状态文件里的 `phase` 取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Stopped,
    Starting,
    Healthy,
    Unhealthy,
    Restarting,
    Failed,
    /// simple 组件非主动退出且 exit 0：不重启，但状态可见。
    StoppedEarly,
    /// `requires` 依赖不可用，暂不启动。
    Blocked,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
            Self::Restarting => "restarting",
            Self::Failed => "failed",
            Self::StoppedEarly => "stopped_early",
            Self::Blocked => "blocked",
        }
    }
}

/// 同一 tick 内所有组件看到的只读快照（避免读到本轮刚被改写的状态）。
#[derive(Debug, Clone)]
struct Snapshot {
    name: String,
    phase: Phase,
    ready_generation: u64,
}

struct Managed {
    entry: ResolvedComponent,
    phase: Phase,
    child: Option<Child>,
    /// 由 acbl 主动发起的停止；用于区分 `stopped_early`。
    stop_requested: bool,
    attempts: u32,
    consecutive_failures: u32,
    restarts: u32,
    probe_failures: u32,
    probe_successes: u32,
    last_probe_at_ms: Option<i64>,
    next_start_at_ms: i64,
    started_at_ms: Option<i64>,
    last_exit: Option<ExitInfo>,
    last_error: Option<String>,
    last_result: Option<String>,
    ready_at_ms: Option<i64>,
    failed_at_ms: Option<i64>,
    last_run_at_ms: Option<i64>,
    /// 就绪代次：每次进入 `healthy` 加一。依赖它的 oneshot 用这个值判断
    /// 「依赖是不是重新就绪了」，而不是看进程有没有刚起来。
    ready_generation: u64,
    /// 该 oneshot 上次执行时，各 `after` 依赖的就绪代次快照。
    ran_dependency_generations: Vec<(String, u64)>,
    log_path: PathBuf,
    dirty: bool,
    /// 承载这个组件的 systemd unit 名（ADR-0034）。
    ///
    /// 非 `None` 时进程跑在独立 unit 里：`child` 为 `None`，存活与停止都走
    /// `systemctl --user`，而不是 `try_wait` / `Child::kill`。
    unit: Option<String>,
    /// 从上一代 launcher 接管的进程 pid（ADR-0034）。
    ///
    /// 非 `None` 时 `child` 必为 `None`：接管的进程不是我们的子进程，拿不到
    /// `Child` 句柄，所以存活判定要走 `/proc`（`instance::process_alive`）
    /// 与启动 token 比对，而不是 `try_wait`。
    adopted_pid: Option<u32>,
}

impl Managed {
    fn record_failure(&mut self, message: impl Into<String>) {
        self.consecutive_failures += 1;
        self.last_error = Some(message.into());
        self.dirty = true;
    }
}

/// 判断上一条 state 里的进程能否被本次启动接管（ADR-0034）。
///
/// 三条全中才返回 `Some(pid)`：
/// 1. state 里记着 pid；
/// 2. 该 pid 还活着；
/// 3. `/proc/<pid>/exe` 与本次要启动的 program 是同一个程序。
///
/// 第 3 条不能省：pid 会被系统复用，「活着」不等于「还是我们的那个进程」。
/// 误接管一个无关进程，比多起一份更难查——它占着端口，而日志里看不出异常。
///
/// 拿不到 program（配置在解析阶段就有问题）时不接管：那种情况应当走正常
/// 启动路径去报错，而不是把一个既存进程当成自己的。
/// 换代后接管：上一代的组件跑在 `acb-component-<name>.service` 里，新 launcher
/// 起来时那个 unit 仍然 active（它不在 bridge 的 cgroup 里，ADB-0034 实测确认）。
///
/// 认领之后不能再去 spawn，否则同一组件两份进程——两份抢同一个端口。
fn adopt_unit(entry: &ResolvedComponent) -> Option<(String, u32)> {
    if entry.kind == ComponentType::Oneshot && entry.stop_action == StopAction::LeaveRunning {
        return None;
    }
    let unit = crate::component_unit::unit_name(&entry.name);
    let state = crate::component_unit::state(&unit)?;
    if state.active_state != "active" {
        return None;
    }
    if state.main_pid == 0 {
        return None;
    }
    // 这里**不做命令行复核**，与裸进程接管（`adopt_candidate`）不同。
    //
    // 理由：unit 名 `acb-component-<name>.service` 是我们自己起的，它本身就是
    // 身份；而命令行不可靠——组件脚本常用 `exec` 把自己换成真正的程序，
    // 脚本名从 cmdline 里彻底消失。实测：
    //   opencode: cmdline = [bash .../opencode_nix.sh]  ← 能对上
    //   dsh:      cmdline = [node .../bin.js]           ← dsh_nix.sh 已被 exec 掉
    // 拿它做判据会让 dsh 每次换代都被重建（实测 4 个组件里只有它重启了），
    // 而 dsh 恰恰是会话所在的那个。
    //
    // 改成校验我们自己注入的 `ACB_COMPONENT_NAME`：它是 unit 的属性，
    // `exec` 抹不掉，也因此挡得住「unit 名被外部占用」这种意外。
    let name = crate::component_unit::component_name_of(&unit)?;
    if name != entry.name {
        return None;
    }
    Some((unit, state.main_pid))
}

fn adopt_candidate(entry: &ResolvedComponent, previous: Option<&ComponentState>) -> Option<u32> {
    // `leave_running` 的 oneshot 本来就故意留着，语义上不算接管。
    if entry.kind == ComponentType::Oneshot && entry.stop_action == StopAction::LeaveRunning {
        return None;
    }
    let pid = previous.and_then(|state| state.pid)?;
    if !crate::instance::process_alive(pid) {
        return None;
    }
    let (program, args) = command_for(entry).ok()?;
    let cmdline = crate::instance::process_cmdline(pid)?;

    // 判据用**命令行**而不是 `/proc/<pid>/exe`。
    //
    // 原因（实测，2026-11-17）：acb 的组件大多是脚本，`exe` 读到的是解释器：
    //   opencode: exe = bash,  cmdline = [bash, .../opencode_nix.sh]
    //   dsh:      exe = node,  cmdline = [node, --expose-internals, .../dsh...]
    // 拿 exe 去比配置里的程序路径永远不相等，接管会全部失败。
    //
    // 判据：配置里的 program 必须出现在 cmdline 里，且 `args` 按序出现在它之后。
    // 前者挡住「pid 复用到一个无关进程」，后者挡住「同一个脚本的不同实例」。
    if !cmdline
        .iter()
        .any(|part| crate::instance::same_program(part, &program))
    {
        return None;
    }
    let start = cmdline
        .iter()
        .position(|part| crate::instance::same_program(part, &program))?;
    let tail = &cmdline[start + 1..];
    if !args.iter().all(|arg| tail.iter().any(|part| part == arg)) {
        return None;
    }
    Some(pid)
}

/// 进程管理内核。只管理声明的组件；空表时 `tick` 是 no-op。
pub struct ComponentManager {
    state_dir: PathBuf,
    instance_id: Option<String>,
    config_sha256: String,
    /// 按启动顺序排列，停止时逆序。
    components: Vec<Managed>,
    enabled_count: usize,
}

impl ComponentManager {
    pub fn new(
        state_dir: &Path,
        table: &ComponentTable,
        config_sha256: &str,
        instance_id: Option<&str>,
        now: i64,
    ) -> Result<Self, String> {
        let state_root = component_state::state_root(state_dir);
        let logs_dir = state_root.join(LOGS_DIR);
        if !table.is_empty() {
            fs::create_dir_all(&logs_dir)
                .map_err(|error| format!("create {}: {error}", logs_dir.display()))?;
            let components_dir = state_root.join(component_state::COMPONENTS_DIR);
            fs::create_dir_all(&components_dir)
                .map_err(|error| format!("create {}: {error}", components_dir.display()))?;
        }

        let mut names: Vec<String> = Vec::new();
        for entry in &table.components {
            if !names.iter().any(|name| name == &entry.name) {
                names.push(entry.name.clone());
            }
        }

        let mut components: Vec<Managed> = Vec::with_capacity(names.len());
        for name in &names {
            let entry = effective_entry(table, name);
            let log_path = match entry.log.target {
                LogTarget::File
                    if entry
                        .log
                        .path
                        .as_deref()
                        .is_some_and(|path| !path.is_empty()) =>
                {
                    PathBuf::from(entry.log.path.as_deref().unwrap_or_default())
                }
                // journald 目前也落文件：acbl 不接管 systemd journal，先保证有日志可查。
                _ => logs_dir.join(format!("{name}.log")),
            };
            let previous = ComponentState::load(state_dir, name)?;
            let previous = previous.filter(|state| state.config_sha256 == config_sha256);
            // 换代判定：每次 launcher 启动都会生成新的 instance_id，所以
            // 上一条 state 的 id 与本次不同就说明这是一个新的 launcher 实例
            // （nixos-rebuild / systemctl restart acb-launcher 都会换代）。
            //
            // 上一代 failed 终态不应跨实例继承：撞名（unit was already loaded）
            // 之类的换代竞态在新实例里往往已经消失，若继续沿用 Failed，
            // `tick_one` 会直接 return，组件再也不会被拉起——实测 rebuild 后
            // dsh 驱动因此从未连桥。相当于换代时自动做一次 `acbctl component
            // reset`，但只针对 failed 终态：同实例内的失败仍是终态，
            // max_attempts 策略依然有效。
            let generation_switched = previous
                .as_ref()
                .and_then(|state| state.instance_id.as_deref())
                .zip(instance_id)
                .is_some_and(|(before, current)| before != current);
            let failed_in_previous = previous
                .as_ref()
                .is_some_and(|state| state.phase == Phase::Failed.as_str());
            // 仅「换代 + 上一代 failed」才重试：同实例内的失败保持终态，
            // 让 max_attempts 策略仍然有效；healthy 组件换代照常继承计数。
            let retry_after_generation_switch = generation_switched && failed_in_previous;
            if retry_after_generation_switch {
                eprintln!(
                    "acb-component: clearing inherited failed state for {} on launcher generation switch",
                    entry.name
                );
            }
            let failed_before = failed_in_previous && !generation_switched;
            let healthy_before = previous
                .as_ref()
                .is_some_and(|state| state.phase == Phase::Healthy.as_str());
            // 第一优先：认领上一代留下的**独立 unit**（换代的正路）。
            // 第二优先：认领裸进程（旧布局 / 回退路径下的兼容）。
            let adopted_unit = adopt_unit(entry);
            let adopted = match &adopted_unit {
                Some((unit, pid)) => {
                    eprintln!(
                        "acb-component: adopted unit {unit} for {} pid={pid} (generation switch left it running)",
                        entry.name
                    );
                    Some(*pid)
                }
                None => adopt_candidate(entry, previous.as_ref()),
            };
            let phase = if !entry.enabled {
                Phase::Stopped
            } else if failed_before {
                Phase::Failed
            } else if adopted.is_some() {
                // 接管来的进程在跑：按 Healthy 起步，由 probe 决定是否真的健康。
                // 置 Starting 会立刻再 spawn 一份，正是要避免的事。
                Phase::Healthy
            } else {
                Phase::Starting
            };

            // 接管判定（ADR-0034）：换代时上一代 launcher 故意不收敛组件，
            // 新 launcher 起来要认领它们，而不是再 spawn 一份。
            //
            // 三条全中才接管，缺一不可：
            //   1. state 里记着 pid
            //   2. 该 pid 还活着
            //   3. `/proc/<pid>/exe` 与本次要启动的 program 是同一个程序
            //      —— pid 会复用，只看「活着」可能认领到毫不相干的进程。
            components.push(Managed {
                unit: adopted_unit.map(|(unit, _)| unit),
                adopted_pid: adopted,
                entry: entry.clone(),
                phase,
                child: None,
                stop_requested: false,
                // 换代清掉的失败终态，其计数也必须一起清零：否则新实例刚
                // 起一次就撞上旧的 max_attempts，立刻又进 Failed。
                attempts: if retry_after_generation_switch {
                    0
                } else {
                    previous.as_ref().map_or(0, |state| state.attempts)
                },
                consecutive_failures: if retry_after_generation_switch {
                    0
                } else {
                    previous
                        .as_ref()
                        .map_or(0, |state| state.consecutive_failures)
                },
                restarts: previous.as_ref().map_or(0, |state| state.restarts),
                probe_failures: 0,
                probe_successes: 0,
                last_probe_at_ms: None,
                next_start_at_ms: now,
                started_at_ms: None,
                last_exit: None,
                last_error: if retry_after_generation_switch {
                    None
                } else {
                    previous.as_ref().and_then(|state| state.last_error.clone())
                },
                last_result: if retry_after_generation_switch {
                    None
                } else {
                    previous
                        .as_ref()
                        .and_then(|state| state.last_result.clone())
                },
                ready_at_ms: previous.as_ref().and_then(|state| state.ready_at_ms),
                failed_at_ms: if retry_after_generation_switch {
                    None
                } else {
                    previous.as_ref().and_then(|state| state.failed_at_ms)
                },
                last_run_at_ms: previous.as_ref().and_then(|state| state.last_run_at_ms),
                // launcher 重启后沿用「上一轮已就绪」的代次 1，避免把恢复当成一次新就绪。
                ready_generation: u64::from(healthy_before),
                ran_dependency_generations: Vec::new(),
                log_path,
                dirty: true,
            });
        }

        // 启动按依赖拓扑序，停止天然逆序。
        let order = startup_order(&components);
        let mut ordered = Vec::with_capacity(components.len());
        let mut slots: Vec<Option<Managed>> = components.into_iter().map(Some).collect();
        for position in order {
            ordered.push(
                slots[position]
                    .take()
                    .expect("startup_order visits every component exactly once"),
            );
        }

        let enabled_count = ordered.iter().filter(|comp| comp.entry.enabled).count();
        let mut manager = Self {
            state_dir: state_dir.to_path_buf(),
            instance_id: instance_id.map(str::to_string),
            config_sha256: config_sha256.to_string(),
            components: ordered,
            enabled_count,
        };
        // 构造时就把 `after` 依赖的当前代次记成「已经跑过」：launcher 重启或
        // 组件恢复时不应把依赖的既有就绪状态当成一次新事件。
        let generations: HashMap<String, u64> = manager
            .components
            .iter()
            .map(|comp| (comp.entry.name.clone(), comp.ready_generation))
            .collect();
        for comp in &mut manager.components {
            if comp.entry.kind != ComponentType::Oneshot || comp.entry.depends.after.is_empty() {
                continue;
            }
            comp.ran_dependency_generations = comp
                .entry
                .depends
                .after
                .iter()
                .filter_map(|name| {
                    generations
                        .get(name)
                        .map(|generation| (name.clone(), *generation))
                })
                .collect();
        }
        manager.flush(now);
        Ok(manager)
    }

    /// launcher 每个 tick 调一次。时间由调用方传入，便于测试推进退避。
    pub fn tick(&mut self, now: i64) {
        if self.components.is_empty() {
            return;
        }
        let snapshot: Vec<Snapshot> = self
            .components
            .iter()
            .map(|comp| Snapshot {
                name: comp.entry.name.clone(),
                phase: comp.phase,
                ready_generation: comp.ready_generation,
            })
            .collect();

        for index in 0..self.components.len() {
            self.tick_one(index, now, &snapshot);
        }

        if self.components.iter().any(|comp| comp.dirty) {
            self.flush(now);
        }
    }

    /// 逆依赖序停止。`oneshot` 按 `stop_action` 收尾；`failed` 保持可见不覆盖。
    pub fn stop_all(&mut self, now: i64) {
        self.stop_all_until(now, None);
    }

    /// 带全局上界的停止：所有组件共享 `deadline`，预算用尽后剩余组件直接
    /// SIGKILL，避免某个不肯退的组件把 launcher 停机卡死。
    pub fn stop_all_within(&mut self, now: i64, deadline: Instant) {
        self.stop_all_until(now, Some(deadline));
    }

    fn stop_all_until(&mut self, now: i64, deadline: Option<Instant>) {
        // 组件是**串行**停的，而每个的 `stop_timeout_secs` 默认 45 秒——比 launcher
        // 的停机总预算（40 秒）还大。只把每步压到「剩余预算」的话，第一个组件
        // 就能吃光全部时间，后面的 bridge 收敛与残留清理一秒都拿不到，最终
        // systemd 在 45 秒处 SIGKILL launcher（2026-11-18 实测两次）。
        //
        // 所以按组件数均分剩余预算：每个组件拿到 fair share，谁也别想独占。
        // 停得快的组件会把没用完的时间留给后面的（每步重算剩余再除）。
        for index in (0..self.components.len()).rev() {
            let fair = deadline.map(|deadline| {
                let remaining = deadline.saturating_duration_since(Instant::now());
                // 还剩几个要停（含当前这个）。
                let count = (0..=index)
                    .filter(|slot| self.components[*slot].phase != Phase::Failed)
                    .count()
                    .max(1) as u32;
                remaining
                    .checked_div(count)
                    .unwrap_or(remaining)
                    .max(Duration::from_secs(1))
            });
            let name = self.components[index].entry.name.clone();
            let started = Instant::now();
            let budget_secs = fair.map(|budget| budget.as_secs()).unwrap_or(0);
            self.stop_one(index, now, fair);
            // 每个组件的实际停止耗时：停机超时时，这行能直接指出是谁吃掉了预算。
            // 只在超过 1 秒时打——正常停止几十毫秒，每条都打会淹掉 journal。
            let elapsed = started.elapsed();
            if elapsed >= Duration::from_secs(1) {
                eprintln!(
                    "acb-component: stop {name} took {}ms (budget {budget_secs}s)",
                    elapsed.as_millis()
                );
            }
        }
        self.flush(now);
    }

    /// 停机时明确要求保留的组件（oneshot + `leave_running`）的 pid。
    ///
    /// launcher 拍后代快照时要把这些子树排除，否则兜底收敛会误杀。
    pub fn leave_running_pids(&self) -> Vec<u32> {
        self.components
            .iter()
            .filter(|comp| {
                comp.entry.kind == ComponentType::Oneshot
                    && comp.entry.stop_action == StopAction::LeaveRunning
            })
            .filter_map(|comp| match &comp.unit {
                Some(unit) => crate::component_unit::state(unit)
                    .map(|state| state.main_pid)
                    .filter(|pid| *pid != 0),
                None => comp.child.as_ref().map(Child::id).or(comp.adopted_pid),
            })
            .collect()
    }

    pub fn phase(&self, name: &str) -> Option<Phase> {
        self.components
            .iter()
            .find(|comp| comp.entry.name == name)
            .map(|comp| comp.phase)
    }

    pub fn pid(&self, name: &str) -> Option<u32> {
        let comp = self
            .components
            .iter()
            .find(|comp| comp.entry.name == name)?;
        if let Some(unit) = &comp.unit {
            return crate::component_unit::state(unit)
                .map(|state| state.main_pid)
                .filter(|pid| *pid != 0);
        }
        comp.child.as_ref().map(Child::id).or(comp.adopted_pid)
    }

    pub fn enabled_count(&self) -> usize {
        self.enabled_count
    }

    fn tick_one(&mut self, index: usize, now: i64, snapshot: &[Snapshot]) {
        if !self.components[index].entry.enabled {
            return;
        }
        let kind = self.components[index].entry.kind;
        let phase = self.components[index].phase;
        if matches!(phase, Phase::Failed | Phase::StoppedEarly | Phase::Stopped) {
            return;
        }

        if phase == Phase::Blocked {
            let after_blocked = self.components[index].entry.kind == ComponentType::Oneshot
                && after_unmet(&self.components[index].entry, snapshot);
            if !requires_unmet(&self.components[index].entry, snapshot) && !after_blocked {
                self.components[index].phase = Phase::Starting;
                self.components[index].next_start_at_ms = now;
                self.components[index].dirty = true;
            }
            self.probe(index, now);
            return;
        }

        // 接管的进程不是我们的子进程，`try_wait` 拿不到它的退出，
        // 只能靠 `/proc` 判活（ADR-0034）。死掉就当作 exited，交给同一套
        // 重启策略处理——接管来的与原生的在后续路径上没有区别。
        // 跑在独立 unit 里的组件：问 systemd 要状态，而不是我们的 `Child`。
        if let Some(unit) = self.components[index].unit.clone() {
            let active = crate::component_unit::state(&unit)
                .map(|state| state.active_state == "active" || state.active_state == "activating")
                .unwrap_or(false);
            if !active {
                self.components[index].unit = None;
                self.components[index].record_failure(format!("unit {unit} is no longer active"));
                self.components[index].phase = Phase::Restarting;
                self.components[index].next_start_at_ms = now;
                return;
            }
        } else if let Some(pid) = self.components[index].adopted_pid {
            if !crate::instance::process_alive(pid) {
                self.components[index].adopted_pid = None;
                self.components[index].record_failure(format!("adopted process {pid} exited"));
                // 走到 restart 分支：phase 置 Restarting 并把重试延后到策略允许。
                self.components[index].phase = Phase::Restarting;
                self.components[index].next_start_at_ms = now;
                return;
            }
        } else {
            // 先收尸：退出语义优先于启动。
            let polled = self.components[index]
                .child
                .as_mut()
                .map(|child| child.try_wait());
            match polled {
                Some(Ok(Some(status))) => self.handle_exit(index, now, status, kind),
                Some(Err(error)) => {
                    self.components[index].record_failure(format!("wait failed: {error}"));
                }
                _ => {}
            }
        }

        // oneshot + after：依赖重新「就绪」（healthy）是一次事件，重跑一次。
        self.trigger_dependency_rerun(index, now, snapshot);

        let phase = self.components[index].phase;
        if matches!(phase, Phase::Starting | Phase::Restarting)
            && now >= self.components[index].next_start_at_ms
        {
            if requires_unmet(&self.components[index].entry, snapshot) {
                self.components[index].phase = Phase::Blocked;
                self.components[index].dirty = true;
                eprintln!(
                    "acb components: {} blocked (requires dependency unavailable)",
                    self.components[index].entry.name
                );
                return;
            }
            // oneshot 的 `after` 依赖必须真的就绪才启动；否则保持 blocked，
            // 不 spawn、不累计 attempts，也就不存在「刷重试」。
            if kind == ComponentType::Oneshot
                && after_unmet(&self.components[index].entry, snapshot)
            {
                self.components[index].phase = Phase::Blocked;
                self.components[index].dirty = true;
                eprintln!(
                    "acb components: {} blocked (after dependency not ready)",
                    self.components[index].entry.name
                );
                return;
            }
            if kind == ComponentType::Oneshot
                && !self.components[index].entry.depends.after.is_empty()
            {
                self.record_after_generations(index, snapshot);
            }
            self.start_one(index, now);
        }

        self.probe(index, now);
    }

    /// 依赖重新就绪的 oneshot 重跑：只有组件已经 `healthy`（这一轮跑完）
    /// 且 `after` 依赖全部重新进入 `healthy`、代次比上次执行时更新时才触发。
    fn trigger_dependency_rerun(&mut self, index: usize, now: i64, snapshot: &[Snapshot]) {
        if self.components[index].entry.kind != ComponentType::Oneshot
            || self.components[index].phase != Phase::Healthy
        {
            return;
        }
        let Some(current) = advanced_after_generations(&self.components[index], snapshot) else {
            return;
        };
        self.components[index].ran_dependency_generations = current;
        self.components[index].phase = Phase::Starting;
        self.components[index].next_start_at_ms = now;
        self.components[index].last_result = Some("dependency ready; rerun".to_string());
        self.components[index].dirty = true;
        eprintln!(
            "acb components: {} rerun (after dependency ready again)",
            self.components[index].entry.name
        );
    }

    fn record_after_generations(&mut self, index: usize, snapshot: &[Snapshot]) {
        let recorded: Vec<(String, u64)> = self.components[index]
            .entry
            .depends
            .after
            .iter()
            .filter_map(|name| {
                current_generation(snapshot, name).map(|generation| (name.clone(), generation))
            })
            .collect();
        self.components[index].ran_dependency_generations = recorded;
    }

    fn start_one(&mut self, index: usize, now: i64) {
        self.components[index].attempts += 1;
        if self.components[index].attempts > 1 {
            self.components[index].restarts += 1;
        }
        let attempts = self.components[index].attempts;
        let name = self.components[index].entry.name.clone();

        match self.spawn(index) {
            Ok(pid) => {
                self.components[index].phase = Phase::Starting;
                self.components[index].started_at_ms = Some(now);
                self.components[index].last_run_at_ms = Some(now);
                self.components[index].probe_failures = 0;
                self.components[index].probe_successes = 0;
                // 第一次探针等一个 interval，避免刚 spawn 就判死。
                self.components[index].last_probe_at_ms = Some(now);
                self.components[index].next_start_at_ms = i64::MAX;
                self.components[index].dirty = true;
                eprintln!("acb components: {name} started pid={pid} attempt={attempts}");
            }
            Err(error) => {
                // 先落错误再回收：spawn 失败也算一次尝试。
                self.components[index].record_failure(error.clone());
                eprintln!("acb components: {name} start failed: {error}");
                self.fail_or_backoff(index, now, "start failed");
            }
        }
    }

    fn handle_exit(&mut self, index: usize, now: i64, status: ExitStatus, kind: ComponentType) {
        let exit = crate::exit_of(status);
        self.components[index].child = None;
        self.components[index].last_exit = Some(exit.clone());
        self.components[index].dirty = true;

        if self.components[index].stop_requested {
            self.components[index].phase = Phase::Stopped;
            self.components[index].last_result = Some("stopped".to_string());
            return;
        }

        let code = exit.code.unwrap_or(-1);
        let name = self.components[index].entry.name.clone();
        if kind == ComponentType::Simple {
            if code == 0 && self.components[index].entry.restart.policy != RestartPolicy::Always {
                // simple 专用：非主动、正常退出，不重启，但状态可见。
                self.components[index].phase = Phase::StoppedEarly;
                self.components[index].last_result = Some("stopped_early".to_string());
                eprintln!("acb components: {name} stopped early (exit 0)");
            } else if code == 0 {
                self.components[index].last_result = Some("exited 0".to_string());
                self.schedule_restart(index, now, "exited 0");
            } else {
                self.components[index].record_failure(format!("exited {code}"));
                eprintln!("acb components: {name} exited {code}");
                self.fail_or_backoff(index, now, "exited non-zero");
            }
            return;
        }

        // oneshot + exit：退出码就是结论，0 即完成，非零按 restart 策略处理。
        if self.components[index].entry.readiness.kind == ReadinessKind::Exit {
            if code == 0 {
                self.mark_healthy(index, now, "succeeded");
                eprintln!("acb components: {name} oneshot succeeded (exit 0)");
            } else {
                self.components[index].record_failure(format!("exited {code}"));
                eprintln!("acb components: {name} oneshot exited {code}");
                self.fail_or_backoff(index, now, "oneshot exited non-zero");
            }
            return;
        }

        // oneshot（tcp/http）：exit 0 只代表“这一次执行成功”，存活仍由探针判定。
        if code == 0 {
            self.components[index].phase = Phase::Starting;
            self.components[index].started_at_ms = Some(now);
            self.components[index].last_result = Some("succeeded".to_string());
            self.components[index].last_run_at_ms = Some(now);
            self.components[index].probe_failures = 0;
            self.components[index].probe_successes = 0;
            self.components[index].last_probe_at_ms = Some(now);
            self.components[index].next_start_at_ms = i64::MAX;
            eprintln!("acb components: {name} oneshot succeeded; waiting for probe");
        } else {
            self.components[index].record_failure(format!("exited {code}"));
            eprintln!("acb components: {name} oneshot exited {code}");
            self.fail_or_backoff(index, now, "oneshot exited non-zero");
        }
    }

    /// 进入 `healthy`：清失败计数并推进就绪代次。依赖它的 oneshot 用代次
    /// 判断「依赖是不是重新就绪」，所以每次进入都必须加一。
    fn mark_healthy(&mut self, index: usize, now: i64, result: &str) {
        self.components[index].phase = Phase::Healthy;
        self.components[index].ready_generation =
            self.components[index].ready_generation.saturating_add(1);
        self.components[index].ready_at_ms = Some(now);
        self.components[index].attempts = 0;
        self.components[index].consecutive_failures = 0;
        self.components[index].last_error = None;
        self.components[index].last_result = Some(result.to_string());
        self.components[index].dirty = true;
    }

    fn fail_or_backoff(&mut self, index: usize, now: i64, reason: &str) {
        let attempts = self.components[index].attempts;
        let max = self.components[index].entry.restart.max_attempts;
        let policy = self.components[index].entry.restart.policy;
        if policy == RestartPolicy::Never || (max > 0 && attempts >= max) {
            self.components[index].phase = Phase::Failed;
            self.components[index].failed_at_ms = Some(now);
            self.components[index].last_result = Some(format!("failed: {reason}"));
            self.components[index].dirty = true;
            eprintln!(
                "acb components: {} failed permanently after {} attempt(s)",
                self.components[index].entry.name, attempts
            );
        } else {
            self.schedule_restart(index, now, reason);
        }
    }

    fn schedule_restart(&mut self, index: usize, now: i64, reason: &str) {
        let delay = backoff_ms(
            &self.components[index].entry,
            self.components[index].attempts,
        );
        self.components[index].phase = Phase::Restarting;
        self.components[index].next_start_at_ms = now + delay;
        self.components[index].last_result = Some(format!("restarting: {reason}"));
        self.components[index].dirty = true;
        eprintln!(
            "acb components: {} restart in {}ms ({reason})",
            self.components[index].entry.name, delay
        );
    }

    fn probe(&mut self, index: usize, now: i64) {
        let phase = self.components[index].phase;
        if !matches!(phase, Phase::Starting | Phase::Healthy | Phase::Unhealthy) {
            return;
        }
        // `exit` 只看退出码：进程结束后没有可探测的端点，也不该判为 unhealthy。
        if self.components[index].entry.readiness.kind == ReadinessKind::Exit {
            return;
        }
        let kind = self.components[index].entry.kind;
        // 进程还没起来就不探测。跑在独立 unit 里的组件没有 `Child`，
        // 只看 `child.is_none()` 会把它当成「没起来」而永远不 probe。
        let running = self.components[index].child.is_some()
            || self.components[index].adopted_pid.is_some()
            || self.components[index].unit.is_some();
        if kind == ComponentType::Simple && !running {
            return;
        }

        let interval_ms = self.components[index]
            .entry
            .readiness
            .interval_secs
            .max(1)
            .saturating_mul(1000) as i64;
        if self.components[index]
            .last_probe_at_ms
            .is_some_and(|last| now - last < interval_ms)
        {
            return;
        }
        self.components[index].last_probe_at_ms = Some(now);

        let Some(started_at) = self.components[index].started_at_ms else {
            return;
        };
        let grace_ms = self.components[index]
            .entry
            .readiness
            .startup_grace_secs
            .saturating_mul(1000) as i64;
        let in_grace = now - started_at < grace_ms;

        if self.probe_ready(index) {
            self.components[index].probe_successes += 1;
            self.components[index].probe_failures = 0;
            let needed = self.components[index]
                .entry
                .readiness
                .success_threshold
                .max(1);
            if self.components[index].phase != Phase::Healthy
                && self.components[index].probe_successes >= needed
            {
                self.mark_healthy(index, now, "healthy");
                eprintln!(
                    "acb components: {} healthy",
                    self.components[index].entry.name
                );
            }
            return;
        }

        self.components[index].probe_successes = 0;
        if in_grace {
            return;
        }
        self.components[index].probe_failures += 1;
        self.components[index].phase = Phase::Unhealthy;
        self.components[index].dirty = true;
        let threshold = self.components[index]
            .entry
            .readiness
            .failures_threshold
            .max(1);
        if self.components[index].probe_failures < threshold {
            return;
        }

        self.components[index].probe_failures = 0;
        self.components[index].record_failure("probe failed");
        eprintln!(
            "acb components: {} probe failed {} times; recycling",
            self.components[index].entry.name, threshold
        );
        self.recycle(index, now);
    }

    /// 探针连续失败后的回收：杀掉当前进程组（若有），再按策略重跑。
    fn recycle(&mut self, index: usize, now: i64) {
        if let Some(mut child) = self.components[index].child.take() {
            terminate_child(
                &mut child,
                &self.components[index].entry,
                &self.components[index].entry.name,
                self.components[index].entry.stop_timeout_secs,
            );
        }
        if self.components[index].entry.kind == ComponentType::Oneshot
            && self.components[index].entry.stop_action == StopAction::Down
        {
            self.run_compose_down(index);
        }
        self.fail_or_backoff(index, now, "probe failed");
    }

    fn probe_ready(&self, index: usize) -> bool {
        let readiness = &self.components[index].entry.readiness;
        match readiness.kind {
            // 组件跑在独立 unit 里时没有 `Child`：判活要看 unit 是否还 active。
            // 只看 `child.is_some()` 会让所有 unit 化的组件永远判不健康。
            ReadinessKind::Process => {
                if let Some(unit) = &self.components[index].unit {
                    crate::component_unit::state(unit)
                        .map(|state| state.active_state == "active")
                        .unwrap_or(false)
                } else {
                    self.components[index].child.is_some()
                        || self.components[index].adopted_pid.is_some()
                }
            }
            ReadinessKind::Tcp => {
                let host = connect_host(&readiness.host);
                let Some(port) = readiness.port else {
                    return false;
                };
                let address = if host.contains(':') {
                    format!("[{host}]:{port}")
                } else {
                    format!("{host}:{port}")
                };
                address
                    .parse()
                    .ok()
                    .map(|address| TcpStream::connect_timeout(&address, PROBE_TIMEOUT).is_ok())
                    .unwrap_or(false)
            }
            ReadinessKind::Http => {
                let url = format!("http://{}{}", readiness.endpoint(), readiness.path);
                crate::http_ok(&url, PROBE_TIMEOUT, None)
            }
            // 问 bridge「这个 harness 现在有没有连着」。
            //
            // 判据是依赖表的 `openConnections`：插件连上时 > 0，卸载后 = 0。
            // 这是**应用层**事实——进程活着、端口在听都不代表插件还在（2026-11-18
            // opencode 掉线 7 小时，tcp 探针全程判健康）。
            //
            // 拿不到 token 或连不上 bridge 时返回 `true`（**不判死**）：
            // 控制台挂掉不该引发组件批量重启，那会把一次控制面故障放大成数据面雪崩。
            ReadinessKind::Bridge => {
                let Some(harness) = readiness.harness.as_deref() else {
                    return true;
                };
                // token 先看环境变量；没有就读 bridge 写的健康令牌文件
                // （`$XDG_RUNTIME_DIR/acb-health.token`）。控制台的 token 是密钥库
                // 里的 secret 引用，环境里通常没有；而那个文件就在同一台机器上，
                // 权限由 runtime dir 保证。
                let token = std::env::var(&readiness.bridge_token_env).ok().or_else(|| {
                    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from)?;
                    std::fs::read_to_string(dir.join("acb-health.token"))
                        .ok()
                        .map(|text| text.trim().to_owned())
                        .filter(|text| !text.is_empty())
                });
                let url = format!(
                    "{}/api/dependencies",
                    readiness.bridge_url.trim_end_matches('/')
                );
                let Some(body) = crate::http_get_body(&url, PROBE_TIMEOUT, token.as_deref()) else {
                    return true;
                };
                let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) else {
                    return true;
                };
                let Some(entry) = json
                    .get("dependencies")
                    .and_then(|value| value.as_array())
                    .and_then(|list| {
                        list.iter()
                            .find(|item| item.get("id").and_then(|id| id.as_str()) == Some(harness))
                    })
                else {
                    // 依赖表里没有这个 harness：可能还没注册，不据此判死。
                    return true;
                };
                let open = entry
                    .get("openConnections")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                open > 0
            }
            // `probe()` 对 exit 直接跳过；这里是穷尽匹配的兜底。
            ReadinessKind::Exit => true,
        }
    }

    fn spawn(&mut self, index: usize) -> Result<u32, String> {
        let entry = &self.components[index].entry;
        let (program, args) = command_for(entry)?;
        let log_path = self.components[index].log_path.clone();
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|error| format!("open log {}: {error}", log_path.display()))?;
        let stderr = log
            .try_clone()
            .map_err(|error| format!("clone log {}: {error}", log_path.display()))?;

        // 优先用独立 unit 起组件（ADR-0034）：组件若留在 acb-bridge 的 cgroup 里，
        // 换代重启 unit 时会被 systemd 一起清掉。
        //
        // `systemd-run` 不可用时回退到直接 spawn——那种情况下换代仍会带走组件，
        // 但组件能跑起来，不能因为隔离机制缺失让整个桥不可用。
        let mut env_pairs: Vec<(String, String)> = Vec::new();

        // 只注入 ACB_COMPONENT_* / ACB_ENV_FILE；密钥值本身不写进组件配置或日志。
        env_pairs.push(("ACB_COMPONENT_NAME".to_owned(), entry.name.clone()));
        env_pairs.push(("ACB_COMPONENT_TYPE".to_owned(), entry.kind.to_string()));
        env_pairs.push((
            "ACB_COMPONENT_STATE_DIR".to_owned(),
            self.state_dir.to_string_lossy().into_owned(),
        ));
        env_pairs.push((
            "ACB_COMPONENT_LOG_PATH".to_owned(),
            log_path.to_string_lossy().into_owned(),
        ));
        env_pairs.push((
            "ACB_COMPONENT_CONFIG_SHA256".to_owned(),
            self.config_sha256.clone(),
        ));
        env_pairs.push((
            "ACB_COMPONENT_STOP_ACTION".to_owned(),
            entry.stop_action.to_string(),
        ));
        if let Some(instance_id) = &self.instance_id {
            env_pairs.push(("ACB_COMPONENT_INSTANCE_ID".to_owned(), instance_id.clone()));
        }
        // `ACB_DATA_DIR` 必须显式传下去（ADR-0034 落地时踩到）。
        //
        // 直接 spawn 的组件继承 launcher 的环境，所以以前不需要写这一行；但
        // `systemd-run` 起的是**独立 unit**，它拿不到 launcher 的环境。少了这个
        // 变量，组件里的 `acbctl` 就按相对路径 `state/acb.db` + 进程 cwd 找库
        // （cwd 是 `$HOME`），于是打开/新建了一个空库：读不到会话与库内密钥，
        // `acbctl sessions` 返回零个会话 → 驱动一个都不起 → dsh 全部失联，
        // 而脚本不会报错（「零个会话」是合法输出）。
        if let Ok(data_dir) = std::env::var("ACB_DATA_DIR") {
            let data_dir = data_dir.trim();
            if !data_dir.is_empty() {
                env_pairs.push(("ACB_DATA_DIR".to_owned(), data_dir.to_owned()));
            }
        }
        if let Some(env_file) = entry
            .process
            .env_file
            .as_deref()
            .filter(|path| !path.is_empty())
        {
            env_pairs.push(("ACB_ENV_FILE".to_owned(), env_file.to_owned()));
        }
        if let Some(file) = entry.file.as_deref().filter(|file| !file.is_empty()) {
            env_pairs.push(("ACB_COMPONENT_FILE".to_owned(), file.to_owned()));
        }
        for (position, value) in entry.compose_bin.iter().enumerate() {
            env_pairs.push((
                format!("ACB_COMPONENT_COMPOSE_BIN_{position}"),
                value.clone(),
            ));
        }
        for (key, value) in &entry.env {
            env_pairs.push((key.clone(), value.clone()));
        }

        // oneshot 不进独立 unit：它的语义是「跑完就退」，而 `systemd-run` 是异步的，
        // 我们会立刻判定「已退出」并反复重跑（实测跑出 10 次）。oneshot 生命周期
        // 短，换代时被带走也不会影响会话，所以保留原来的同步路径。
        let unitizable = entry.kind != ComponentType::Oneshot;
        if unitizable && crate::component_unit::available() {
            let spec = crate::component_unit::Spec {
                name: &entry.name,
                program: &program,
                args: &args,
                cwd: entry.process.cwd.as_deref(),
                env: env_pairs,
                log_path: &log_path,
            };
            let unit = crate::component_unit::start(&spec).map_err(|error| {
                eprintln!("acb-component: {} start failed: {error}", entry.name);
                error
            })?;
            // 独立 unit 拿不到 `Child`：存活判定与停止都另走一条路（见 tick_one / stop_one）。
            let pid = crate::component_unit::wait_main_pid(&unit, Duration::from_secs(5))
                .ok_or_else(|| format!("{unit} started but never became active"))?;
            self.components[index].child = None;
            self.components[index].adopted_pid = None;
            self.components[index].unit = Some(unit);
            return Ok(pid);
        }

        // 回退路径：直接 spawn（换代会带走组件，但没有 systemd-run 时别无选择）。
        let mut command = Command::new(&program);
        command
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        if let Some(cwd) = entry.process.cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
            command.current_dir(cwd);
        }
        for (key, value) in &env_pairs {
            command.env(key, value);
        }

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let child = command
            .spawn()
            .map_err(|error| format!("spawn {program}: {error}"))?;
        let pid = child.id();
        self.components[index].child = Some(child);
        self.components[index].unit = None;
        Ok(pid)
    }

    fn run_compose_down(&mut self, index: usize) {
        let Some((program, args)) = compose_down_command(&self.components[index].entry) else {
            return;
        };
        let log_path = self.components[index].log_path.clone();
        let Ok(log) = OpenOptions::new().create(true).append(true).open(&log_path) else {
            return;
        };
        let Ok(stderr) = log.try_clone() else {
            return;
        };
        let mut command = Command::new(&program);
        command
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        if let Some(cwd) = self.components[index]
            .entry
            .process
            .cwd
            .as_deref()
            .filter(|cwd| !cwd.is_empty())
        {
            command.current_dir(cwd);
        }
        // `compose down` 也要有上限：容器停不下来时它会一直等，而这条路径
        // 在 launcher 主循环里——卡住就是 watchdog SIGABRT。
        let waited = crate::component_unit::run_with_deadline(
            &mut command,
            std::time::Duration::from_secs(30),
        );
        match waited.map(|output| output.status) {
            Ok(status) if status.success() => eprintln!(
                "acb components: {} compose down ok",
                self.components[index].entry.name
            ),
            Ok(status) => eprintln!(
                "acb components: {} compose down exited {:?}",
                self.components[index].entry.name,
                status.code()
            ),
            Err(error) => eprintln!(
                "acb components: {} compose down failed: {error}",
                self.components[index].entry.name
            ),
        }
    }

    /// `budget` = 这一段的预算（已由调用方按剩余组件数均分），不是全局 deadline。
    fn stop_one(&mut self, index: usize, _now: i64, budget: Option<Duration>) {
        if self.components[index].phase == Phase::Failed {
            return;
        }
        self.components[index].stop_requested = true;
        // 独立 unit 最干净：交给 systemctl 停，它会按 unit 的 KillMode 收敛整组。
        if let Some(unit) = self.components[index].unit.take() {
            let configured = self.components[index].entry.stop_timeout_secs;
            let timeout = match budget {
                Some(budget) => budget.as_secs().clamp(1, configured),
                None => configured,
            };
            if let Err(error) = crate::component_unit::stop(&unit, timeout) {
                eprintln!(
                    "acb-component: cannot stop unit {unit} for {}: {error}",
                    self.components[index].entry.name
                );
            }
        }
        // 接管的进程不是我们的子进程，`Child` 那边是空的；必须单独按 pid 停，
        // 否则 `systemctl stop` 之后它还活着（ADR-0034 的验证项 2 就靠这条）。
        if let Some(pid) = self.components[index].adopted_pid.take() {
            let token = crate::instance::process_start_token(pid);
            let configured = self.components[index].entry.stop_timeout_secs;
            let timeout = match budget {
                Some(budget) => budget.as_secs().clamp(1, configured),
                None => configured,
            };
            let stop_deadline = Instant::now() + Duration::from_secs(timeout);
            if let Err(error) =
                crate::instance::terminate_within(pid, token.as_deref(), stop_deadline)
            {
                eprintln!(
                    "acb-component: cannot stop adopted component {} pid={pid}: {error}",
                    self.components[index].entry.name
                );
            }
        }
        if let Some(mut child) = self.components[index].child.take() {
            let configured = self.components[index].entry.stop_timeout_secs;
            let timeout = match budget {
                Some(budget) => budget.as_secs().clamp(1, configured),
                None => configured,
            };
            let exit = terminate_child(
                &mut child,
                &self.components[index].entry,
                &self.components[index].entry.name,
                timeout,
            );
            self.components[index].last_exit = exit;
        }
        if self.components[index].entry.kind == ComponentType::Oneshot
            && self.components[index].entry.stop_action == StopAction::Down
        {
            self.run_compose_down(index);
        }
        self.components[index].phase = Phase::Stopped;
        self.components[index].last_result = Some("stopped".to_string());
        self.components[index].dirty = true;
    }

    fn flush(&mut self, now: i64) {
        let instance_id = self.instance_id.clone();
        let config_sha256 = self.config_sha256.clone();
        let state_dir = self.state_dir.clone();
        for comp in &mut self.components {
            if !comp.dirty {
                continue;
            }
            let state = build_state(comp, instance_id.as_deref(), &config_sha256, now);
            if let Err(error) = component_state::write_state(&state_dir, &state) {
                eprintln!(
                    "acb components: write state for {}: {error}",
                    comp.entry.name
                );
            }
            comp.dirty = false;
        }
    }
}

impl Drop for ComponentManager {
    fn drop(&mut self) {
        // 兜底只在 launcher 非正常返回时生效。`leave_running` 的 oneshot
        // 由使用者明确要求“别动它”，这里必须尊重。
        for comp in &mut self.components {
            if comp.entry.kind == ComponentType::Oneshot
                && comp.entry.stop_action == StopAction::LeaveRunning
            {
                continue;
            }
            if let Some(mut child) = comp.child.take() {
                let _ = signal_group(child.id(), comp.entry.process.kill_mode, libc::SIGKILL);
                let _ = child.wait();
            }
        }
    }
}

fn effective_entry<'a>(table: &'a ComponentTable, name: &str) -> &'a ResolvedComponent {
    table
        .components
        .iter()
        .find(|entry| entry.name == name && entry.enabled)
        .or_else(|| table.components.iter().find(|entry| entry.name == name))
        .expect("name was collected from the table")
}

fn startup_order(components: &[Managed]) -> Vec<usize> {
    let by_name: HashMap<&str, usize> = components
        .iter()
        .enumerate()
        .map(|(index, comp)| (comp.entry.name.as_str(), index))
        .collect();
    let mut visited = vec![0u8; components.len()];
    let mut order = Vec::with_capacity(components.len());
    for index in 0..components.len() {
        visit(index, components, &by_name, &mut visited, &mut order);
    }
    order
}

fn visit(
    index: usize,
    components: &[Managed],
    by_name: &HashMap<&str, usize>,
    visited: &mut [u8],
    order: &mut Vec<usize>,
) {
    if visited[index] == 2 {
        return;
    }
    if visited[index] == 1 {
        // 解析期已经拒绝环；这里防御性跳过。
        return;
    }
    visited[index] = 1;
    let depends = &components[index].entry.depends;
    for name in depends
        .after
        .iter()
        .chain(&depends.requires)
        .chain(&depends.wants)
        .chain(&depends.part_of)
    {
        if let Some(&dependency) = by_name.get(name.as_str()) {
            visit(dependency, components, by_name, visited, order);
        }
    }
    visited[index] = 2;
    order.push(index);
}

fn requires_unmet(entry: &ResolvedComponent, snapshot: &[Snapshot]) -> bool {
    entry.depends.requires.iter().any(|dependency| {
        snapshot
            .iter()
            .find(|snap| &snap.name == dependency)
            .map(|snap| {
                matches!(
                    snap.phase,
                    Phase::Failed | Phase::Blocked | Phase::Stopped | Phase::StoppedEarly
                )
            })
            .unwrap_or(true)
    })
}

/// oneshot 的 `after` 依赖是否还没就绪：必须进入 `healthy`，光「进程刚起来」
/// 不算。缺失的依赖按未就绪处理（安全失败，不猜）。
fn after_unmet(entry: &ResolvedComponent, snapshot: &[Snapshot]) -> bool {
    entry.depends.after.iter().any(|dependency| {
        snapshot
            .iter()
            .find(|snap| &snap.name == dependency)
            .map(|snap| snap.phase != Phase::Healthy)
            .unwrap_or(true)
    })
}

fn current_generation(snapshot: &[Snapshot], name: &str) -> Option<u64> {
    snapshot
        .iter()
        .find(|snap| snap.name == name)
        .map(|snap| snap.ready_generation)
}

/// `after` 依赖全部就绪、且其中至少一个的代次比上次执行时更新时，返回当前代次快照。
fn advanced_after_generations(comp: &Managed, snapshot: &[Snapshot]) -> Option<Vec<(String, u64)>> {
    let mut current = Vec::with_capacity(comp.entry.depends.after.len());
    for name in &comp.entry.depends.after {
        let snap = snapshot.iter().find(|snap| &snap.name == name)?;
        if snap.phase != Phase::Healthy {
            return None;
        }
        current.push((name.clone(), snap.ready_generation));
    }
    let advanced = current.iter().any(|(name, generation)| {
        comp.ran_dependency_generations
            .iter()
            .find(|(recorded, _)| recorded == name)
            .map(|(_, recorded_generation)| generation > recorded_generation)
            .unwrap_or(false)
    });
    advanced.then_some(current)
}

fn command_for(entry: &ResolvedComponent) -> Result<(String, Vec<String>), String> {
    if let Some(command) = entry.command.as_ref() {
        let mut args = command.iter().skip(1).cloned().collect::<Vec<_>>();
        args.extend(entry.args.iter().cloned());
        return Ok((command[0].clone(), args));
    }
    if entry.kind == ComponentType::Oneshot && !entry.compose_bin.is_empty() {
        let file = entry
            .file
            .as_deref()
            .filter(|file| !file.is_empty())
            .ok_or_else(|| "oneshot 组件缺少 file".to_string())?;
        let mut args = entry
            .compose_bin
            .iter()
            .skip(1)
            .cloned()
            .collect::<Vec<_>>();
        // `--env-file` 必须显式传：`podman-compose` 只读 **compose 同目录**的 `.env`
        // 这个文件名。compose 文件在 `deploy/` 而 env 真值叫 `snowluma.env`，
        // 不传就静默退回默认端口（3000/3001），把 ACB 切断——不再靠
        // `deploy/.env` 软链接这种隐式兼容。
        if let Some(env_file) = entry
            .process
            .env_file
            .as_deref()
            .filter(|path| !path.is_empty())
        {
            args.push("--env-file".to_string());
            args.push(env_file.to_string());
        }
        args.push("-f".to_string());
        args.push(file.to_string());
        args.push("up".to_string());
        args.push("-d".to_string());
        // 显式重建：容器配置没变时 `up -d` 什么都不做，镜像换了也不会生效。
        // 具名卷不受影响——重建只换容器，卷还是那些卷。
        if entry.compose_recreate {
            args.push("--force-recreate".to_string());
        }
        return Ok((entry.compose_bin[0].clone(), args));
    }
    if let Some(script) = entry.script.as_deref().filter(|script| !script.is_empty()) {
        return Ok((script.to_string(), entry.args.clone()));
    }
    Err("组件既没有 command 也没有预设脚本".to_string())
}

fn compose_down_command(entry: &ResolvedComponent) -> Option<(String, Vec<String>)> {
    if entry.kind != ComponentType::Oneshot
        || entry.stop_action != StopAction::Down
        || entry.compose_bin.is_empty()
    {
        return None;
    }
    let file = entry.file.as_deref().filter(|file| !file.is_empty())?;
    let mut args = entry
        .compose_bin
        .iter()
        .skip(1)
        .cloned()
        .collect::<Vec<_>>();
    args.push("-f".to_string());
    args.push(file.to_string());
    args.push("down".to_string());
    Some((entry.compose_bin[0].clone(), args))
}

fn backoff_ms(entry: &ResolvedComponent, attempts: u32) -> i64 {
    let configured = &entry.restart.backoff_secs;
    let ladder: &[u64] = if configured.is_empty() {
        &DEFAULT_BACKOFF_SECS
    } else {
        configured
    };
    let position = attempts.saturating_sub(1) as usize;
    let secs = ladder
        .get(position)
        .copied()
        .or_else(|| ladder.last().copied())
        .unwrap_or(1);
    let cap = if entry.restart.backoff_cap_secs == 0 {
        60
    } else {
        entry.restart.backoff_cap_secs
    };
    secs.min(cap) as i64 * 1000
}

fn connect_host(host: &str) -> &str {
    match host {
        "0.0.0.0" => "127.0.0.1",
        "::" => "::1",
        other => other,
    }
}

fn terminate_child(
    child: &mut Child,
    entry: &ResolvedComponent,
    name: &str,
    timeout_secs: u64,
) -> Option<ExitInfo> {
    let pid = child.id();
    let _ = signal_group(pid, entry.process.kill_mode, libc::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(crate::exit_of(status)),
            Ok(None) => {}
            Err(_) => break,
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let target = match entry.process.kill_mode {
        KillMode::Group => format!("process group {pid}"),
        KillMode::Process => format!("pid {pid}"),
    };
    eprintln!(
        "acb components: {name} {target} did not exit within {timeout_secs}s; sending SIGKILL"
    );
    let _ = signal_group(pid, entry.process.kill_mode, libc::SIGKILL);
    child.wait().ok().map(|status| crate::exit_of(status))
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn signal_group(pid: u32, kill_mode: KillMode, signal: libc::c_int) -> Result<(), String> {
    let target = match kill_mode {
        KillMode::Group => -(pid as libc::pid_t),
        KillMode::Process => pid as libc::pid_t,
    };
    // SAFETY: 只向已知子进程/其进程组发送信号，不读写共享内存。
    let result = unsafe { libc::kill(target, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!("kill({target}, {signal}) failed"))
    }
}

#[cfg(not(unix))]
fn signal_group(_pid: u32, _kill_mode: KillMode, _signal: i32) -> Result<(), String> {
    Err("signals are unsupported on this platform".to_string())
}

fn build_state(
    comp: &Managed,
    instance_id: Option<&str>,
    config_sha256: &str,
    now: i64,
) -> ComponentState {
    ComponentState {
        schema: STATE_SCHEMA,
        name: comp.entry.name.clone(),
        phase: comp.phase.as_str().to_string(),
        restart_policy: comp.entry.restart.policy.to_string(),
        enabled: comp.entry.enabled,
        attempts: comp.attempts,
        consecutive_failures: comp.consecutive_failures,
        restarts: comp.restarts,
        // 接管的进程没有 `Child` 句柄，pid 单独记在 `adopted_pid` 里；
        // 落盘时两者合一，`acbctl component status` 才看得到真实 pid。
        pid: comp.child.as_ref().map(Child::id).or(comp.adopted_pid),
        instance_id: instance_id.map(str::to_string),
        config_sha256: config_sha256.to_string(),
        last_error: comp.last_error.clone(),
        ready_at_ms: comp.ready_at_ms,
        failed_at_ms: comp.failed_at_ms,
        log_path: Some(comp.log_path.to_string_lossy().into_owned()),
        last_run_at_ms: comp.last_run_at_ms,
        last_result: comp.last_result.clone(),
        probe_url: component_state::probe_url(&comp.entry),
        updated_at_ms: now,
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;

    fn compose_entry(recreate: bool) -> ResolvedComponent {
        // 最小的 container 预设：组件只需声明 preset，不用写 command。
        let presets = crate::component::PresetRegistry::from_toml(
            r#"
[[preset]]
id = "test_container"
component = "snow"
type = "oneshot"
source = "container"
script = "/bin/true"
"#,
        )
        .expect("presets");
        let text = format!(
            r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "snow"
enabled = true
type = "oneshot"
preset = "test_container"
file = "/tmp/x.yml"
compose_bin = ["podman-compose"]
compose_recreate = {recreate}
[component.readiness]
kind = "tcp"
port = 1
"#
        );
        let (_file, table) =
            crate::component::parse_config_file(&text, &presets, std::path::Path::new("/tmp"))
                .expect("配置合法");
        table
            .components
            .into_iter()
            .find(|entry| entry.name == "snow")
            .expect("有 snow")
    }

    fn compose_entry_with_env(recreate: bool) -> ResolvedComponent {
        let presets = crate::component::PresetRegistry::from_toml(
            r#"
[[preset]]
id = "test_container"
component = "snow"
type = "oneshot"
source = "container"
script = "/bin/true"
"#,
        )
        .expect("presets");
        let text = format!(
            r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "snow"
enabled = true
type = "oneshot"
preset = "test_container"
file = "/tmp/x.yml"
compose_bin = ["podman-compose"]
compose_recreate = {recreate}
[component.process]
env_file = "/tmp/snow.env"
[component.readiness]
kind = "tcp"
port = 1
"#
        );
        let (_file, table) =
            crate::component::parse_config_file(&text, &presets, std::path::Path::new("/tmp"))
                .expect("配置合法");
        table
            .components
            .into_iter()
            .find(|entry| entry.name == "snow")
            .expect("有 snow")
    }

    /// 配了 `env_file` 就必须显式传给 compose。
    ///
    /// `podman-compose` 只读 **compose 同目录**的 `.env` 这个名字。compose 文件
    /// 在 `deploy/`、真值叫 `snowluma.env`，不显式传就会**静默退回默认端口**
    /// （3000/3001）把 ACB 切断。以前靠 `deploy/.env` 软链接兜着，那种隐式兼容
    /// 已废弃。
    #[test]
    fn compose_passes_env_file_explicitly() {
        let (program, args) = command_for(&compose_entry_with_env(false)).expect("能拼");
        assert_eq!(program, "podman-compose");
        let joined = args.join(" ");
        assert!(
            joined.contains("--env-file /tmp/snow.env"),
            "要显式传 env 文件，实际：{args:?}"
        );
    }

    /// 没配 `env_file` 时不加这个参数（别凭空造路径）。
    #[test]
    fn compose_omits_env_file_when_unset() {
        let (_program, args) = command_for(&compose_entry(false)).expect("能拼");
        assert!(
            !args.iter().any(|arg| arg == "--env-file"),
            "没配就不该传，实际：{args:?}"
        );
    }

    /// 打开 `compose_recreate` 时，`up -d` 后面必须带 `--force-recreate`。
    ///
    /// 为什么需要它：容器配置没变时 `up -d` 什么都不做，镜像是 `latest` 也不会
    /// 换——「重启组件」看着成功了，实际跑的还是旧容器。
    #[test]
    fn compose_recreate_adds_force_recreate() {
        let (_program, args) = command_for(&compose_entry(true)).expect("能拼");
        assert!(
            args.contains(&"--force-recreate".to_string()),
            "要显式重建，实际参数：{args:?}"
        );
    }

    /// 默认不带：`up -d` 本身是幂等的，没必要每次换一个新容器。
    #[test]
    fn compose_recreate_is_off_by_default() {
        let (_program, args) = command_for(&compose_entry(false)).expect("能拼");
        assert!(
            !args.contains(&"--force-recreate".to_string()),
            "默认不该重建，实际参数：{args:?}"
        );
    }
}
