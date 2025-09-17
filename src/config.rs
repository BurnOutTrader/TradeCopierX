
use anyhow::{Context, Result};
use std::env;
use crate::models::AuthMode;

#[derive(Clone, Debug)]
pub struct Config {
    pub src_api_base: String,
    pub dest_api_base: String,

    pub src_auth: AuthMode,
    pub dest_auth: AuthMode,

    pub source_account: String,    // id or name (leader)
    pub dest_accounts: Vec<String>,// comma separated (followers)

    pub max_resync_step: i32,      // clamp per correction
    pub source_poll_ms: u64,       // leader polling cadence for Position/searchOpen
    pub enable_follower_drift_check: bool, // optional follower drift polling
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let src_api_base = env_var("SRC_API_BASE")?;
        let dest_api_base = env_var("DEST_API_BASE")?;

        let src_username = env_var("SRC_USERNAME")?;
        let src_api_key = env_var("SRC_API_KEY")?;
        let dest_username = env_var("DEST_USERNAME")?;
        let dest_api_key = env_var("DEST_API_KEY")?;

        let source_account = env_var("SOURCE_ACCOUNT")?;
        let dest_accounts = env_var("DEST_ACCOUNTS")?
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();

        // You asked for 1.5s cadence to leave capacity for orders
        let source_poll_ms = env::var("SOURCE_POLL_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1500);

        let max_resync_step = env::var("PX_MAX_RESYNC")
            .ok()
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(i32::MAX);

        let enable_follower_drift_check = env::var("FOLLOWER_DRIFT_CHECK")
            .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
            .unwrap_or(false); // default OFF to save rate budget

        Ok(Self {
            src_api_base,
            dest_api_base,
            src_auth: AuthMode::ApiKey { username: src_username, api_key: src_api_key },
            dest_auth: AuthMode::ApiKey { username: dest_username, api_key: dest_api_key },
            source_account,
            dest_accounts,
            max_resync_step,
            source_poll_ms,
            enable_follower_drift_check,
        })
    }
}

fn env_var(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("missing env {}", name))
}