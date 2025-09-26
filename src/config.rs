use anyhow::{anyhow, Context, Result};
use std::env;
use crate::models::AuthMode;

#[derive(Clone, Debug)]
pub struct Config {
    pub src_api_base: String,

    pub src_auth: AuthMode,

    pub dest_api_bases: Vec<String>,
    pub dest_usernames: Vec<String>,
    pub dest_api_keys: Vec<String>,

    pub source_account: String,           // id or name (leader)
    pub dest_accounts: Vec<String>,       // aligned 1:1 with dest_* vectors

    pub max_resync_step: i32,             // clamp per correction
    pub source_poll_ms: u64,              // leader polling cadence for Position/searchOpen
    pub enable_follower_drift_check: bool,// optional follower drift polling
    pub enable_order_copy: bool,         // optional leader open orders mirroring (OFF by default)
    pub order_poll_ms: u64,              // cadence for order polling (defaults to SOURCE_POLL_MS)
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let src_api_base = env_var("SRC_API_BASE")?;

        let src_username = env_var("SRC_USERNAME")?;
        let src_api_key = env_var("SRC_API_KEY")?;

        // Read new plural vars; fall back to legacy single vars
        let dest_api_bases = env::var("DEST_API_BASES")
            .ok()
            .map(|s| split_csv(&s))
            .or_else(|| env::var("DEST_API_BASE").ok().map(|s| vec![s]))
            .unwrap_or_default();
        let dest_usernames = env::var("DEST_USERNAMES")
            .ok()
            .map(|s| split_csv(&s))
            .or_else(|| env::var("DEST_USERNAME").ok().map(|s| vec![s]))
            .unwrap_or_default();
        let dest_api_keys = env::var("DEST_API_KEYS")
            .ok()
            .map(|s| split_csv(&s))
            .or_else(|| env::var("DEST_API_KEY").ok().map(|s| vec![s]))
            .unwrap_or_default();

        if dest_api_bases.is_empty() || dest_usernames.is_empty() || dest_api_keys.is_empty() {
            return Err(anyhow!("Missing destination configuration: provide DEST_API_BASES/DEST_USERNAMES/DEST_API_KEYS or legacy DEST_* variables"));
        }
        if dest_api_bases.len() != dest_usernames.len() || dest_api_bases.len() != dest_api_keys.len() {
            return Err(anyhow!("Destination vectors must be equal length: DEST_API_BASES, DEST_USERNAMES, DEST_API_KEYS"));
        }

        let source_account = env_var("SOURCE_ACCOUNT")?;

        // DEST_ACCOUNTS now aligned 1:1 with dest firms (comma-separated list)
        let dest_accounts = env::var("DEST_ACCOUNTS")
            .map(|s| split_csv(&s))
            .unwrap_or_default();
        if dest_accounts.is_empty() {
            return Err(anyhow!("No destination accounts provided (set DEST_ACCOUNTS)"));
        }
        if dest_accounts.len() != dest_api_bases.len() {
            return Err(anyhow!("DEST_ACCOUNTS length must match destination vectors length"));
        }

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

        let enable_order_copy = env::var("ORDER_COPY")
            .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
            .unwrap_or(false); // default OFF

        let order_poll_ms = env::var("ORDER_POLL_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(source_poll_ms);

        Ok(Self {
            src_api_base,
            src_auth: AuthMode::ApiKey { username: src_username, api_key: src_api_key },
            dest_api_bases,
            dest_usernames,
            dest_api_keys,
            source_account,
            dest_accounts,
            max_resync_step,
            source_poll_ms,
            enable_follower_drift_check,
            enable_order_copy,
            order_poll_ms,
        })
    }
}

fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

fn env_var(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("missing env {}", name))
}