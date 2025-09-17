use anyhow::{anyhow, Result};
use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    pub src_api_base: String,
    pub dest_api_base: String,

    pub src_username: String,
    pub src_api_key: String,
    pub dest_username: String,
    pub dest_api_key: String,

    // Account identifiers: can be numeric IDs or exact names
    pub source_account_id: String,
    pub dest_account_ids: Vec<String>,

}

impl Config {
    pub fn from_env() -> Result<Self> {
        let src_api_base = env::var("PX_SRC_API_BASE")?;
        let dest_api_base = env::var("PX_DEST_API_BASE")?;

        let src_username = env::var("PX_SRC_USERNAME")?;
        let src_api_key  = env::var("PX_SRC_API_KEY")?;
        let dest_username= env::var("PX_DEST_USERNAME")?;
        let dest_api_key = env::var("PX_DEST_API_KEY")?;

        let source_account_id = env::var("PX_SOURCE_ACCOUNT_ID")
            .or_else(|_| env::var("PX_SOURCE_ACCOUNT"))?; // fallback
        let dest_accounts_raw = env::var("PX_DEST_ACCOUNTS")
            .or_else(|_| env::var("PX_DEST_ACCOUNT").map(|s| s.to_string()))
            .or_else(|_| env::var("PX_DEST_ACCOUNT_ID").map(|s| s.to_string()))
            .map_err(|_| anyhow!("Provide PX_DEST_ACCOUNTS (comma list) or PX_DEST_ACCOUNT / PX_DEST_ACCOUNT_ID"))?;

        let dest_account_ids: Vec<String> = dest_accounts_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if dest_account_ids.is_empty() {
            return Err(anyhow!("No destination accounts provided."));
        }

        Ok(Self {
            src_api_base,
            dest_api_base,
            src_username,
            src_api_key,
            dest_username,
            dest_api_key,
            source_account_id,
            dest_account_ids,
        })
    }
}