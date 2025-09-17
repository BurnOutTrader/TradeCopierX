use crate::client::{PxClient, set_live_mode_live};
use crate::models::{PlaceOrderReq, PositionSearchOpenRes};
use crate::rtc::{RtcEvent, start_userhub};
use ahash::AHashMap;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tracing::{info, warn, error};
use std::sync::atomic::{AtomicU64, Ordering};

static ORDER_TAG_SEQ: AtomicU64 = AtomicU64::new(1);

#[inline]
fn unique_sync_tag(contract_id: &str, dest: i32, step: i32) -> String {
    let seq = ORDER_TAG_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("TCX:SYNC-LIVE:{}:{}:{}:{}", contract_id, dest, step, seq)
}

pub struct Copier {
    pub src: Arc<PxClient>,
    pub dest: Arc<PxClient>,
    pub source_account_id: i32,
    pub dest_account_ids: Vec<i32>,

    // In-memory running positions, keyed by (accountId, contractId)
    pub src_pos: RwLock<AHashMap<(i32, String), i32>>,   // source net per contract
    pub dest_pos: RwLock<AHashMap<(i32, String), i32>>,  // follower net per contract
    pub seen: RwLock<HashSet<i64>>,
}

impl Copier {
    pub fn new(src: Arc<PxClient>, dest: Arc<PxClient>, source_account_id: i32, dest_account_ids: Vec<i32>) -> Self {
        Self {
            src, dest, source_account_id, dest_account_ids,
            src_pos: RwLock::new(AHashMap::new()),
            dest_pos: RwLock::new(AHashMap::new()),
            seen: RwLock::new(HashSet::new()),
        }
    }

    fn signed_from_api(dir: i32, size: i32) -> i32 {
        // PositionRecord.type uses PositionType (1=Long,2=Short)
        if dir == 1 { size } else { -size }
    }
    fn side_to_delta(side: i32, size: i32) -> i32 {
        // TradeRecord.side: 0=buy -> +, 1=sell -> -
        if side == 0 { size } else { -size }
    }

    // ===== One-time reconcile with REST =====
    pub async fn reconcile_once(&self) {
        use std::collections::HashSet;
        info!("Startup reconcile: loading source/dest open positions via REST");

        // Source
        let mut leader_open: HashSet<String> = HashSet::new();
        match self.src.search_open_positions(self.source_account_id).await {
            Ok(PositionSearchOpenRes { positions }) => {
                let mut sp = self.src_pos.write().await;
                for p in positions {
                    leader_open.insert(p.contract_id.clone());
                    let net = Self::signed_from_api(p.r#type, p.size);
                    if net == 0 {
                        sp.remove(&(self.source_account_id, p.contract_id.clone()));
                    } else {
                        sp.insert((self.source_account_id, p.contract_id.clone()), net);
                    }
                }
            }
            Err(e) => warn!("Startup: failed to load source positions: {}", e),
        }

        // Followers
        for &dest in &self.dest_account_ids {
            match self.dest.search_open_positions(dest).await {
                Ok(PositionSearchOpenRes { positions }) => {
                    for p in positions {
                        let net = Self::signed_from_api(p.r#type, p.size);
                        {
                            let mut dp = self.dest_pos.write().await;
                            if net == 0 {
                                dp.remove(&(dest, p.contract_id.clone()));
                            } else {
                                dp.insert((dest, p.contract_id.clone()), net);
                            }
                        }
                        // If leader is flat, close follower
                        if !leader_open.contains(&p.contract_id) && net != 0 {
                            info!("Startup: closing {} on follower {} (leader flat)", p.contract_id, dest);
                            if let Err(e) = self.dest.close_contract(dest, &p.contract_id).await {
                                warn!("Startup: close {} on {} failed: {}", p.contract_id, dest, e);
                            } else {
                                let mut dp = self.dest_pos.write().await;
                                dp.remove(&(dest, p.contract_id.clone()));
                            }
                        }
                    }
                }
                Err(e) => warn!("Startup: failed to load dest {} positions: {}", dest, e),
            }
        }

        // Bring followers to exact leader size for all leader-open contracts
        for cid in leader_open {
            self.sync_followers_to_leader(&cid).await;
        }
    }

    async fn get_dest_net_cached(&self, dest: i32, contract_id: &str) -> i32 {
        *self.dest_pos.read().await
            .get(&(dest, contract_id.to_string()))
            .unwrap_or(&0)
    }

    async fn sync_followers_to_leader(&self, contract_id: &str) {
        // Read leader net from cache
        let src_key = (self.source_account_id, contract_id.to_string());
        let src_net = match self.src_pos.read().await.get(&src_key) {
            None => 0,
            Some(v) => *v,
        };

        for &dest in &self.dest_account_ids {
            let dest_net = self.get_dest_net_cached(dest, contract_id).await;
            let diff = src_net - dest_net;
            if diff == 0 { continue; }

            let side = if diff > 0 { 0 } else { 1 };
            let size = diff.abs();

            let tag = unique_sync_tag(contract_id, dest, diff);

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
                    info!("Sync {}: adjusted follower {} by {} -> order {}", contract_id, dest, diff, resp.order_id);
                    // Update follower cache to leader value
                    let mut dp = self.dest_pos.write().await;
                    if src_net == 0 {
                        dp.remove(&(dest, contract_id.to_string()));
                    } else {
                        dp.insert((dest, contract_id.to_string()), src_net);
                    }
                }
                Err(e) => {
                    warn!("Sync {}: place_order follower {} diff {} failed: {}", contract_id, dest, diff, e);
                }
            }
        }
    }

    async fn flatten_followers_if_leader_flat(&self, contract_id: &str) {
        let src_key = (self.source_account_id, contract_id.to_string());
        let leader_flat = { !self.src_pos.read().await.contains_key(&src_key) };
        if !leader_flat { return; }

        // For each follower with non-zero pos, place opposing market order
        let mut to_flatten: Vec<(i32, i32)> = Vec::new();
        {
            let dp = self.dest_pos.read().await;
            for &dest in &self.dest_account_ids {
                let key = (dest, contract_id.to_string());
                if let Some(&pos) = dp.get(&key) {
                    if pos != 0 { to_flatten.push((dest, pos)); }
                }
            }
        }

        for (dest, pos) in to_flatten {
            let side = if pos > 0 { 1 } else { 0 };
            let size = pos.abs();
            let tag = format!("TCX:FLATTEN:{}:{}", contract_id, dest);
            let req = PlaceOrderReq {
                account_id: dest,
                contract_id,
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
                    info!("Flatten follower {} on {} (pos {}) via order {}", dest, contract_id, pos, resp.order_id);
                    let mut dp = self.dest_pos.write().await;
                    dp.remove(&(dest, contract_id.to_string()));
                }
                Err(e) => error!("Flatten follower {} on {} failed: {}", dest, contract_id, e),
            }
        }
    }

    // ===== Main loop: RTC-only, no REST polling =====
    pub async fn run(&self) -> anyhow::Result<()> {
        info!("TradeCopierX starting");
        self.reconcile_once().await;

        // From here on, **no more REST searches**:
        set_live_mode_live();

        // Start RTC
        let mut rx = start_userhub(
            self.src.clone(),
            self.source_account_id,
            self.dest_account_ids.clone(),
        ).await?;

        loop {
            tokio::select! {
                maybe_evt = rx.recv() => {
                    match maybe_evt {
                        Some(RtcEvent::Trade(tr)) => {
                            // De-dup source trades
                            {
                                let mut seen = self.seen.write().await;
                                if !seen.insert(tr.id) { continue; }
                            }
                            // Update source position cache
                            let key = (self.source_account_id, tr.contract_id.clone());
                            let mut sp = self.src_pos.write().await;
                            let prev = *sp.get(&key).unwrap_or(&0);
                            let next = prev + Self::side_to_delta(tr.side, tr.size);
                            if next == 0 { sp.remove(&key); } else { sp.insert(key, next); }
                            drop(sp);

                            // Converge followers
                            self.sync_followers_to_leader(&tr.contract_id).await;
                            self.flatten_followers_if_leader_flat(&tr.contract_id).await;
                            self.sync_followers_to_leader(&tr.contract_id).await;
                        }
                        Some(RtcEvent::SrcPosition { contract_id, signed_net }) => {
                            let mut sp = self.src_pos.write().await;
                            let key = (self.source_account_id, contract_id.clone());
                            if signed_net == 0 { sp.remove(&key); } else { sp.insert(key, signed_net); }
                            drop(sp);
                            self.sync_followers_to_leader(&contract_id).await;
                            self.flatten_followers_if_leader_flat(&contract_id).await;
                            self.sync_followers_to_leader(&contract_id).await;
                        }
                        Some(RtcEvent::DestPosition { account_id, contract_id, signed_net }) => {
                            if self.dest_account_ids.iter().any(|&d| d == account_id) {
                                let mut dp = self.dest_pos.write().await;
                                if signed_net == 0 {
                                    dp.remove(&(account_id, contract_id.clone()));
                                } else {
                                    dp.insert((account_id, contract_id.clone()), signed_net);
                                }
                            }
                        }
                        None => {
                            warn!("RTC channel closed; recreating");
                            rx = start_userhub(
                                self.src.clone(),
                                self.source_account_id,
                                self.dest_account_ids.clone(),
                            ).await?;
                        }
                    }
                }
            }
        }
    }
}