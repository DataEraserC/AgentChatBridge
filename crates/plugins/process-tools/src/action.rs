//! 把外置进程 provider 包成动作执行器：同一队列、同一超时与重启策略。

use std::sync::Arc;

use acb_kernel::PluginActionRunner;
use serde_json::Value;

use crate::provider::ProcessToolProvider;

pub struct ProcessActionRunner {
    id: String,
    provider: Arc<ProcessToolProvider>,
}

impl ProcessActionRunner {
    pub fn new(id: String, provider: Arc<ProcessToolProvider>) -> Self {
        Self { id, provider }
    }
}

impl PluginActionRunner for ProcessActionRunner {
    fn plugin_id(&self) -> &str {
        &self.id
    }

    fn run(&self, action: &str, args: Value) -> Result<Value, String> {
        self.provider
            .run_action(action, args)
            .map_err(|error| format!("{error:?}"))
    }
}
