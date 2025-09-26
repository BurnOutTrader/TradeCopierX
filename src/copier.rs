use crate::client::PxClient;
use crate::models::{PlaceOrderReq, PositionRecord, OrderRecord, ModifyOrderReq};
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
    pub enable_order_copy: bool,
    pub order_poll_ms: u64,

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
        enable_order_copy: bool,
        order_poll_ms: u64,
    ) -> Self {
        Self {
            src, dests, source_account_id, dest_account_ids, max_resync_step, source_poll_ms,
            enable_follower_drift_check,
            enable_order_copy,
            order_poll_ms,
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

        // Optional: order mirroring loop (OFF by default)
        if self.enable_order_copy {
            let src = self.src.clone();
            let source_account_id = self.source_account_id;
            let dest_clients = self.dests.clone();
            let dest_ids = self.dest_account_ids.clone();
            let poll_ms = self.order_poll_ms;

            #[derive(Clone, Debug, PartialEq)]
            struct OrderSnap {
                contract_id: String,
                r#type: i32,
                side: i32,
                size: i32,
                limit_price: Option<f64>,
                stop_price: Option<f64>,
                trail_price: Option<f64>,
            }
            fn snap_from(o: &OrderRecord) -> OrderSnap {
                OrderSnap {
                    contract_id: o.contract_id.clone(),
                    r#type: o.r#type,
                    side: o.side,
                    size: o.size,
                    limit_price: o.limit_price,
                    stop_price: o.stop_price,
                    trail_price: o.trail_price,
                }
            }

            tokio::spawn(async move {
                use ahash::AHashMap;
                let mut last: AHashMap<i64, OrderSnap> = AHashMap::new();
                let mut map_dest_order: AHashMap<(i32, i64), i64> = AHashMap::new();
                loop {
                    match src.search_open_orders(source_account_id).await {
                        Ok(res) => {
                            let mut current: AHashMap<i64, OrderSnap> = AHashMap::new();
                            for o in res.orders {
                                current.insert(o.id, snap_from(&o));
                            }

                            // New orders
                            for (oid, snap) in current.iter() {
                                if !last.contains_key(oid) {
                                    let n = dest_clients.len().min(dest_ids.len());
                                    for i in 0..n {
                                        let dest = dest_ids[i];
                                        let client = &dest_clients[i];
                                        let tag = Some(format!("TCX:ORD:{}", oid));
                                        let req = PlaceOrderReq {
                                            account_id: dest,
                                            contract_id: &snap.contract_id,
                                            r#type: snap.r#type,
                                            side: snap.side,
                                            size: snap.size,
                                            limit_price: snap.limit_price,
                                            stop_price: snap.stop_price,
                                            trail_price: snap.trail_price,
                                            custom_tag: tag,
                                            linked_order_id: None,
                                        };
                                        match client.place_order(&req).await {
                                            Ok(r) => {
                                                info!("Order copy: placed dest {} for leader order {} => dest order {}", dest, oid, r.order_id);
                                                map_dest_order.insert((dest, *oid), r.order_id);
                                            }
                                            Err(e) => warn!("Order copy: failed to place for leader order {} on dest {}: {}", oid, dest, e),
                                        }
                                    }
                                }
                            }

                            // Removed orders
                            for (oid, _snap) in last.iter() {
                                if !current.contains_key(oid) {
                                    let n = dest_clients.len().min(dest_ids.len());
                                    for i in 0..n {
                                        let dest = dest_ids[i];
                                        if let Some(&d_oid) = map_dest_order.get(&(dest, *oid)) {
                                            let client = &dest_clients[i];
                                            if let Err(e) = client.cancel_order(dest, d_oid).await {
                                                warn!("Order copy: failed to cancel dest {} for leader order {}: {}", dest, oid, e);
                                            } else {
                                                info!("Order copy: cancelled dest {} order {} (leader order {} gone)", dest, d_oid, oid);
                                                map_dest_order.remove(&(dest, *oid));
                                            }
                                        }
                                    }
                                }
                            }

                            // Modified orders
                            for (oid, snap_now) in current.iter() {
                                if let Some(prev) = last.get(oid) {
                                    if prev != snap_now {
                                        let n = dest_clients.len().min(dest_ids.len());
                                        for i in 0..n {
                                            let dest = dest_ids[i];
                                            let client = &dest_clients[i];
                                            if let Some(&d_oid) = map_dest_order.get(&(dest, *oid)) {
                                                let mreq = ModifyOrderReq {
                                                    account_id: dest,
                                                    order_id: d_oid,
                                                    size: Some(snap_now.size),
                                                    limit_price: snap_now.limit_price,
                                                    stop_price: snap_now.stop_price,
                                                    trail_price: snap_now.trail_price,
                                                };
                                                match client.modify_order(&mreq).await {
                                                    Ok(()) => info!("Order copy: modified dest {} order {} to mirror leader {}", dest, d_oid, oid),
                                                    Err(e) => warn!("Order copy: modify failed on dest {} order {}: {}", dest, d_oid, e),
                                                }
                                            } else {
                                                // No mapping (e.g., restart) — place new
                                                let tag = Some(format!("TCX:ORD:{}", oid));
                                                let req = PlaceOrderReq {
                                                    account_id: dest,
                                                    contract_id: &snap_now.contract_id,
                                                    r#type: snap_now.r#type,
                                                    side: snap_now.side,
                                                    size: snap_now.size,
                                                    limit_price: snap_now.limit_price,
                                                    stop_price: snap_now.stop_price,
                                                    trail_price: snap_now.trail_price,
                                                    custom_tag: tag,
                                                    linked_order_id: None,
                                                };
                                                if let Ok(r) = client.place_order(&req).await {
                                                    map_dest_order.insert((dest, *oid), r.order_id);
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            last = current;
                        }
                        Err(e) => warn!("Order copy: leader order poll failed: {}", e),
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
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