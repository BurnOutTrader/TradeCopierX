use serde::{Deserialize, Serialize};

// ===== Common API envelope =====
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiEnvelope<T> {
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

// ===== Auth =====
#[derive(Clone, Debug)]
pub enum AuthMode {
    ApiKey { username: String, api_key: String },
}

// ===== Account search =====
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

// ===== Position search open =====
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionSearchOpenReq { pub account_id: i32 }

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionRecord {
    pub id: i64,
    pub account_id: i32,
    pub contract_id: String,
    pub creation_timestamp: String,
    pub r#type: i32, // 1=Long, 2=Short (per your docs)
    pub size: i32,
    pub average_price: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionSearchOpenRes { pub positions: Vec<PositionRecord> }

// ===== Orders & trades =====
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceOrderReq<'a> {
    pub account_id: i32,
    pub contract_id: &'a str,
    pub r#type: i32, // 2 = Market
    pub side: i32,   // 0 buy, 1 sell
    pub size: i32,
    #[serde(skip_serializing_if = "Option::is_none")] pub limit_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")] pub stop_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")] pub trail_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")] pub custom_tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub linked_order_id: Option<i64>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceOrderRes { pub order_id: i64 }

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseContractReq<'a> { pub account_id: i32, pub contract_id: &'a str }