use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};
use tokio::time::{sleep, Instant};
use crate::client::PxClient;
use crate::models::PlaceOrderReq;
use crate::rtc::{start_userhub, RtcEvent};
use ahash::AHashMap;

use std::sync::atomic::{AtomicU64, Ordering};
static ORDER_TAG_SEQ: AtomicU64 = AtomicU64::new(1);

#[inline]
fn unique_sync_tag(contract_id: &str, dest: i32, step: i32) -> String {
    let seq = ORDER_TAG_SEQ.fetch_add(1, Ordering::Relaxed);
    // Guaranteed-unique per process; avoids "custom tag already in use"
    format!("TCX:SYNC-LIVE:{}:{}:{}:{}", contract_id, dest, step, seq)
}

// =============== Copier Logic =================
pub struct Copier {
    pub src: Arc<PxClient>,
    pub dest: Arc<PxClient>,
    pub source_account_id: i32,
    pub dest_account_ids: Vec<i32>,
    pub poll_interval_ms: u64,
    pub sync_interval_ms: u64,
    pub seen: RwLock<HashSet<i64>>,

    // In-memory running positions, keyed by (accountId, contractId)
    pub src_pos: RwLock<AHashMap<(i32, String), i32>>,   // source net position per contract
    pub dest_pos: RwLock<AHashMap<(i32, String), i32>>,  // destination net position per contract (aggregated per dest account)
}

impl Copier {
    pub(crate) fn new(src: Arc<PxClient>, dest: Arc<PxClient>, source_account_id: i32, dest_account_ids: Vec<i32>, poll_interval_ms: u64, sync_interval_ms: u64) -> Self {
        Self {
            src, dest, source_account_id, dest_account_ids, poll_interval_ms, sync_interval_ms,
            seen: RwLock::new(HashSet::new()),
            src_pos: RwLock::new(AHashMap::new()),
            dest_pos: RwLock::new(AHashMap::new()),
        }
    }

    async fn reconcile_on_startup(&self) {
        use std::collections::HashSet;
        use tracing::{info, warn};

        // 1) Build set of contracts that are OPEN on the leader, and seed source cache with SIGNED net
        let mut src_open: HashSet<String> = HashSet::new();
        match self.src.search_open_positions(self.source_account_id).await {
            Ok(res) => {
                for p in res.positions {
                    src_open.insert(p.contract_id.clone());
                    // SIGNED net: type 0 = long (+), 1 = short (-)
                    let net = if p.r#type == 0 { p.size } else { -p.size };
                    let mut sp = self.src_pos.write().await;
                    if net == 0 {
                        sp.remove(&(self.source_account_id, p.contract_id.clone()));
                    } else {
                        sp.insert((self.source_account_id, p.contract_id.clone()), net);
                    }
                }
            }
            Err(e) => warn!("startup reconcile: failed to load source positions: {}", e),
        }

        // 2) For each follower, seed follower cache with SIGNED nets and
        //    close any contract not open on leader (leader is flat there)
        for &dest in &self.dest_account_ids {
            match self.dest.search_open_positions(dest).await {
                Ok(res) => {
                    for p in res.positions {
                        let net = if p.r#type == 0 { p.size } else { -p.size };
                        // seed follower cache
                        {
                            let mut dp = self.dest_pos.write().await;
                            if net == 0 {
                                dp.remove(&(dest, p.contract_id.clone()));
                            } else {
                                dp.insert((dest, p.contract_id.clone()), net);
                            }
                        }
                        // If leader is flat on this contract, close follower side
                        if !src_open.contains(&p.contract_id) {
                            info!(
                            "Startup reconcile: closing {} on dest {} (leader flat)",
                            p.contract_id, dest
                        );
                            if let Err(e) = self.dest.close_contract(dest, &p.contract_id).await {
                                warn!(
                                "Failed to close {} on dest {} during startup reconcile: {}",
                                p.contract_id, dest, e
                            );
                            } else {
                                let mut dp = self.dest_pos.write().await;
                                dp.remove(&(dest, p.contract_id.clone()));
                            }
                        }
                    }
                }
                Err(e) => warn!("startup reconcile: failed to load dest {} positions: {}", dest, e),
            }
        }

        // 3) For contracts OPEN on leader, bring followers to the exact leader size
        for contract_id in src_open.iter() {
            self.sync_dest_to_source_for_contract_live(contract_id).await;
        }
    }

    fn side_to_delta(side: i32, size: i32) -> i32 {
        // 0=Bid/buy => +size, 1=Ask/sell => -size
        if side == 0 { size } else { -size }
    }

    async fn update_src_position(&self, contract_id: &str, side: i32, size: i32) -> (i32, i32) {
        let key = (self.source_account_id, contract_id.to_string());
        let mut map = self.src_pos.write().await;
        let prev = *map.get(&key).unwrap_or(&0);
        let next = prev + Self::side_to_delta(side, size);
        if next == 0 { map.remove(&key); } else { map.insert(key, next); }
        (prev, next)
    }

    async fn get_live_dest_net(&self, dest: i32, contract_id: &str) -> i32 {
        // Stop hitting REST for follower position. Rely on in-memory cache which is
        // updated on our own order placements and during startup reconcile.
        // This avoids continuous POST /api/Position/searchOpen calls.
        *self.dest_pos.read().await
            .get(&(dest, contract_id.to_string()))
            .unwrap_or(&0)
    }

    async fn sync_dest_to_source_for_contract_live(&self, contract_id: &str) {
        // Leader net from cache (kept accurate via trade stream + startup seed)
        let src_key = (self.source_account_id, contract_id.to_string());
        let src_net = match self.src_pos.read().await.get(&src_key) {
            None => return,
            Some(net) => *net,
        };

        let max_step: i32 = std::env::var("PX_MAX_RESYNC")
            .ok().and_then(|s| s.parse().ok())
            .unwrap_or(i32::MAX);

        for &dest in &self.dest_account_ids {
            let dest_net = self.get_live_dest_net(dest, contract_id).await;
            let diff = src_net - dest_net;
            if diff == 0 { continue; }

            // Clamp correction per pass if requested
            let step = diff.clamp(-max_step, max_step);
            if step == 0 { continue; }

            // Market order toward parity (0 = buy, 1 = sell)
            let side = if step > 0 { 0 } else { 1 };
            let size = step.abs();

            // Always-unique tag (prevents API error code 2)
            let tag = unique_sync_tag(contract_id, dest, step);

            let req = PlaceOrderReq {
                account_id: dest,
                contract_id,
                r#type: 2, // market
                side,
                size,
                limit_price: None,
                stop_price: None,
                trail_price: None,
                custom_tag: Some(tag),
                linked_order_id: None,
            };

            match self.dest.place_order(&req).await {
                Ok(resp) => {
                    tracing::info!(
                        "Live sync {}: adjusted dest {} by {} (leader net {}) via order {}",
                        contract_id, dest, step, src_net, resp.order_id
                    );
                    // Bring cache to leader net to avoid oscillation
                    let mut dp = self.dest_pos.write().await;
                    if src_net == 0 {
                        dp.remove(&(dest, contract_id.to_string()));
                    } else {
                        dp.insert((dest, contract_id.to_string()), src_net);
                    }
                    // Optionally tighten convergence if you have this helper:
                    // self.converge_contract_quick(contract_id).await;
                }
                Err(e) => {
                    // Other real errors (not the tag one—we preempted that)
                    tracing::warn!(
                        "Live sync {}: failed to adjust dest {} by {}: {}",
                        contract_id, dest, step, e
                    );
                }
            }
        }
    }

    async fn flatten_dest_if_needed(&self, contract_id: &str) {
        // If source is flat on this contract, force flatten all dest accounts for that contract
        let src_key = (self.source_account_id, contract_id.to_string());
        let src_flat = { !self.src_pos.read().await.contains_key(&src_key) };
        if !src_flat { return; }

        let mut to_flatten: Vec<(i32, i32)> = Vec::new();
        {
            let dest_map = self.dest_pos.read().await;
            for &dest in &self.dest_account_ids {
                let key = (dest, contract_id.to_string());
                if let Some(&pos) = dest_map.get(&key) {
                    if pos != 0 { to_flatten.push((dest, pos)); }
                }
            }
        }

        for (dest, pos) in to_flatten {
            let side = if pos > 0 { 1 } else { 0 }; // if long, sell; if short, buy
            let size = pos.abs();
            let tag = format!("TCX:FLATTEN:{}:{}", contract_id, dest);
            let req = PlaceOrderReq {
                account_id: dest,
                contract_id: contract_id,
                r#type: 2,
                side,
                size,
                limit_price: None,
                stop_price: None,
                trail_price: None,
                custom_tag: Some(tag),
                linked_order_id: None,
            };
            match self.dest.place_order(&req).await {
                Ok(resp) => {
                    info!("Flattened acct {} on {} with order {} (pos was {})", dest, contract_id, resp.order_id, pos);
                    // Update in-memory dest position to zero
                    let mut dest_map = self.dest_pos.write().await;
                    dest_map.remove(&(dest, contract_id.to_string()));
                }
                Err(e) => error!("Failed to flatten acct {} on {}: {}", dest, contract_id, e),
            }
        }
    }

    pub(crate) async fn run(&self) -> anyhow::Result<()> {
        info!("TradeCopierX starting (RTC mode)");
        // One-off REST seed; after this we stay off REST in live mode
        self.reconcile_on_startup().await;

        // Channel for follower position updates coming from RTC (GatewayUserPosition)
        let (mut dest_pos_tx, mut dest_pos_rx) = mpsc::channel::<(i32, String, i32)>(1024);

        // Start the RTC user hub (source + followers)
        let mut trade_rx = start_userhub(
            self.src.clone(),
            self.source_account_id,
            self.dest_account_ids.clone(),
            dest_pos_tx.clone(),
        ).await?;

        let mut last_sync = Instant::now();

        loop {
            tokio::select! {
            // ===== Source trades from RTC =====
            maybe_evt = trade_rx.recv() => {
                match maybe_evt {
                    Some(RtcEvent::Trade(tr)) => {
                        // De-dup trades
                        {
                            let mut seen = self.seen.write().await;
                            if !seen.insert(tr.id) { continue; }
                        }
                        // Update leader cache and converge followers
                        let _ = self.update_src_position(&tr.contract_id, tr.side, tr.size).await;
                        self.sync_dest_to_source_for_contract_live(&tr.contract_id).await;
                        self.flatten_dest_if_needed(&tr.contract_id).await;
                        self.sync_dest_to_source_for_contract_live(&tr.contract_id).await;
                    }
                    Some(RtcEvent::SrcPosition { contract_id, signed_net }) => {
                        // Update leader cache from position snapshot and converge followers
                        {
                            let mut sp = self.src_pos.write().await;
                            let key = (self.source_account_id, contract_id.clone());
                            if signed_net == 0 { sp.remove(&key); } else { sp.insert(key, signed_net); }
                        }
                        self.sync_dest_to_source_for_contract_live(&contract_id).await;
                        self.flatten_dest_if_needed(&contract_id).await;
                        self.sync_dest_to_source_for_contract_live(&contract_id).await;
                    }
                    None => {
                        // The RTC task ended (disconnect). Rebuild both the dest-pos channel and the hub.
                        warn!("RTC trade channel closed; rebuilding connection");
                        let (new_tx, new_rx) = mpsc::channel::<(i32, String, i32)>(1024);
                        dest_pos_tx = new_tx;
                        dest_pos_rx = new_rx;
                        trade_rx = start_userhub(
                            self.src.clone(),
                            self.source_account_id,
                            self.dest_account_ids.clone(),
                            dest_pos_tx.clone(),
                        ).await?;
                    }
                }
            }

            // ===== Follower positions from RTC =====
            Some((dest_id, contract_id, signed_net)) = dest_pos_rx.recv() => {
                let mut dp = self.dest_pos.write().await;
                if signed_net == 0 {
                    dp.remove(&(dest_id, contract_id.clone()));
                } else {
                    dp.insert((dest_id, contract_id.clone()), signed_net);
                }
            }

            // ===== Periodic convergence WITHOUT REST =====
            _ = sleep(Duration::from_millis(self.poll_interval_ms)) => {
                if last_sync.elapsed() >= Duration::from_millis(self.sync_interval_ms) {
                    use std::collections::HashSet;
                    let mut all_contracts: HashSet<String> = HashSet::new();
                    {
                        let sp = self.src_pos.read().await;
                        for (k, _) in sp.iter() { all_contracts.insert(k.1.clone()); }
                    }
                    {
                        let dp = self.dest_pos.read().await;
                        for (k, _) in dp.iter() { all_contracts.insert(k.1.clone()); }
                    }
                    for cid in all_contracts.iter() {
                        self.sync_dest_to_source_for_contract_live(cid).await;
                    }
                    last_sync = Instant::now();
                }
            }
        }
        }
    }
}