use crate::client::PxClient;
use crate::models::{PlaceOrderReq, PositionRecord};
use ahash::AHashMap;
use anyhow::Result;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

static ORDER_TAG_SEQ: AtomicU64 = AtomicU64::new(1);

#[inline]
fn unique_sync_tag(contract_id: &str, dest: i32, step: i32) -> String {
    let seq = ORDER_TAG_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("TCX:SYNC:{}:{}:{}:{}", contract_id, dest, step, seq)
}

pub struct Copier {
    pub src: Arc<PxClient>,
    pub dests: Vec<Arc<PxClient>>, // support multiple follower firms/clients
    pub source_account_id: i32,
    pub dest_account_ids: Vec<i32>,
    pub max_resync_step: i32,
    pub source_poll_ms: u64,
    pub enable_follower_drift_check: bool,

    // In-memory running positions, keyed by (accountId, contractId)
    pub src_pos: Arc<RwLock<AHashMap<(i32, String), i32>>>,   // source net position per contract
    pub dest_pos: Arc<RwLock<AHashMap<(i32, String), i32>>>,  // destination net position per contract
}

impl Copier {
    pub fn new(
        src: Arc<PxClient>,
        dests: Vec<Arc<PxClient>>,
        source_account_id: i32,
        dest_account_ids: Vec<i32>,
        max_resync_step: i32,
        source_poll_ms: u64,
        enable_follower_drift_check: bool,
    ) -> Self {
        Self {
            src, dests, source_account_id, dest_account_ids, max_resync_step, source_poll_ms,
            enable_follower_drift_check,
            src_pos: Arc::new(RwLock::new(AHashMap::new())),
            dest_pos: Arc::new(RwLock::new(AHashMap::new())),
        }
    }

    #[inline]
    fn signed_net_from_record(p: &PositionRecord) -> i32 {
        // PositionType per docs: 1=Long, 2=Short
        match p.r#type { 1 => p.size, 2 => -p.size, _ => 0 }
    }

    pub async fn reconcile_on_startup(&self) {
        info!("Startup reconcile: begin");

        // 1) Load leader open positions; seed src cache
        let mut src_open: HashSet<String> = HashSet::new();
        match self.src.search_open_positions(self.source_account_id).await {
            Ok(res) => {
                let mut sp = self.src_pos.write().await;
                sp.clear();
                for p in res.positions {
                    src_open.insert(p.contract_id.clone());
                    let net = Self::signed_net_from_record(&p);
                    if net != 0 {
                        sp.insert((self.source_account_id, p.contract_id.clone()), net);
                    }
                }
            }
            Err(e) => warn!("Startup reconcile: failed to load source positions: {}", e),
        }

        // 2) For each follower (paired by index), seed follower cache and close any contract leader is flat on
        let n = self.dests.len().min(self.dest_account_ids.len());
        for i in 0..n {
            let client = &self.dests[i];
            let dest = self.dest_account_ids[i];
            match client.search_open_positions(dest).await {
                Ok(res) => {
                    // seed cache
                    {
                        let mut dp = self.dest_pos.write().await;
                        // clear only this account's entries
                        dp.retain(|(acc, _), _| *acc != dest);
                        for p in &res.positions {
                            let net = Self::signed_net_from_record(p);
                            if net != 0 {
                                dp.insert((dest, p.contract_id.clone()), net);
                            }
                        }
                    }
                    // if leader flat on contract, close follower side
                    for p in res.positions {
                        if !src_open.contains(&p.contract_id) {
                            let net = Self::signed_net_from_record(&p);
                            if net != 0 {
                                info!("Startup reconcile: closing {} on dest {} (leader flat)", p.contract_id, dest);
                                if let Err(e) = client.close_contract(dest, &p.contract_id).await {
                                    warn!("Failed to close {} on dest {}: {}", p.contract_id, dest, e);
                                } else {
                                    let mut dp = self.dest_pos.write().await;
                                    dp.remove(&(dest, p.contract_id.clone()));
                                }
                            }
                        }
                    }
                }
                Err(e) => warn!("Startup reconcile: failed to load dest {} positions: {}", dest, e),
            }
        }

        // 3) For contracts open on leader, bring followers to leader size
        for contract_id in src_open.iter() {
            self.sync_dest_to_source_for_contract(contract_id).await;
        }

        info!("Startup reconcile: complete");
    }

    #[inline]
    fn delta(side: i32, size: i32) -> i32 {
        // 0=Bid/buy => +size, 1=Ask/sell => -size
        if side == 0 { size } else { -size }
    }

    async fn set_src_snapshot(&self, snapshot: &[(String, i32)]) {
        let mut sp = self.src_pos.write().await;
        // replace everything for source account
        sp.retain(|(acc, _), _| *acc != self.source_account_id);
        for (cid, net) in snapshot {
            if *net != 0 {
                sp.insert((self.source_account_id, cid.clone()), *net);
            }
        }
    }

    async fn get_dest_net_cached(&self, dest: i32, contract_id: &str) -> i32 {
        *self.dest_pos.read().await
            .get(&(dest, contract_id.to_string()))
            .unwrap_or(&0)
    }

    pub async fn sync_dest_to_source_for_contract(&self, contract_id: &str) {
        let src_key = (self.source_account_id, contract_id.to_string());
        let src_net = match self.src_pos.read().await.get(&src_key) {
            None => 0,
            Some(net) => *net,
        };

        let n = self.dests.len().min(self.dest_account_ids.len());
        for i in 0..n {
            let dest = self.dest_account_ids[i];
            let dest_net = self.get_dest_net_cached(dest, contract_id).await;
            let diff = src_net - dest_net;
            if diff == 0 { continue; }

            let step = diff.clamp(-self.max_resync_step, self.max_resync_step);
            if step == 0 { continue; }
            let side = if step > 0 { 0 } else { 1 };
            let size = step.abs();
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

            let client = &self.dests[i];
            match client.place_order(&req).await {
                Ok(resp) => {
                    info!(
                        "Sync {}: adjusted dest {} by {} (leader net {}) via order {}",
                        contract_id, dest, step, src_net, resp.order_id
                    );
                    // set follower cache to leader net after order success
                    let mut dp = self.dest_pos.write().await;
                    if src_net == 0 {
                        dp.remove(&(dest, contract_id.to_string()));
                    } else {
                        dp.insert((dest, contract_id.to_string()), src_net);
                    }
                }
                Err(e) => {
                    warn!("Sync {}: failed to adjust dest {} by {}: {}", contract_id, dest, step, e);
                }
            }
        }
    }

    pub async fn flatten_dest_if_needed(&self, contract_id: &str) {
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
            // find paired client index for this dest
            if let Some(i) = self.dest_account_ids.iter().position(|&d| d == dest) {
                let client = &self.dests[i];
                match client.place_order(&req).await {
                    Ok(resp) => {
                        info!("Flattened acct {} on {} with order {} (pos was {})", dest, contract_id, resp.order_id, pos);
                        let mut dest_map = self.dest_pos.write().await;
                        dest_map.remove(&(dest, contract_id.to_string()));
                    }
                    Err(e) => error!("Failed to flatten acct {} on {}: {}", dest, contract_id, e),
                }
            } else {
                warn!("No client paired for destination account {} during flatten", dest);
            }
        }
    }

    /// REST-only loop:
    /// - Poll leader open positions every `source_poll_ms`
    /// - Compute deltas vs last snapshot
    /// - Sync followers to leader net using market orders
    /// - (Optional) follower drift check is available but disabled by default
    pub async fn run_rest_only(&self) -> Result<()> {
        info!("TradeCopierX starting (REST-only mode). Polling leader positions every {} ms", self.source_poll_ms);

        // Initial last snapshot equals startup cache
        let mut last_snapshot: AHashMap<String, i32> = {
            let mut map = AHashMap::new();
            let sp = self.src_pos.read().await;
            for ((acc, cid), net) in sp.iter() {
                if *acc == self.source_account_id {
                    map.insert(cid.clone(), *net);
                }
            }
            map
        };

        // Optional follower drift checker (OFF by default)
        if self.enable_follower_drift_check {
            let dest_clients = self.dests.clone();
            let dest_ids = self.dest_account_ids.clone();
            let dest_pos = self.dest_pos.clone();
            tokio::spawn(async move {
                loop {
                    let n = dest_clients.len().min(dest_ids.len());
                    for i in 0..n {
                        let client = &dest_clients[i];
                        let dest = dest_ids[i];
                        match client.search_open_positions(dest).await {
                            Ok(res) => {
                                let mut dp = dest_pos.write().await;
                                dp.retain(|(acc, _), _| *acc != dest);
                                for p in res.positions {
                                    let net = match p.r#type { 1 => p.size, 2 => -p.size, _ => 0 };
                                    if net != 0 {
                                        dp.insert((dest, p.contract_id.clone()), net);
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("Follower drift check (dest {}): {}", dest, e);
                            }
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
            });
        }

        loop {
            // 1) Poll leader positions
            match self.src.search_open_positions(self.source_account_id).await {
                Ok(res) => {
                    // Build current snapshot map<contractId, signedNet>
                    let mut current: AHashMap<String, i32> = AHashMap::new();
                    for p in res.positions {
                        let net = Self::signed_net_from_record(&p);
                        if net != 0 {
                            current.insert(p.contract_id.clone(), net);
                        }
                    }

                    // Update in-memory src cache to current snapshot
                    let snapshot_vec: Vec<(String, i32)> = current.iter().map(|(k, v)| (k.clone(), *v)).collect();
                    self.set_src_snapshot(&snapshot_vec).await;

                    // 2) For each observed contract, sync followers
                    //    (Union of old+current keys handles both opens and closes)
                    let mut union: HashSet<String> = HashSet::new();
                    for k in current.keys() { union.insert(k.clone()); }
                    for k in last_snapshot.keys() { union.insert(k.clone()); }

                    for cid in union {
                        self.sync_dest_to_source_for_contract(&cid).await;
                        self.flatten_dest_if_needed(&cid).await;
                    }

                    // 3) Save current as last
                    last_snapshot = current;
                }
                Err(e) => {
                    warn!("Leader poll failed: {}", e);
                }
            }

            // 4) Sleep to respect rate budget (default 1500 ms)
            tokio::time::sleep(std::time::Duration::from_millis(self.source_poll_ms)).await;
        }
    }
}