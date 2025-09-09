//! TradeCopierX — minimal MVP to copy fills between ProjectX accounts
//! Dependencies (add these to Cargo.toml):
//! - Uses REST polling of /api/Trade/search for new trades on the SOURCE account, then mirrors orders to DEST accounts.
//! - Auth uses /api/Auth/loginKey (API Key) to obtain a JWT session token per tenant. Tokens expire periodically; the client auto-refreshes.
//! - For production, consider switching to ProjectX SignalR user hub for real-time events.

use anyhow::{anyhow, Context, Result};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::{collections::{HashMap, HashSet}, env, time::Duration, sync::Arc};
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{error, info, warn};

#[derive(Clone, Debug)]
enum AuthMode {
    ApiKey { username: String, api_key: String },
}

#[derive(Clone, Debug)]
struct Config {
    // SOURCE (read trades)
    src_api_base: String,
    src_auth: AuthMode,
    source_account_id: i64,

    // DESTINATION (place orders)
    dest_api_base: String,
    dest_auth: AuthMode,
    dest_account_ids: Vec<i64>,

    // Polling
    poll_interval_ms: u64,
}

impl Config {
    fn from_env() -> Result<Self> {
        let env_parse_vec = |key: &str| -> Vec<i64> {
            env::var(key)
                .unwrap_or_default()
                .split(',')
                .filter_map(|s| s.trim().parse::<i64>().ok())
                .collect()
        };

        let src_api_base = env::var("PX_SRC_API_BASE").unwrap_or_else(|_| "https://gateway-api-demo.s2f.projectx.com".to_string());
        let dest_api_base = env::var("PX_DEST_API_BASE").unwrap_or_else(|_| src_api_base.clone());

        // SOURCE uses API key auth
        let src_auth = AuthMode::ApiKey {
            username: env::var("PX_SRC_USERNAME").unwrap_or_default(),
            api_key: env::var("PX_SRC_API_KEY").unwrap_or_default(),
        };

        // DEST uses API key auth
        let dest_auth = AuthMode::ApiKey {
            username: env::var("PX_DEST_USERNAME").unwrap_or_default(),
            api_key: env::var("PX_DEST_API_KEY").unwrap_or_default(),
        };

        Ok(Self {
            src_api_base,
            src_auth,
            source_account_id: env::var("PX_SRC_ACCOUNT").context("PX_SRC_ACCOUNT missing")?.parse()?,
            dest_api_base,
            dest_auth,
            dest_account_ids: env_parse_vec("PX_DEST_ACCOUNTS"),
            poll_interval_ms: env::var("PX_POLL_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(100),
        })
    }
}

// =============== API Models =================
#[derive(Debug, Clone, Deserialize)]
struct ApiEnvelope<T> {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    errorCode: i32,
    #[serde(default)]
    errorMessage: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(flatten)]
    data: T,
}

#[derive(Debug, Clone, Serialize)]
struct LoginKeyReq<'a> { userName: &'a str, apiKey: &'a str }
#[derive(Debug, Clone, Deserialize)]
struct LoginKeyRes {}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TradeSearchReq<'a> {
    account_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_timestamp: Option<&'a str>, // RFC3339
    #[serde(skip_serializing_if = "Option::is_none")]
    end_timestamp: Option<&'a str>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TradeRecord {
    trade_id: i64,
    order_id: i64,
    account_id: i64,
    contract_id: String,
    side: i32, // 0=Bid(buy),1=Ask(sell)
    size: i32,
    price: f64,
    timestamp: String, // RFC3339
    #[serde(default)]
    custom_tag: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TradeSearchRes {
    #[serde(default)]
    trades: Vec<TradeRecord>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlaceOrderReq<'a> {
    account_id: i64,
    contract_id: &'a str,
    r#type: i32, // 2 = Market (default copier behavior)
    side: i32,   // 0 buy, 1 sell
    size: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trail_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    custom_tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    linked_order_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaceOrderRes {
    order_id: i64,
}

// =============== API Client =================
struct PxClient {
    api_base: String,
    auth: AuthMode,
    http: Client,
    token: RwLock<Option<String>>,
}

impl PxClient {
    fn new(api_base: String, auth: AuthMode) -> Self {
        let http = Client::builder().build().unwrap();
        Self { api_base, auth, http, token: RwLock::new(None) }
    }

    async fn login(&self) -> Result<String> {
        let (username, api_key) = match &self.auth {
            AuthMode::ApiKey { username, api_key } => (username, api_key),
        };
        let url = format!("{}/api/Auth/loginKey", self.api_base);
        let body = LoginKeyReq { userName: username, apiKey: api_key };
        let resp = self.http.post(url).json(&body).send().await?;
        if resp.status() != StatusCode::OK { return Err(anyhow!("loginKey http status {}", resp.status())); }
        let env: ApiEnvelope<LoginKeyRes> = resp.json().await?;
        let token = env.token.ok_or_else(|| anyhow!("missing token in loginKey response"))?;
        *self.token.write().await = Some(token.clone());
        Ok(token)
    }

    async fn bearer(&self) -> Result<String> {
        if let Some(tok) = self.token.read().await.clone() { return Ok(tok); }
        self.login().await
    }

    async fn authed_post<T: for<'de> Deserialize<'de>, B: Serialize + ?Sized>(&self, path: &str, body: &B) -> Result<T> {
        let mut attempts = 0;
        loop {
            attempts += 1;
            let token = self.bearer().await?;
            let url = format!("{}{}", self.api_base, path);
            let resp = self.http.post(url)
                .bearer_auth(&token)
                .json(body)
                .send().await?;

            if resp.status() == StatusCode::UNAUTHORIZED && attempts < 2 {
                // refresh token once
                self.login().await?;
                continue;
            }
            if !resp.status().is_success() {
                let status = resp.status();
                let txt = resp.text().await.unwrap_or_default();
                return Err(anyhow!("POST {} failed: {} — {}", path, status, txt));
            }
            let env: ApiEnvelope<T> = resp.json().await?;
            if !env.success && env.errorCode != 0 {
                return Err(anyhow!("API error {}: {:?}", env.errorCode, env.errorMessage));
            }
            return Ok(env.data);
        }
    }

    // Trades
    async fn search_trades(&self, req: &TradeSearchReq<'_>) -> Result<TradeSearchRes> {
        self.authed_post("/api/Trade/search", req).await
    }

    // Orders
    async fn place_order(&self, req: &PlaceOrderReq<'_>) -> Result<PlaceOrderRes> {
        self.authed_post("/api/Order/place", req).await
    }
}

// =============== Copier Logic =================
struct Copier {
    src: Arc<PxClient>,
    dest: Arc<PxClient>,
    source_account_id: i64,
    dest_account_ids: Vec<i64>,
    poll_interval_ms: u64,
    seen: RwLock<HashSet<i64>>,

    // In-memory running positions, keyed by (accountId, contractId)
    src_pos: RwLock<HashMap<(i64, String), i32>>,   // source net position per contract
    dest_pos: RwLock<HashMap<(i64, String), i32>>,  // destination net position per contract (aggregated per dest account)
}

impl Copier {
    fn new(src: Arc<PxClient>, dest: Arc<PxClient>, source_account_id: i64, dest_account_ids: Vec<i64>, poll_interval_ms: u64) -> Self {
        Self {
            src, dest, source_account_id, dest_account_ids, poll_interval_ms,
            seen: RwLock::new(HashSet::new()),
            src_pos: RwLock::new(HashMap::new()),
            dest_pos: RwLock::new(HashMap::new()),
        }
    }

    fn side_to_delta(side: i32, size: i32) -> i32 {
        // 0=Bid/buy => +size, 1=Ask/sell => -size
        if side == 0 { size } else { -size }
    }

    async fn update_src_position(&self, contract_id: &str, side: i32, size: i32) -> (i32, i32) {
        let key = (self.source_account_id, contract_id.to_string());
        let mut map = self.src_pos.write().await;
        let prev = *map.get(&key).unwrap_or(&0);
        let next = prev + Self::side_to_delta(side, size);
        if next == 0 { map.remove(&key); } else { map.insert(key, next); }
        (prev, next)
    }

    async fn update_dest_position(&self, dest_acct: i64, contract_id: &str, side: i32, size: i32) -> i32 {
        let key = (dest_acct, contract_id.to_string());
        let mut map = self.dest_pos.write().await;
        let prev = *map.get(&key).unwrap_or(&0);
        let next = prev + Self::side_to_delta(side, size);
        if next == 0 { map.remove(&key); } else { map.insert(key, next); }
        next
    }

    async fn flatten_dest_if_needed(&self, contract_id: &str) {
        // If source is flat on this contract, force flatten all dest accounts for that contract
        let src_key = (self.source_account_id, contract_id.to_string());
        let src_flat = { !self.src_pos.read().await.contains_key(&src_key) };
        if !src_flat { return; }

        let mut to_flatten: Vec<(i64, i32)> = Vec::new();
        {
            let dest_map = self.dest_pos.read().await;
            for &dest in &self.dest_account_ids {
                let key = (dest, contract_id.to_string());
                if let Some(&pos) = dest_map.get(&key) {
                    if pos != 0 { to_flatten.push((dest, pos)); }
                }
            }
        }

        for (dest, pos) in to_flatten {
            let side = if pos > 0 { 1 } else { 0 }; // if long, sell; if short, buy
            let size = pos.abs();
            let tag = format!("TCX:FLATTEN:{}:{}", contract_id, dest);
            let req = PlaceOrderReq {
                account_id: dest,
                contract_id: contract_id,
                r#type: 2,
                side,
                size,
                limit_price: None,
                stop_price: None,
                trail_price: None,
                custom_tag: Some(tag),
                linked_order_id: None,
            };
            match self.dest.place_order(&req).await {
                Ok(resp) => {
                    info!("Flattened acct {} on {} with order {} (pos was {})", dest, contract_id, resp.order_id, pos);
                    // Update in-memory dest position to zero
                    let mut dest_map = self.dest_pos.write().await;
                    dest_map.remove(&(dest, contract_id.to_string()));
                }
                Err(e) => error!("Failed to flatten acct {} on {}: {}", dest, contract_id, e),
            }
        }
    }

    async fn run(&self) -> Result<()> {
        info!("TradeCopierX starting");
        let mut since_rfc3339: Option<String> = None;

        loop {
            // 1) Poll source trades
            let req = TradeSearchReq {
                account_id: self.source_account_id,
                start_timestamp: since_rfc3339.as_deref(),
                end_timestamp: None,
            };
            match self.src.search_trades(&req).await {
                Ok(res) => {
                    for tr in res.trades {
                        // avoid duplicates
                        let mut seen = self.seen.write().await;
                        if !seen.insert(tr.trade_id) { continue; }
                        drop(seen);

                        // Update source net position and, if now flat on this contract, trigger follower flattening
                        let (_prev_src, _next_src) = self.update_src_position(&tr.contract_id, tr.side, tr.size).await;

                        // 2) Mirror to destination accounts
                        for &dest in &self.dest_account_ids {
                            if dest == tr.account_id { continue; }
                            let tag = format!("TCX:{}:{}", tr.trade_id, dest); // unique per dest
                            let place = PlaceOrderReq {
                                account_id: dest,
                                contract_id: &tr.contract_id,
                                r#type: 2, // market (simple MVP)
                                side: tr.side,
                                size: tr.size,
                                limit_price: None,
                                stop_price: None,
                                trail_price: None,
                                custom_tag: Some(tag),
                                linked_order_id: None,
                            };
                            match self.dest.place_order(&place).await {
                                Ok(resp) => {
                                    info!("Mirrored trade {} to acct {} as order {}", tr.trade_id, dest, resp.order_id);
                                    // Track dest running position
                                    let _ = self.update_dest_position(dest, &tr.contract_id, tr.side, tr.size).await;
                                },
                                Err(e) => error!("Failed to mirror trade {} to acct {}: {}", tr.trade_id, dest, e),
                            }
                        }

                        // If the leader is flat on this contract, ensure followers are flat too
                        self.flatten_dest_if_needed(&tr.contract_id).await;

                        // advance watermark
                        since_rfc3339 = Some(tr.timestamp.clone());
                    }
                }
                Err(e) => {
                    warn!("trade search error: {}", e);
                }
            }

            sleep(Duration::from_millis(self.poll_interval_ms)).await;
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let cfg = Config::from_env()?;
    let src = Arc::new(PxClient::new(cfg.src_api_base.clone(), cfg.src_auth.clone()));
    let dest = Arc::new(PxClient::new(cfg.dest_api_base.clone(), cfg.dest_auth.clone()));

    // Pre-auth both
    let _ = src.login().await.context("src login failed")?;
    let _ = dest.login().await.context("dest login failed")?;

    let copier = Copier::new(src.clone(), dest.clone(), cfg.source_account_id, cfg.dest_account_ids.clone(), cfg.poll_interval_ms);
    copier.run().await?;

    Ok(())
}
