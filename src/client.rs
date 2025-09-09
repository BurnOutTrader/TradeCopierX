use reqwest::{Client, StatusCode};
use tokio::sync::RwLock;
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use crate::models::{AccountSearchReq, AccountSearchRes, ApiEnvelope, AuthMode, CloseContractReq, LoginKeyReq, LoginKeyRes, PlaceOrderReq, PlaceOrderRes, PositionSearchOpenReq, PositionSearchOpenRes, TradeSearchReq, TradeSearchRes};
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
    pub async fn place_order(&self, req: &PlaceOrderReq<'_>) -> anyhow::Result<PlaceOrderRes> {
        self.authed_post("/api/Order/place", req).await
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