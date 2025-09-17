use std::sync::Arc;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::{sync::mpsc, time::{sleep, Duration}};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::{client::PxClient, models::TradeRecord};

// Events we care about from the user hub
#[derive(Debug, Clone)]
pub enum RtcEvent {
    Trade(TradeRecord),
    // We could add Order/Position if needed later
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

pub async fn start_userhub(src: Arc<PxClient>, source_account_id: i32) -> anyhow::Result<mpsc::Receiver<RtcEvent>> {
    let (tx, rx) = mpsc::channel::<RtcEvent>(1024);

    tokio::spawn(async move {
        let mut attempt: u32 = 0;
        loop {
            attempt = attempt.saturating_add(1);
            // get token (refresh if needed)
            let token = match src.bearer().await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("RTC: failed to get bearer token: {}", e);
                    sleep(Duration::from_millis(1000)).await;
                    continue;
                }
            };
            let url = user_hub_url(&token);
            tracing::info!("RTC: connecting to {}", url);
            match connect_async(url).await {
                Ok((mut ws, _resp)) => {
                    tracing::info!("RTC: connected");
                    // Reset attempt counter after successful connect
                    attempt = 0;

                    // Handshake
                    let hs = serde_json::json!({"protocol":"json","version":1});
                    if ws.send(frame_json(&hs)).await.is_err() { continue; }

                    // Subscriptions
                    let subs = vec![
                        serde_json::json!({"type":1, "target":"SubscribeAccounts", "arguments": []}),
                        serde_json::json!({"type":1, "target":"SubscribeOrders",   "arguments": [source_account_id]}),
                        serde_json::json!({"type":1, "target":"SubscribePositions","arguments": [source_account_id]}),
                        serde_json::json!({"type":1, "target":"SubscribeTrades",   "arguments": [source_account_id]}),
                    ];
                    for m in subs {
                        if let Err(e) = ws.send(frame_json(&m)).await { 
                            tracing::warn!("RTC: failed to send subscribe: {}", e);
                            break; 
                        }
                    }

                    // Read loop
                    while let Some(msg) = ws.next().await {
                        match msg {
                            Ok(Message::Text(txt)) => {
                                for part in txt.split('\u{001e}') {
                                    let p = part.trim();
                                    if p.is_empty() { continue; }
                                    if let Ok(inv) = serde_json::from_str::<ServerInvocation>(p) {
                                        if inv.r#type == 1 {
                                            if let Some(tgt) = inv.target.as_deref() {
                                                if tgt == "GatewayUserTrade" {
                                                    if let Some(args) = inv.arguments.as_ref() {
                                                        if let Some(first) = args.get(0) {
                                                            if let Ok(tr) = serde_json::from_value::<TradeRecord>(first.clone()) {
                                                                let _ = tx.send(RtcEvent::Trade(tr)).await;
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        // ignore other message types
                                    } else {
                                        tracing::debug!("RTC: non-invocation message: {}", p);
                                    }
                                }
                            }
                            Ok(Message::Binary(_)) => {}
                            Ok(Message::Ping(_)) => {}
                            Ok(Message::Pong(_)) => {}
                            Ok(Message::Frame(_)) => {}
                            Ok(Message::Close(_)) => {
                                tracing::warn!("RTC: server closed connection");
                                break;
                            }
                            Err(e) => {
                                tracing::warn!("RTC websocket error: {}", e);
                                break;
                            }
                        }
                    }
                    tracing::info!("RTC: disconnected, will reconnect");
                }
                Err(e) => {
                    tracing::warn!("RTC: connect failed: {}", e);
                }
            }
            // backoff with cap
            let backoff_ms = (500u64 * (1u64 << (attempt.min(6)))).min(10_000);
            sleep(Duration::from_millis(backoff_ms)).await;
        }
    });

    Ok(rx)
}
