use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use chrono::Utc;
use tokio::time::sleep;
use crate::client::PxClient;
use crate::models::{PlaceOrderReq, TradeSearchReq};
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

        // 1) Build set of contracts that are OPEN on the leader
        let mut src_open: HashSet<String> = HashSet::new();
        match self.src.search_open_positions(self.source_account_id).await {
            Ok(res) => {
                for p in res.positions {
                    src_open.insert(p.contract_id.clone());
                    // Seed source pos cache
                    let mut sp = self.src_pos.write().await;
                    sp.insert((self.source_account_id, p.contract_id.clone()), p.size);
                }
            }
            Err(e) => warn!("startup reconcile: failed to load source positions: {}", e),
        }

        // 2) For each follower, close any contract not open on leader
        for &dest in &self.dest_account_ids {
            match self.dest.search_open_positions(dest).await {
                Ok(res) => {
                    for p in res.positions {
                        // seed follower cache
                        {
                            let mut dp = self.dest_pos.write().await;
                            dp.insert((dest, p.contract_id.clone()), p.size);
                        }
                        if !src_open.contains(&p.contract_id) {
                            info!("Startup reconcile: closing {} on dest {} (leader flat)", p.contract_id, dest);
                            if let Err(e) = self.dest.close_contract(dest, &p.contract_id).await {
                                warn!("Failed to close {} on dest {} during startup reconcile: {}", p.contract_id, dest, e);
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

        // For contracts open on leader, ensure followers match leader size
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

    async fn update_dest_position(&self, dest_acct: i32, contract_id: &str, side: i32, size: i32) -> i32 {
        let key = (dest_acct, contract_id.to_string());
        let mut map = self.dest_pos.write().await;
        let prev = *map.get(&key).unwrap_or(&0);
        let next = prev + Self::side_to_delta(side, size);
        if next == 0 { map.remove(&key); } else { map.insert(key, next); }
        next
    }
    async fn get_live_dest_net(&self, dest: i32, contract_id: &str) -> i32 {
        match self.dest.search_open_positions(dest).await {
            Ok(res) => {
                for p in res.positions {
                    if p.contract_id == contract_id { return p.size; }
                }
                0
            }
            Err(_) => {
                // fallback to cache if API errors
                *self.dest_pos.read().await.get(&(dest, contract_id.to_string())).unwrap_or(&0)
            }
        }
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
        info!("TradeCopierX starting");
        // A) Flatten stragglers first (don’t replay history)
        self.reconcile_on_startup().await;

        // B) Only consume trades from now onwards
        let mut since_rfc3339: Option<String> = Some(Utc::now().to_rfc3339());

        let mut last_sync = std::time::Instant::now();

        loop {
            // 1) Poll source trades
            let req = TradeSearchReq {
                account_id: self.source_account_id,
                start_timestamp: since_rfc3339.as_deref(),
                end_timestamp: None,
            };
            match self.src.search_trades(&req).await {
                Ok(res) => {
                    for tr in res.trades {
                        // avoid duplicates
                        let mut seen = self.seen.write().await;
                        if !seen.insert(tr.id) { continue; }
                        drop(seen);

                        // Update source net position and, if now flat on this contract, trigger follower flattening
                        let (_prev_src, _next_src) = self.update_src_position(&tr.contract_id, tr.side, tr.size).await;

                        // 2) Mirror to destination accounts
                        for &dest in &self.dest_account_ids {
                            if dest == tr.account_id { continue; }
                            let tag = format!("TCX:{}:{}", tr.id, dest); // unique per dest
                            let place = PlaceOrderReq {
                                account_id: dest,
                                contract_id: &tr.contract_id,
                                r#type: 2, // market (simple MVP)
                                side: tr.side,
                                size: tr.size,
                                limit_price: None,
                                stop_price: None,
                                trail_price: None,
                                custom_tag: Some(tag),
                                linked_order_id: None,
                            };
                            match self.dest.place_order(&place).await {
                                Ok(resp) => {
                                    info!("Mirrored trade {} to acct {} as order {}", tr.id, dest, resp.order_id);
                                    // Track dest running position
                                    let _ = self.update_dest_position(dest, &tr.contract_id, tr.side, tr.size).await;
                                },
                                Err(e) => error!("Failed to mirror trade {} to acct {}: {}", tr.id, dest, e),
                            }
                        }

                        // If the leader is flat on this contract, ensure followers are flat too
                        self.flatten_dest_if_needed(&tr.contract_id).await;

                        // Keep followers exactly matched to leader net for this contract
                        self.sync_dest_to_source_for_contract_live(&tr.contract_id).await;

                        // advance watermark
                        since_rfc3339 = Some(tr.creation_timestamp.clone());
                    }
                }
                Err(e) => {
                    warn!("trade search error: {}", e);
                }
            }

            if last_sync.elapsed() >= Duration::from_millis(self.sync_interval_ms) {
                // Build a set of all contracts we have seen either on leader or followers
                let mut all_contracts: std::collections::HashSet<String> = std::collections::HashSet::new();
                {
                    let sp = self.src_pos.read().await; for (_k, _v) in sp.iter() { all_contracts.insert(_k.1.clone()); }
                }
                {
                    let dp = self.dest_pos.read().await; for (_k, _v) in dp.iter() { all_contracts.insert(_k.1.clone()); }
                }
                for cid in all_contracts.iter() {
                    self.sync_dest_to_source_for_contract_live(cid).await;
                }
                last_sync = std::time::Instant::now();
            }

            sleep(Duration::from_millis(self.poll_interval_ms)).await;
        }
    }
}