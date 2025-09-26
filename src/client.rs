use crate::models::*;
use anyhow::anyhow;
use once_cell::sync::OnceCell;
use rand::{rngs::SmallRng, Rng, SeedableRng};
use reqwest::{Client, StatusCode};
use serde::{de::DeserializeOwned, Serialize};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};
use tracing::Instrument;

static REST_LIMITER: OnceCell<Arc<Semaphore>> = OnceCell::new();

fn rest_limiter() -> Arc<Semaphore> {
    REST_LIMITER.get_or_init(|| {
        let permits = std::env::var("PX_REST_CONCURRENCY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        Arc::new(Semaphore::new(permits.max(1)))
    }).clone()
}

pub struct PxClient {
    pub api_base: String,
    pub auth: AuthMode,
    pub http: Client,
    pub token: RwLock<Option<String>>,
}

impl PxClient {
    pub fn new(api_base: String, auth: AuthMode) -> Self {
        let http = Client::builder()
            .user_agent("TradeCopierX/0.3")
            .build()
            .unwrap();
        Self { api_base, auth, http, token: RwLock::new(None) }
    }

    fn backoff_cfg() -> (u64, u32) {
        let base = std::env::var("PX_HTTP_BACKOFF_BASE_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(250u64);
        let maxr = std::env::var("PX_HTTP_MAX_RETRIES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5u32);
        (base, maxr)
    }

    async fn sleep_backoff(attempts: u32, retry_after_ms: Option<u64>) {
        use tokio::time::{sleep, Duration};
        let dur_ms = if let Some(ms) = retry_after_ms {
            ms
        } else {
            let (base, _) = Self::backoff_cfg();
            let exp = base.saturating_mul(1u64 << (attempts.saturating_sub(1).min(8)));
            let mut rng = SmallRng::from_entropy();
            exp + rng.gen_range(0..=base)
        };
        sleep(Duration::from_millis(dur_ms.min(10_000))).await;
    }

    pub async fn login(&self) -> anyhow::Result<String> {
        let (username, api_key) = match &self.auth {
            AuthMode::ApiKey { username, api_key } => (username, api_key),
        };
        let url = format!("{}/api/Auth/loginKey", self.api_base);
        let body = serde_json::json!({ "userName": username, "apiKey": api_key });
        let resp = self.http.post(url).json(&body).send().await?;
        if resp.status() != StatusCode::OK {
            return Err(anyhow!("loginKey http status {}", resp.status()));
        }
        let env: ApiEnvelope<serde_json::Value> = resp.json().await?;
        let token = env
            .token
            .ok_or_else(|| anyhow!("missing token in loginKey response"))?;
        *self.token.write().await = Some(token.clone());
        Ok(token)
    }

    pub async fn bearer(&self) -> anyhow::Result<String> {
        if let Some(tok) = self.token.read().await.clone() {
            return Ok(tok);
        }
        self.login().await
    }

    async fn take_permit() -> OwnedSemaphorePermit {
        rest_limiter().clone().acquire_owned().await.unwrap()
    }

    pub async fn authed_post<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> anyhow::Result<T> {
        // Serialize REST calls to reduce 429 bursts.
        let _permit = Self::take_permit().await;

        let (base_ms, max_retries) = Self::backoff_cfg();
        let mut attempts: u32 = 0;
        loop {
            attempts += 1;
            let token = self.bearer().await?;
            let url = format!("{}{}", self.api_base, path);
            let span = tracing::info_span!("authed_post", %path, attempt = attempts);
            let resp = self
                .http
                .post(url)
                .bearer_auth(&token)
                .json(body)
                .send()
                .instrument(span)
                .await?;

            let status = resp.status();

            if status == StatusCode::UNAUTHORIZED && attempts < 2 {
                self.login().await?;
                continue;
            }

            if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                let retry_after_ms = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .map(|sec| sec * 1000);

                // Retry critical endpoints more; others return error faster
                let is_critical = matches!(path,
                                    "/api/Order/place"
                                    | "/api/Order/modify"
                                    | "/api/Order/cancel"
                                    | "/api/Order/searchOpen"
                                    | "/api/Position/closeContract"
                                    | "/api/Position/searchOpen"
                                    | "/api/Account/search"
                                );
                if !is_critical {
                    let txt = resp.text().await.unwrap_or_default();
                    return Err(anyhow!("HTTP {} on {} — {}", status, path, txt));
                }

                if attempts <= max_retries {
                    tracing::warn!(
                        "HTTP {} on {} — backing off (attempt {}/{}, base={}ms)",
                        status.as_u16(),
                        path,
                        attempts,
                        max_retries,
                        base_ms
                    );
                    Self::sleep_backoff(attempts, retry_after_ms).await;
                    continue;
                }
                let body_txt = resp.text().await.unwrap_or_default();
                return Err(anyhow!(
                    "POST {} failed after {} attempts: {} — {}",
                    path,
                    attempts,
                    status,
                    body_txt
                ));
            }

            if !status.is_success() {
                let txt = resp.text().await.unwrap_or_default();
                return Err(anyhow!("POST {} failed: {} — {}", path, status, txt));
            }

            let env: ApiEnvelope<T> = resp.json().await?;
            if !env.success && env.error_code != 0 {
                return Err(anyhow!("API error {}: {:?}", env.error_code, env.error_message));
            }
            return Ok(env.data);
        }
    }

    // ===== Allowed operations =====
    pub async fn place_order(&self, req: &PlaceOrderReq<'_>) -> anyhow::Result<PlaceOrderRes> {
        self.authed_post("/api/Order/place", req).await
    }

    pub async fn close_contract(&self, account_id: i32, contract_id: &str) -> anyhow::Result<()> {
        let req = CloseContractReq { account_id, contract_id };
        let _void: serde_json::Value = self.authed_post("/api/Position/closeContract", &req).await?;
        Ok(())
    }

    // ===== Polling calls =====
    pub async fn search_accounts(&self, only_active: Option<bool>) -> anyhow::Result<AccountSearchRes> {
        let req = AccountSearchReq { only_active_accounts: only_active };
        self.authed_post("/api/Account/search", &req).await
    }

    pub async fn search_open_positions(&self, account_id: i32) -> anyhow::Result<PositionSearchOpenRes> {
        let req = PositionSearchOpenReq { account_id };
        self.authed_post("/api/Position/searchOpen", &req).await
    }

    // Orders
    pub async fn search_open_orders(&self, account_id: i32) -> anyhow::Result<OrderSearchOpenRes> {
        let req = OrderSearchOpenReq { account_id };
        self.authed_post("/api/Order/searchOpen", &req).await
    }

    pub async fn modify_order(&self, req: &ModifyOrderReq) -> anyhow::Result<()> {
        let _void: serde_json::Value = self.authed_post("/api/Order/modify", req).await?;
        Ok(())
    }

    pub async fn cancel_order(&self, account_id: i32, order_id: i64) -> anyhow::Result<()> {
        let req = CancelOrderReq { account_id, order_id };
        let _void: serde_json::Value = self.authed_post("/api/Order/cancel", &req).await?;
        Ok(())
    }
}