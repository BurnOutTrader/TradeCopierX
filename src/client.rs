use reqwest::{Client, StatusCode};
use tokio::sync::RwLock;
use anyhow::anyhow;
use reqwest::header::ACCEPT;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::models::{AccountSearchReq, AccountSearchRes, ApiEnvelope, AuthMode, CloseContractReq, LoginKeyReq, LoginKeyRes, PositionSearchOpenReq, PositionSearchOpenRes};
// =============== API Client =================
pub struct PxClient {
    pub api_base: String,
    pub auth: AuthMode,
    pub http: Client,
    pub token: RwLock<Option<String>>,
}

impl PxClient {
    pub fn new(api_base: String, auth: AuthMode) -> Self {
        let http = Client::builder().build().unwrap();
        Self { api_base, auth, http, token: RwLock::new(None) }
    }

    fn backoff_cfg() -> (u64, u32) {
        // base_ms, max_retries
        let base = std::env::var("PX_HTTP_BACKOFF_BASE_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(250u64);
        let maxr = std::env::var("PX_HTTP_MAX_RETRIES").ok().and_then(|s| s.parse().ok()).unwrap_or(6u32);
        (base, maxr)
    }

    async fn sleep_backoff(attempts: u32, retry_after_ms: Option<u64>) {
        use rand::{rngs::SmallRng, Rng, SeedableRng};
        use tokio::time::{sleep, Duration};
        // Prefer server instruction
        let dur_ms = if let Some(ms) = retry_after_ms { ms } else {
            let (base, _) = Self::backoff_cfg();
            let exp = base.saturating_mul(1u64 << (attempts.saturating_sub(1).min(10)));
            // jitter 0..base
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
        let body = LoginKeyReq { user_name: username, api_key };
        let resp = self.http.post(url).json(&body).send().await?;
        if resp.status() != StatusCode::OK { return Err(anyhow!("loginKey http status {}", resp.status())); }
        let env: ApiEnvelope<LoginKeyRes> = resp.json().await?;
        let token = env.token.ok_or_else(|| anyhow!("missing token in loginKey response"))?;
        *self.token.write().await = Some(token.clone());
        Ok(token)
    }

    pub async fn bearer(&self) -> anyhow::Result<String> {
        if let Some(tok) = self.token.read().await.clone() { return Ok(tok); }
        self.login().await
    }

    pub async fn authed_post<T: for<'de> Deserialize<'de>, B: Serialize + ?Sized>(&self, path: &str, body: &B) -> anyhow::Result<T> {
        let (_, max_retries) = Self::backoff_cfg();
        let mut attempts: u32 = 0;
        loop {
            attempts += 1;
            let token = self.bearer().await?;
            let url = format!("{}{}", self.api_base, path);
            let resp = self.http.post(url)
                .bearer_auth(&token)
                .json(body)
                .send().await?;

            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && attempts < 2 {
                // refresh token once
                self.login().await?;
                continue;
            }

            if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                // Respect Retry-After header if present
                let retry_after_ms = resp.headers().get("retry-after").and_then(|h| h.to_str().ok()).and_then(|s| {
                    // numeric seconds only
                    if let Ok(sec) = s.trim().parse::<u64>() { Some(sec * 1000) } else { None }
                });
                if attempts <= max_retries { 
                    tracing::warn!("HTTP {} on {} — backing off (attempt {}/{})", status.as_u16(), path, attempts, max_retries);
                    Self::sleep_backoff(attempts, retry_after_ms).await;
                    continue;
                }
                let body_txt = resp.text().await.unwrap_or_default();
                return Err(anyhow!("POST {} failed after {} attempts: {} — {}", path, attempts, status, body_txt));
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

    // Orders
    pub(crate) async fn place_order(
        &self,
        req: &crate::models::PlaceOrderReq<'_>,
    ) -> anyhow::Result<crate::models::PlaceOrderRes> {
        let path = "/api/Order/place";
        let mut attempts = 0;

        loop {
            attempts += 1;
            let token = self.bearer().await?;
            let url = format!("{}{}", self.api_base, path);
            let resp = self.http
                .post(url)
                .bearer_auth(&token)
                .header(ACCEPT, "application/json")
                .json(req)
                .send()
                .await?;

            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && attempts < 2 {
                self.login().await?;
                continue;
            }

            if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                let retry_after_ms = resp.headers().get("retry-after").and_then(|h| h.to_str().ok()).and_then(|s| {
                    if let Ok(sec) = s.trim().parse::<u64>() { Some(sec * 1000) } else { None }
                });
                let (_, max_retries) = Self::backoff_cfg();
                if attempts <= max_retries {
                    tracing::warn!("place_order: HTTP {} — backing off (attempt {}/{})", status.as_u16(), attempts, max_retries);
                    Self::sleep_backoff(attempts, retry_after_ms).await;
                    continue;
                }
                let body = resp.text().await.unwrap_or_default();
                return Err(anyhow!("place_order failed after {} attempts: HTTP {} — {}", attempts, status, body));
            }

            let body = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(anyhow!("HTTP {} — {}", status, body));
            }

            // 1) Envelope: if present and success==false -> hard error
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                if let Some(s) = v.get("success").and_then(|x| x.as_bool()) {
                    if !s {
                        let code = v.get("errorCode").and_then(|x| x.as_i64()).unwrap_or_default();
                        let msg  = v.get("errorMessage").and_then(|x| x.as_str()).unwrap_or("unknown error");
                        return Err(anyhow!("API error (code {}): {}", code, msg));
                    }
                }
                // try orderId from either shape
                if let Some(oid) = v.get("orderId").and_then(|x| x.as_i64())
                    .or_else(|| v.get("data").and_then(|d| d.get("orderId")).and_then(|x| x.as_i64()))
                {
                    return Ok(crate::models::PlaceOrderRes { order_id: oid });
                }
            }

            // 2) Direct model
            if let Ok(p) = serde_json::from_str::<crate::models::PlaceOrderRes>(&body) {
                return Ok(p);
            }

            // 3) Accept empty/OK-ish as success with synthetic id
            let trimmed = body.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("ok") || trimmed == "true" || trimmed == "false" {
                tracing::debug!("place_order: 2xx with empty/non-JSON body: {:?}", trimmed);
                return Ok(crate::models::PlaceOrderRes { order_id: 0 });
            }

            // 4) Unknown 2xx payload -> warn, but return soft success
            tracing::warn!("place_order: 2xx but unknown body: {}", body);
            return Ok(crate::models::PlaceOrderRes { order_id: 0 });
        }
    }

    // Accounts
    pub async fn search_accounts(&self, only_active: Option<bool>) -> anyhow::Result<AccountSearchRes> {
        let req = AccountSearchReq { only_active_accounts: only_active };
        self.authed_post("/api/Account/search", &req).await
    }

    pub async fn search_open_positions(&self, account_id: i32) -> anyhow::Result<PositionSearchOpenRes> {
        let req = PositionSearchOpenReq { account_id };
        self.authed_post("/api/Position/searchOpen", &req).await
    }

    pub async fn close_contract(&self, account_id: i32, contract_id: &str) -> anyhow::Result<()> {
        let req = CloseContractReq { account_id, contract_id };
        // same authed_post, but ignore envelope payload
        let mut attempts = 0;
        loop {
            attempts += 1;
            let token = self.bearer().await?;
            let url = format!("{}{}", self.api_base, "/api/Position/closeContract");
            let resp = self.http.post(url).bearer_auth(&token).json(&req).send().await?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && attempts < 2 {
                self.login().await?;
                continue;
            }
            if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                let retry_after_ms = resp.headers().get("retry-after").and_then(|h| h.to_str().ok()).and_then(|s| {
                    if let Ok(sec) = s.trim().parse::<u64>() { Some(sec * 1000) } else { None }
                });
                let (_, max_retries) = Self::backoff_cfg();
                if attempts <= max_retries {
                    tracing::warn!("close_contract: HTTP {} — backing off (attempt {}/{})", status.as_u16(), attempts, max_retries);
                    Self::sleep_backoff(attempts, retry_after_ms).await;
                    continue;
                }
                let txt = resp.text().await.unwrap_or_default();
                return Err(anyhow!("POST /api/Position/closeContract failed after {} attempts: {} — {}", attempts, status, txt));
            }
            if !status.is_success() {
                let txt = resp.text().await.unwrap_or_default();
                return Err(anyhow!("POST /api/Position/closeContract failed: {} — {}", status, txt));
            }
            break;
        }
        Ok(())
    }
}