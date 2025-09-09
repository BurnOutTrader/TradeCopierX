use serde::{Deserialize, Serialize};
use reqwest::{Client, StatusCode};
use tokio::sync::RwLock;
use anyhow::anyhow;

// =============== API Models =================
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiEnvelope<T> {
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub error_code: i32,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(flatten)]
    pub data: T,
}

#[derive(Debug, Clone, Serialize)]
struct LoginKeyReq<'a> {
    #[serde(rename = "userName")] pub user_name: &'a str,
    #[serde(rename = "apiKey")] pub api_key: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginKeyRes {}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeSearchReq<'a> {
    pub account_id: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_timestamp: Option<&'a str>, // RFC3339
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_timestamp: Option<&'a str>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(unused)]
pub struct TradeRecord {
    pub id: i64,
    pub account_id: i32,
    pub contract_id: String,
    pub creation_timestamp: String, // RFC3339
    pub price: f64,
    #[serde(default)]
    pub profit_and_loss: Option<f64>,
    #[serde(default)]
    pub fees: Option<f64>,
    pub side: i32, // 0=Bid(buy),1=Ask(sell)
    pub size: i32,
    pub voided: bool,
    pub order_id: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeSearchRes {
    pub trades: Vec<TradeRecord>
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceOrderReq<'a> {
    pub account_id: i32,
    pub contract_id: &'a str,
    pub r#type: i32, // 2 = Market (default copier behavior)
    pub side: i32,   // 0 buy, 1 sell
    pub size: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trail_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linked_order_id: Option<i64>,
}

// =============== Account Models =================
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSearchReq {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only_active_accounts: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSummary {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSearchRes {
    #[serde(default)]
    pub accounts: Vec<AccountSummary>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceOrderRes {
    pub order_id: i64,
}

// =============== API Client =================
pub struct PxClient {
    pub api_base: String,
    pub auth: AuthMode,
    pub http: Client,
    pub token: RwLock<Option<String>>,
}

// =============== Position Models =================
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionSearchOpenReq { account_id: i32 }

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(unused)]
pub struct PositionRecord {
    pub id: i64,
    pub account_id: i32,
    pub contract_id: String,
    pub creation_timestamp: String,
    pub r#type: i32,        // direction flag from API
    pub size: i32,          // net size
    pub average_price: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionSearchOpenRes { pub positions: Vec<PositionRecord> }

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseContractReq<'a> {pub  account_id: i32, pub contract_id: &'a str }

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

#[derive(Clone, Debug)]
pub(crate) enum AuthMode {
    ApiKey { username: String, api_key: String },
}