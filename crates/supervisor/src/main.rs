//! `acb-supervisor`：独立进程监督器。
//!
//! systemd（或其它进程管理器）只负责拉起和重启本进程；版本切换/回滚由本进程
//! 根据策略决定并执行。配置见 `config.example.toml` 的 `[supervisor]` 段。

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use acb_supervisor::{
    BuiltinPolicy, CommandPolicy, Config, HttpProbe, Parts, ProcessRunner, Supervisor, SystemClock,
    SystemSleeper, load_config,
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
            return ExitCode::from(78);
        }
    };

    let policy = match build_policy(&config) {
        Ok(policy) => policy,
        Err(error) => {
            eprintln!("acb-supervisor: {error}");
            return ExitCode::from(78);
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
            return ExitCode::from(78);
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

fn build_policy(config: &Config) -> Result<Arc<dyn acb_supervisor::Policy>, String> {
    match config.policy.provider.as_str() {
        "acb.supervisor.default" | "default" => Ok(Arc::new(BuiltinPolicy)),
        "command" => {
            let command = config
                .policy
                .command
                .as_deref()
                .map(str::trim)
                .filter(|command| !command.is_empty())
                .ok_or_else(|| "policy.provider=command requires policy.command".to_owned())?;
            Ok(Arc::new(CommandPolicy::new(command)))
        }
        other => Err(format!("unknown supervisor policy provider: {other}")),
    }
}

struct AlwaysHealthy;

impl acb_supervisor::Probe for AlwaysHealthy {
    fn healthy(&mut self) -> bool {
        true
    }
}
