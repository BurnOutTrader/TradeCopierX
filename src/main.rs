//! TradeCopierX — minimal MVP to copy fills between ProjectX accounts
//! Dependencies (add these to Cargo.toml):
//! - Uses REST polling of /api/Trade/search for new trades on the SOURCE account, then mirrors orders to DEST accounts.
//! - Auth uses /api/Auth/loginKey (API Key) to obtain a JWT session token per tenant. Tokens expire periodically; the client auto-refreshes.
//! - For production, consider switching to ProjectX SignalR user hub for real-time events.

mod models;
mod config;
mod copier;
mod client;

use anyhow::{anyhow, Context, Result};
use std::{env, fs, sync::Arc};
use dotenvy::dotenv;
use config::Config;
use copier::Copier;
use client::PxClient;

// =============== Account ID Resolver Helper =================
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
    // Diagnostics: cwd and .env
    if let Ok(cwd) = env::current_dir() { println!("cwd: {}", cwd.display()); }
    match fs::metadata(".env") {
        Ok(_) => println!(".env: found"),
        Err(_) => println!(".env: not found in current directory"),
    }
    for k in [
        "PX_SRC_API_BASE","PX_DEST_API_BASE","PX_SRC_USERNAME","PX_SRC_API_KEY","PX_SRC_ACCOUNT",
        "PX_DEST_USERNAME","PX_DEST_API_KEY","PX_DEST_ACCOUNTS","PX_DEST_ACCOUNT","PX_DEST_ACCOUNT_ID","PX_POLL_MS",
        "PX_SOURCE_USERNAME","PX_SOURCE_API_KEY","PX_SOURCE_ACCOUNT","PX_SOURCE_ACCOUNT_ID"
    ] {
        match env::var(k) {
            Ok(v) => {
                let summary = if k.ends_with("API_KEY") {
                    format!("{}=****{}", k, &v.chars().rev().take(4).collect::<String>().chars().rev().collect::<String>())
                } else {
                    v.clone()
                };
                println!("ENV {}: {}", k, if summary.len()>64 { format!("{}...", &summary[..64]) } else { summary });
            },
            Err(_) => println!("ENV {}: (unset)", k),
        }
    }
    tracing_subscriber::fmt().with_env_filter("info").init();

    let cfg = Config::from_env()?;
    let src = Arc::new(PxClient::new(cfg.src_api_base.clone(), cfg.src_auth.clone()));
    let dest = Arc::new(PxClient::new(cfg.dest_api_base.clone(), cfg.dest_auth.clone()));

    // Pre-auth both
    let _ = src.login().await.context("src login failed")?;
    let _ = dest.login().await.context("dest login failed")?;
    // Resolve source & destination account IDs (accept numeric or name strings)
    let src_id = resolve_account_id(&src, &cfg.source_account_id).await?;
    let mut dest_ids: Vec<i32> = Vec::new();
    if cfg.dest_account_ids.is_empty() {
        return Err(anyhow!("No destination accounts provided (set PX_DEST_ACCOUNTS or PX_DEST_ACCOUNT / PX_DEST_ACCOUNT_ID)"));
    }
    for s in &cfg.dest_account_ids {
        dest_ids.push(resolve_account_id(&dest, s).await?);
    }

    let copier = Copier::new(src.clone(), dest.clone(), src_id, dest_ids, cfg.poll_interval_ms, cfg.poll_interval_ms);
    copier.run().await?;

    Ok(())
}
