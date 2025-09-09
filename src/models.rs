use serde::{Deserialize, Serialize};
// =============== API Models =================
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApiEnvelope<T> {
    #[serde(default)]
    pub(crate) success: bool,
    #[serde(default)]
    pub(crate) error_code: i32,
    #[serde(default)]
    pub(crate) error_message: Option<String>,
    #[serde(default)]
    pub(crate) token: Option<String>,
    #[serde(flatten)]
    pub(crate) data: T,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LoginKeyReq<'a> {
    #[serde(rename = "userName")] pub user_name: &'a str,
    #[serde(rename = "apiKey")] pub api_key: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LoginKeyRes {}

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


// =============== Position Models =================
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionSearchOpenReq { pub(crate) account_id: i32 }

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

#[derive(Clone, Debug)]
pub(crate) enum AuthMode {
    ApiKey { username: String, api_key: String },
}