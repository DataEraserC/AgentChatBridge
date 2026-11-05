//! `acb-supervisor`：独立进程监督器。
//!
//! systemd（或其它进程管理器）只负责拉起和重启本进程；版本切换/回滚由本进程
//! 根据策略决定并执行。配置见 `config.example.toml` 的 `[supervisor]` 段。

use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use acb_supervisor::{
    AlwaysHealthy, HttpProbe, Parts, ProcessRunner, Supervisor, SystemClock, SystemSleeper,
    build_policy, load_config,
};

fn main() -> ExitCode {
    let Some(path) = config_path() else {
        eprintln!("usage: acb-supervisor --config /path/to/supervisor.toml");
        return ExitCode::from(2);
    };
    let config = match load_config(&path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("acb-supervisor: {error}");
            return ExitCode::from(acb_supervisor::EXIT_GIVE_UP as u8);
        }
    };

    let policy = match build_policy(&config) {
        Ok(policy) => policy,
        Err(error) => {
            eprintln!("acb-supervisor: {error}");
            return ExitCode::from(acb_supervisor::EXIT_GIVE_UP as u8);
        }
    };
    let probe: Box<dyn acb_supervisor::Probe> = match &config.health_url {
        Some(url) => Box::new(HttpProbe::new(url)),
        None => Box::new(AlwaysHealthy),
    };
    let supervisor = Supervisor::new(
        config,
        Parts {
            clock: Box::new(SystemClock),
            runner: Box::new(ProcessRunner::default()),
            probe,
            sleeper: Box::new(SystemSleeper),
            policy,
        },
    );
    let mut supervisor = match supervisor {
        Ok(supervisor) => supervisor,
        Err(error) => {
            eprintln!("acb-supervisor: {error}");
            return ExitCode::from(acb_supervisor::EXIT_GIVE_UP as u8);
        }
    };

    let mut reported_failure = false;
    loop {
        supervisor.tick();
        if !reported_failure {
            if let Some(reason) = supervisor.failed() {
                eprintln!("acb-supervisor: failed: {reason}; staying alive for inspection");
                reported_failure = true;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
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
