mod models;
mod config;
mod client;
mod rtc;
mod copier;

use anyhow::{anyhow, Context, Result};
use config::Config;
use copier::Copier;
use client::{PxClient};
use models::AuthMode;
use std::{env, sync::Arc};
use dotenvy::dotenv;
use tracing_subscriber::EnvFilter;

// Helper: resolve id-or-name to numeric account id via startup-only search
async fn resolve_account_id(client: &PxClient, id_or_name: &str) -> Result<i32> {
    if let Ok(n) = id_or_name.parse::<i32>() { return Ok(n); }
    let res = client.search_accounts(Some(true)).await?;
    if let Some(acc) = res.accounts.into_iter().find(|a| a.name == id_or_name) {
        Ok(acc.id)
    } else {
        Err(anyhow!("Account not found by name: {}", id_or_name))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    // Logging
    let filter = env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    tracing_subscriber::fmt().with_env_filter(EnvFilter::new(filter)).init();

    // Show some env for diagnostics (masked)
    for k in [
        "PX_SRC_API_BASE","PX_DEST_API_BASE","PX_SRC_USERNAME","PX_SRC_API_KEY","PX_SOURCE_ACCOUNT_ID",
        "PX_DEST_USERNAME","PX_DEST_API_KEY","PX_DEST_ACCOUNTS","PX_TICK_MS","PX_RTC_BASE",
        "PX_HTTP_BACKOFF_BASE_MS","PX_HTTP_MAX_RETRIES","PX_REST_CONCURRENCY",
    ] {
        match env::var(k) {
            Ok(v) => {
                let summary = if k.ends_with("API_KEY") {
                    format!("{}=****{}", k, &v.chars().rev().take(4).collect::<String>().chars().rev().collect::<String>())
                } else { format!("{}={}", k, v) };
                println!("{}", summary);
            },
            Err(_) => println!("{}=(unset)", k),
        }
    }

    let cfg = Config::from_env()?;

    // Build clients
    let src_auth  = AuthMode::ApiKey { username: cfg.src_username.clone(), api_key: cfg.src_api_key.clone() };
    let dest_auth = AuthMode::ApiKey { username: cfg.dest_username.clone(), api_key: cfg.dest_api_key.clone() };

    let src  = Arc::new(PxClient::new(cfg.src_api_base.clone(),  src_auth));
    let dest = Arc::new(PxClient::new(cfg.dest_api_base.clone(), dest_auth));

    // Pre-auth both
    let _ = src.login().await.context("src login failed")?;
    let _ = dest.login().await.context("dest login failed")?;

    // Resolve account ids
    let src_id = resolve_account_id(&src, &cfg.source_account_id).await?;
    let mut dest_ids: Vec<i32> = Vec::new();
    for s in &cfg.dest_account_ids {
        dest_ids.push(resolve_account_id(&dest, s).await?);
    }

    // Start copier
    let copier = Copier::new(src.clone(), dest.clone(), src_id, dest_ids);
    copier.run().await?;
    Ok(())
}