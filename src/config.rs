use std::env;
use anyhow::Context;
use crate::models::AuthMode;

#[derive(Clone, Debug)]
pub(crate) struct Config {
    // SOURCE (read trades)
    pub(crate) src_api_base: String,
    pub(crate) src_auth: AuthMode,
    pub(crate) source_account_id: String,

    // DESTINATION (place orders)
    pub(crate) dest_api_base: String,
    pub(crate) dest_auth: AuthMode,
    pub(crate) dest_account_ids: Vec<String>,

    // Polling
    pub(crate) poll_interval_ms: u64,
}

impl Config {
    pub(crate) fn from_env() -> anyhow::Result<Self> {
        fn var_first(keys: &[&str]) -> Option<String> {
            for k in keys {
                if let Ok(v) = env::var(k) {
                    let t = v.trim();
                    if !t.is_empty() {
                        return Some(t.to_string());
                    }
                }
            }
            None
        }
        let env_parse_vec = |key: &str| -> Vec<String> {
            env::var(key)
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };

        let src_api_base = var_first(&["PX_SRC_API_BASE", "PX_API_BASE"]).unwrap_or_else(|| "https://gateway-api-demo.s2f.projectx.com".to_string());
        let dest_api_base = var_first(&["PX_DEST_API_BASE"]).unwrap_or_else(|| src_api_base.clone());

        // SOURCE uses API key auth
        let src_auth = AuthMode::ApiKey {
            username: var_first(&["PX_SRC_USERNAME", "PX_SOURCE_USERNAME"]).unwrap_or_default(),
            api_key: var_first(&["PX_SRC_API_KEY", "PX_SOURCE_API_KEY"]).unwrap_or_default(),
        };

        // DEST uses API key auth
        let dest_auth = AuthMode::ApiKey {
            username: var_first(&["PX_DEST_USERNAME"]).unwrap_or_default(),
            api_key: var_first(&["PX_DEST_API_KEY"]).unwrap_or_default(),
        };

        Ok(Self {
            src_api_base,
            src_auth,
            source_account_id: var_first(&["PX_SRC_ACCOUNT", "PX_SOURCE_ACCOUNT", "PX_SOURCE_ACCOUNT_ID"]).context("Missing source account id (set PX_SRC_ACCOUNT or PX_SOURCE_ACCOUNT)")?,
            dest_api_base,
            dest_auth,
            dest_account_ids: {
                let list = env_parse_vec("PX_DEST_ACCOUNTS");
                if !list.is_empty() {
                    list
                } else if let Some(one) = var_first(&["PX_DEST_ACCOUNT", "PX_DEST_ACCOUNT_ID"]) {
                    vec![one]
                } else {
                    vec![]
                }
            },
            poll_interval_ms: {
                let ms = env::var("PX_POLL_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(500);
                ms.max(500)
            },
        })
    }
}