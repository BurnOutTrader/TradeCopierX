use std::sync::Arc;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::{client::PxClient, models::TradeRecord};

// Events we care about from the user hub
#[derive(Debug, Clone)]
pub enum RtcEvent {
    Trade(TradeRecord),
    SrcPosition { contract_id: String, signed_net: i32 },
}

fn default_rtc_base() -> String {
    std::env::var("PX_RTC_BASE").unwrap_or_else(|_| "https://rtc.topstepx.com".to_string())
}

fn user_hub_url(token: &str) -> String {
    let base = default_rtc_base();
    // Ensure wss scheme and append /hubs/user
    let url = base.trim_end_matches('/');
    let wss = url.replace("http://", "ws://").replace("https://", "wss://");
    let enc_tok = urlencoding::encode(token);
    format!("{}/hubs/user?access_token={}", wss, enc_tok)
}

#[derive(Deserialize)]
struct ServerInvocation {
    #[serde(default)]
    r#type: i32,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    arguments: Option<Vec<serde_json::Value>>,
}

fn frame_json(v: &serde_json::Value) -> Message {
    // SignalR JSON protocol frames with 0x1E (record separator)
    let mut s = v.to_string();
    s.push('\u{001e}');
    Message::Text(s)
}

pub async fn start_userhub(
    src: Arc<PxClient>,
    source_account_id: i32,
    dest_account_ids: Vec<i32>,
    dest_pos_tx: mpsc::Sender<(i32, String, i32)>, // (destId, contractId, signedNet)
) -> anyhow::Result<mpsc::Receiver<RtcEvent>> {
    let (tx, rx) = mpsc::channel::<RtcEvent>(1024);

    tokio::spawn(async move {
        use futures_util::{SinkExt, StreamExt};
        use tokio::time::{sleep, Duration};
        use tokio_tungstenite::tungstenite::Message;

        let mut attempt: u32 = 0;

        loop {
            attempt = attempt.saturating_add(1);

            // 1) Bearer (refresh if needed)
            let token = match src.bearer().await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("RTC: failed to get bearer token: {}", e);
                    sleep(Duration::from_millis(1000)).await;
                    continue;
                }
            };

            // 2) Connect
            let url = user_hub_url(&token);
            tracing::info!("RTC: connecting to {}", url);

            match tokio_tungstenite::connect_async(url).await {
                Ok((mut ws, _resp)) => {
                    tracing::info!("RTC: connected");
                    attempt = 0; // reset backoff

                    // 3) Handshake
                    let hs = serde_json::json!({ "protocol": "json", "version": 1 });
                    if ws.send(frame_json(&hs)).await.is_err() {
                        tracing::warn!("RTC: failed to send handshake; reconnecting");
                        continue;
                    }

                    // 4) Subscriptions
                    let mut subs = vec![
                        serde_json::json!({"type":1, "target":"SubscribeAccounts", "arguments": []}),
                        serde_json::json!({"type":1, "target":"SubscribeOrders",   "arguments": [source_account_id]}),
                        serde_json::json!({"type":1, "target":"SubscribePositions","arguments": [source_account_id]}),
                        serde_json::json!({"type":1, "target":"SubscribeTrades",   "arguments": [source_account_id]}),
                    ];
                    for id in &dest_account_ids {
                        subs.push(serde_json::json!({"type":1, "target":"SubscribePositions","arguments":[id]}));
                    }
                    for m in subs {
                        if let Err(e) = ws.send(frame_json(&m)).await {
                            tracing::warn!("RTC: failed to send subscribe: {}", e);
                            continue;
                        }
                    }

                    // 5) Read loop
                    while let Some(msg) = ws.next().await {
                        match msg {
                            Ok(Message::Text(txt)) => {
                                for part in txt.split('\u{001e}') {
                                    let p = part.trim();
                                    if p.is_empty() { continue; }

                                    match serde_json::from_str::<ServerInvocation>(p) {
                                        Ok(inv) if inv.r#type == 1 => {
                                            match inv.target.as_deref() {
                                                Some("GatewayUserTrade") => {
                                                    if let Some(args) = inv.arguments.as_ref()
                                                        && let Some(first) = args.get(0)
                                                        && let Ok(tr) = serde_json::from_value::<TradeRecord>(first.clone())
                                                    {
                                                        let _ = tx.send(RtcEvent::Trade(tr)).await;
                                                    }
                                                }
                                                Some("GatewayUserPosition") => {
                                                    if let Some(args) = inv.arguments.as_ref()
                                                        && let Some(first) = args.get(0)
                                                        && let Some(acc) = first.get("accountId").and_then(|v| v.as_i64())
                                                        && let Some(cid) = first.get("contractId").and_then(|v| v.as_str())
                                                        && let Some(pt)  = first.get("type").and_then(|v| v.as_i64())
                                                        && let Some(sz)  = first.get("size").and_then(|v| v.as_i64())
                                                    {
                                                        let acc_i32 = acc as i32;
                                                        let signed = match pt {
                                                            1 =>  sz as i32,  // Long
                                                            2 => -(sz as i32), // Short
                                                            _ => 0,
                                                        };
                                                        if acc_i32 == source_account_id {
                                                            let _ = tx.send(RtcEvent::SrcPosition { contract_id: cid.to_string(), signed_net: signed }).await;
                                                        } else if dest_account_ids.iter().any(|&d| d == acc_i32) {
                                                            let _ = dest_pos_tx.send((acc_i32, cid.to_string(), signed)).await;
                                                        }
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                        Ok(_) => {} // ignore other SignalR message types
                                        Err(_) => {
                                            tracing::debug!("RTC: unparsed frame: {}", p);
                                        }
                                    }
                                }
                            }
                            Ok(Message::Close(_)) => {
                                tracing::warn!("RTC: server closed connection");
                                break;
                            }
                            Err(e) => {
                                tracing::warn!("RTC websocket error: {}", e);
                                break;
                            }
                            _ => {}
                        }
                    }

                    tracing::info!("RTC: disconnected, will reconnect");
                }
                Err(e) => {
                    tracing::warn!("RTC: connect failed: {}", e);
                }
            }

            // 6) Exponential backoff (cap 10s)
            let backoff_ms = (500u64 * (1u64 << (attempt.min(6)))).min(10_000);
            sleep(Duration::from_millis(backoff_ms)).await;
        }
    });

    Ok(rx)
}
