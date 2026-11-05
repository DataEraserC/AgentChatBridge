//! 外置进程 provider 的 stdout 行协议。
//!
//! 第一行必须是 manifest；之后每条 `decide` 请求回一条 `reply`。
//! 请求方向直接复用 core 的 `CoreMessage`（`type = "decide"`），避免再造一套
//! 与 `acb-adapter-api` 平行的形状。

use acb_adapter_api::WirePluginMetadata;
use acb_adapter_api::WireResult;
use serde::Deserialize;

/// 子进程启动后必须发的第一行。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ProviderManifestLine {
    pub provider: String,
    pub extension: String,
    pub api_version: u32,
    #[serde(default)]
    pub metadata: WirePluginMetadata,
}

/// 子进程 stdout 上的后续消息。
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostLine {
    /// 重复的 manifest：忽略并记日志。
    ProviderManifest {
        provider: String,
        extension: String,
        api_version: u32,
        #[serde(default)]
        metadata: WirePluginMetadata,
    },
    Reply {
        id: u64,
        result: WireResult,
    },
    Log {
        #[serde(default)]
        level: String,
        message: String,
    },
}

impl HostLine {
    /// 解析第一行；不是 manifest 就报错。
    pub fn first_manifest(line: &str) -> Result<ProviderManifestLine, String> {
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|error| format!("invalid manifest JSON: {error}"))?;
        if value.get("type").and_then(|value| value.as_str()) != Some("provider_manifest") {
            return Err("first line must be a provider_manifest".to_owned());
        }
        serde_json::from_value(value).map_err(|error| format!("invalid manifest shape: {error}"))
    }
}
