//! 插件动作：manifest 里声明的显式操作，经同一套收口执行。
//!
//! 动作和工具的区别只在入口：工具是模型调用，动作是人在控制台点。执行仍然
//! 走插件进程、仍然受 `plugin` 能力门禁与超时/重启策略约束。
//!
//! 这里只定义载体接口；具体实现由各自插件载体提供（外置进程、进程内示例）。

use serde_json::Value;

/// 一个能执行动作的插件。按 `plugin_id` 注册，控制台按 id 找。
pub trait PluginActionRunner: Send + Sync {
    fn plugin_id(&self) -> &str;

    /// 执行动作。失败返回一句话，调用方转成 400/502，不 panic。
    fn run(&self, action: &str, args: Value) -> Result<Value, String>;
}
