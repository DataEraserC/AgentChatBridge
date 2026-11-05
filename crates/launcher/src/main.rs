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

use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use acb_supervisor::{
    AlwaysHealthy, CommandSpec, EXIT_GIVE_UP, EXIT_OK, HttpProbe, Parts, ProcessRunner,
    RollbackMode, Supervisor, SystemClock, SystemSleeper, build_policy, load_config, load_current,
    now_ms, save_current,
};

fn main() -> ExitCode {
    let Some(path) = config_path() else {
        eprintln!("usage: acb-launcher --config /path/to/supervisor.toml");
        return ExitCode::from(2);
    };
    let mut config = match load_config(&path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("acb-launcher: {error}");
            return ExitCode::from(EXIT_GIVE_UP as u8);
        }
    };

    let state_dir = PathBuf::from(&config.state_dir);
    if let Err(error) = std::fs::create_dir_all(&state_dir) {
        eprintln!(
            "acb-launcher: cannot create state dir {}: {error}",
            state_dir.display()
        );
        return ExitCode::from(EXIT_GIVE_UP as u8);
    }
    let configured = CommandSpec {
        program: config.program.clone(),
        args: config.args.clone(),
    };
    let current = match load_current(&state_dir) {
        Some(current) => {
            if current != configured {
                eprintln!(
                    "acb-launcher: current.json selects {} (config says {})",
                    current.id(),
                    configured.id()
                );
            }
            current
        }
        None => configured,
    };
    if let Err(error) = save_current(&state_dir, &current, now_ms()) {
        eprintln!("acb-launcher: cannot write current.json: {error}");
        return ExitCode::from(EXIT_GIVE_UP as u8);
    }
    config.program = current.program.clone();
    config.args = current.args.clone();
    // systemd 重启由 launcher 请求；策略本身不直接起 LKG。
    config.rollback_mode = RollbackMode::External;

    let policy = match build_policy(&config) {
        Ok(policy) => policy,
        Err(error) => {
            eprintln!("acb-launcher: {error}");
            return ExitCode::from(EXIT_GIVE_UP as u8);
        }
    };
    let probe: Box<dyn acb_supervisor::Probe> = match &config.health_url {
        Some(url) => Box::new(HttpProbe::new(url)),
        None => Box::new(AlwaysHealthy),
    };
    let mut supervisor = match Supervisor::new(
        config,
        Parts {
            clock: Box::new(SystemClock),
            runner: Box::new(ProcessRunner::default()),
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

    let mut notifier = Notifier::new();
    notifier.ready();
    eprintln!("acb-launcher: started {}", current.id());
    shutdown::install();

    while !supervisor.stopped() {
        if shutdown::requested() {
            // systemd stop：SIGTERM 已发给同一 cgroup，子进程自己排空；这里只按契约退出 0。
            // 手动运行（没有 NOTIFY_SOCKET）时没有人替我们停子进程，自己收掉。
            if std::env::var_os("NOTIFY_SOCKET").is_none() {
                supervisor.shutdown();
            }
            notifier.stopping();
            return ExitCode::from(EXIT_OK as u8);
        }
        supervisor.tick();
        notifier.watchdog();
        thread::sleep(Duration::from_millis(100));
    }

    if let Some(reason) = supervisor.failed() {
        eprintln!("acb-launcher: stopped: {reason}");
    }
    notifier.stopping();
    ExitCode::from(supervisor.exit_code().unwrap_or(EXIT_OK) as u8)
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
