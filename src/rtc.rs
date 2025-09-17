use crate::client::PxClient;
use crate::models::TradeRecord;
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub enum RtcEvent {
    Trade(TradeRecord),
    SrcPosition { contract_id: String, signed_net: i32 },
}

// Only USER HUB. No market hub anywhere.
fn user_hub_url(rtc_base: &str, token: &str) -> String {
    let url = rtc_base.trim_end_matches('/');
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
    let mut s = v.to_string();
    s.push('\u{001e}'); // SignalR record separator
    Message::Text(s)
}

pub async fn start_userhub(
    src: Arc<PxClient>,
    rtc_base: String,
    source_account_id: i32,
) -> Result<mpsc::Receiver<RtcEvent>> {
    let (tx, rx) = mpsc::channel::<RtcEvent>(2048);

    tokio::spawn(async move {
        use tokio::time::{sleep, Duration};

        let mut attempt: u32 = 0;

        loop {
            attempt = attempt.saturating_add(1);

            // 1) token
            let token = match src.bearer().await {
                Ok(t) => t,
                Err(e) => {
                    warn!("RTC: failed to get bearer token: {}", e);
                    sleep(Duration::from_millis(1000)).await;
                    continue;
                }
            };

            // 2) connect to USER HUB only
            let url = user_hub_url(&rtc_base, &token);
            info!("RTC: connecting to {}", url);

            match tokio_tungstenite::connect_async(url).await {
                Ok((mut ws, _resp)) => {
                    info!("RTC: connected");
                    attempt = 0;

                    // 3) handshake
                    let hs = serde_json::json!({ "protocol": "json", "version": 1 });
                    if ws.send(frame_json(&hs)).await.is_err() {
                        warn!("RTC: failed to send handshake; reconnecting");
                        continue;
                    }

                    // 4) subscribe: source only
                    let subs = vec![
                        serde_json::json!({"type":1, "target":"SubscribeAccounts", "arguments": []}),
                        serde_json::json!({"type":1, "target":"SubscribeOrders",   "arguments": [source_account_id]}),
                        serde_json::json!({"type":1, "target":"SubscribePositions","arguments": [source_account_id]}),
                        serde_json::json!({"type":1, "target":"SubscribeTrades",   "arguments": [source_account_id]}),
                    ];
                    for m in subs {
                        if let Err(e) = ws.send(frame_json(&m)).await {
                            warn!("RTC: failed to send subscribe: {}", e);
                            continue;
                        }
                    }

                    // 5) read loop
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
                                                        && let Some(pt)  = first.get("type").and_then(|v| v.as_i64())  // 1=Long 2=Short
                                                        && let Some(sz)  = first.get("size").and_then(|v| v.as_i64())
                                                    {
                                                        if acc as i32 == source_account_id {
                                                            let signed = match pt { 1 =>  sz as i32, 2 => -(sz as i32), _ => 0 };
                                                            let _ = tx.send(RtcEvent::SrcPosition {
                                                                contract_id: cid.to_string(),
                                                                signed_net: signed
                                                            }).await;
                                                        }
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                        Ok(_) => {}
                                        Err(_) => { tracing::debug!("RTC: unparsed frame: {}", p); }
                                    }
                                }
                            }
                            Ok(Message::Close(_)) => {
                                warn!("RTC: server closed connection");
                                break;
                            }
                            Err(e) => {
                                warn!("RTC websocket error: {}", e);
                                break;
                            }
                            _ => {}
                        }
                    }

                    info!("RTC: disconnected, will reconnect");
                }
                Err(e) => {
                    warn!("RTC: connect failed: {}", e);
                }
            }

            // backoff (cap 10s)
            let backoff_ms = (500u64 * (1u64 << (attempt.min(6)))).min(10_000);
            sleep(std::time::Duration::from_millis(backoff_ms)).await;
        }
    });

    Ok(rx)
}