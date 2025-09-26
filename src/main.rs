mod client;
mod config;
mod copier;
mod models;

use anyhow::{anyhow, Context, Result};
use client::PxClient;
use config::Config;
use copier::Copier;
use dotenvy::dotenv;
use std::{env, fs, sync::Arc};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    // Diagnostics: cwd and .env presence
    if let Ok(cwd) = env::current_dir() { println!("cwd: {}", cwd.display()); }
    match fs::metadata(".env") {
        Ok(_) => println!(".env: found"),
        Err(_) => println!(".env: NOT found in current directory"),
    }

    tracing_subscriber::fmt().with_env_filter("info").init();

    let cfg = Config::from_env()?;
    info!("SRC_API_BASE={}", cfg.src_api_base);

    // Build clients
    let src = Arc::new(PxClient::new(cfg.src_api_base.clone(), cfg.src_auth.clone()));

    // Pre-auth source
    let _ = src.login().await.context("src login failed")?;

    // Resolve source account ID (accept numeric or account name)
    let src_id = resolve_account_id(&src, &cfg.source_account).await?;

    // Build destination clients aligned with accounts
    if cfg.dest_api_bases.is_empty() {
        return Err(anyhow!("No destination firms provided (set DEST_API_BASES or legacy DEST_API_BASE)"));
    }
    if cfg.dest_accounts.is_empty() {
        return Err(anyhow!("No destination accounts provided (set DEST_ACCOUNTS in .env)"));
    }
    let mut dest_clients: Vec<Arc<PxClient>> = Vec::new();
    let mut dest_ids: Vec<i32> = Vec::new();
    for i in 0..cfg.dest_api_bases.len() {
        let client = Arc::new(PxClient::new(
            cfg.dest_api_bases[i].clone(),
            crate::models::AuthMode::ApiKey {
                username: cfg.dest_usernames[i].clone(),
                api_key: cfg.dest_api_keys[i].clone(),
            },
        ));
        // login each dest client
        let _ = client.login().await.context("dest login failed")?;
        // resolve its paired account id
        let acc_id = resolve_account_id(&client, &cfg.dest_accounts[i]).await?;
        dest_clients.push(client);
        dest_ids.push(acc_id);
    }

    // Construct copier
    let copier = Copier::new(
        src.clone(),
        dest_clients,
        src_id,
        dest_ids,
        cfg.max_resync_step,
        cfg.source_poll_ms,     // leader poll: Position/searchOpen every N ms
        cfg.enable_follower_drift_check, // optional drift check
        cfg.enable_order_copy,  // optional order mirroring
        cfg.order_poll_ms,
    );

    // One-time reconcile via REST (align followers to leader)
    copier.reconcile_on_startup().await;

    // REST-only loop: poll leader positions every source_poll_ms, never use RTC
    copier.run_rest_only().await?;

    Ok(())
}

async fn resolve_account_id(client: &PxClient, id_or_name: &str) -> Result<i32> {
    if let Ok(n) = id_or_name.parse::<i32>() {
        return Ok(n);
    }
    let res = client.search_accounts(Some(true)).await?;
    if let Some(acc) = res.accounts.into_iter().find(|a| a.name == id_or_name) {
        Ok(acc.id)
    } else {
        Err(anyhow::anyhow!("Account not found by name: {}", id_or_name))
    }
}