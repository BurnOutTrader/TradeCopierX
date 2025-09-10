use reqwest::{Client, StatusCode};
use tokio::sync::RwLock;
use anyhow::anyhow;
use reqwest::header::ACCEPT;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::models::{AccountSearchReq, AccountSearchRes, ApiEnvelope, AuthMode, CloseContractReq, LoginKeyReq, LoginKeyRes, PositionSearchOpenReq, PositionSearchOpenRes, TradeSearchReq, TradeSearchRes};
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
            if !env.success && env.error_code != 0 {
                return Err(anyhow!("API error {}: {:?}", env.error_code, env.error_message));
            }
            return Ok(env.data);
        }
    }

    // Trades
    pub async fn search_trades(&self, req: &TradeSearchReq<'_>) -> anyhow::Result<TradeSearchRes> {
        self.authed_post("/api/Trade/search", req).await
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

            if resp.status() == StatusCode::UNAUTHORIZED && attempts < 2 {
                self.login().await?;
                continue;
            }

            let status = resp.status();
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
            if resp.status() == StatusCode::UNAUTHORIZED && attempts < 2 {
                self.login().await?;
                continue;
            }
            if !resp.status().is_success() {
                let status = resp.status();
                let txt = resp.text().await.unwrap_or_default();
                return Err(anyhow!("POST /api/Position/closeContract failed: {} — {}", status, txt));
            }
            break;
        }
        Ok(())
    }
}