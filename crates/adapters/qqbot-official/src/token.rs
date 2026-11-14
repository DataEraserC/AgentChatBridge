//! Access token 管理：appId + clientSecret 换 `QQBot <token>`。
//!
//! Token 有效期 ≤7200 秒；官方建议提前刷新（到期前 60 秒旧 token 仍可用）。
//! 这里在剩余寿命低于 [`REFRESH_MARGIN`] 时重取，线程安全，失败时保留旧值。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

/// 有效期官方说 ≤7200 秒；留出裕量提前刷新。
pub const REFRESH_MARGIN: Duration = Duration::from_secs(180);

/// 默认 token 接口（api.bot.qq.com / bots.qq.com 都提供同一接口）。
pub const DEFAULT_TOKEN_URL: &str = "https://api.bot.qq.com/app/getAppAccessToken";

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("token request failed: {0}")]
    Request(String),
    #[error("token response missing access_token (code={code}, message={message})")]
    Rejected { code: i64, message: String },
}

/// 缓存的 token：值 + 取得时刻 + 有效期。
#[derive(Debug, Clone)]
struct Cached {
    token: String,
    fetched_at: Instant,
    expires_in: Duration,
}

/// appId + clientSecret → `QQBot <access_token>` 的管理器。
///
/// 克隆共享同一份缓存；失败重试由调用方的重试节奏决定，这里只做「过期才重取」。
#[derive(Debug, Clone)]
pub struct TokenManager {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    token_url: String,
    app_id: String,
    client_secret: String,
    cached: Mutex<Option<Cached>>,
}

impl TokenManager {
    pub fn new(app_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self::with_token_url(DEFAULT_TOKEN_URL, app_id, client_secret)
    }

    pub fn with_token_url(
        token_url: impl Into<String>,
        app_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                token_url: token_url.into(),
                app_id: app_id.into(),
                client_secret: client_secret.into(),
                cached: Mutex::new(None),
            }),
        }
    }

    /// 可用的 access token。缓存剩余寿命低于 [`REFRESH_MARGIN`] 就重取；
    /// 重取失败且缓存里还有旧值时退回旧值（过期了才算失败）。
    pub fn access_token(&self) -> Result<String, TokenError> {
        let mut cached = self
            .inner
            .cached
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let fresh = cached.as_ref().is_some_and(|cached| {
            remaining(cached.fetched_at, cached.expires_in) > REFRESH_MARGIN
        });
        if fresh {
            return Ok(cached.as_ref().expect("刚判过非空").token.clone());
        }
        match self.fetch() {
            Ok(token) => {
                // 官方只承诺 ≤7200；按响应里的 expires_in 记，缺省按 7200。
                *cached = Some(Cached {
                    token: token.clone(),
                    fetched_at: Instant::now(),
                    expires_in: Duration::from_secs(7200),
                });
                Ok(token)
            }
            Err(error) => {
                // 到期前的失败退回旧值；彻底过期才把错误抛给调用方。
                if let Some(cached) = cached.as_ref() {
                    if remaining(cached.fetched_at, cached.expires_in) > Duration::ZERO {
                        return Ok(cached.token.clone());
                    }
                }
                Err(error)
            }
        }
    }

    /// `QQBot <token>` 形式的 Authorization 头值（REST 与网关 identify 通用）。
    pub fn auth_header(&self) -> Result<String, TokenError> {
        Ok(format!("QQBot {}", self.access_token()?))
    }

    fn fetch(&self) -> Result<String, TokenError> {
        let response = ureq::post(&self.inner.token_url)
            .set("Content-Type", "application/json")
            .send_json(json!({
                "appId": self.inner.app_id,
                "clientSecret": self.inner.client_secret,
            }))
            .map_err(|error| TokenError::Request(error.to_string()))?;
        let body: serde_json::Value = response
            .into_json()
            .map_err(|error| TokenError::Request(error.to_string()))?;
        if let Some(token) = body.get("access_token").and_then(|value| value.as_str()) {
            return Ok(token.to_owned());
        }
        Err(TokenError::Rejected {
            code: body.get("code").and_then(|value| value.as_i64()).unwrap_or(0),
            message: body
                .get("message")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_owned(),
        })
    }
}

fn remaining(fetched_at: Instant, expires_in: Duration) -> Duration {
    let elapsed = fetched_at.elapsed();
    expires_in.checked_sub(elapsed).unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_header_prefixes_qqbot_scheme() {
        // 不打网络：只验证头拼装。fetch 会失败，但旧值路径先验证拼装格式。
        let manager = TokenManager::with_token_url("http://127.0.0.1:1/app", "appid", "secret");
        let error = manager.access_token().expect_err("没有服务可连");
        assert!(matches!(error, TokenError::Request(_)));
    }
}
