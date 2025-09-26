use crate::client::PxClient;
use crate::models::{PlaceOrderReq, PositionRecord, OrderRecord, ModifyOrderReq};
use ahash::AHashMap;
use anyhow::Result;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use std::time::{Instant, Duration};

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
    pub order_catchup_ms: u64,

    // In-memory running positions, keyed by (accountId, contractId)
    pub src_pos: Arc<RwLock<AHashMap<(i32, String), i32>>>,   // source net position per contract
    pub dest_pos: Arc<RwLock<AHashMap<(i32, String), i32>>>,  // destination net position per contract

    // Track leader stability per contract: (last_net, consecutive_count)
    pub src_stable: Arc<RwLock<AHashMap<String, (i32, u32)>>>,

    // Track recent leader fills per contract to prevent immediate opposite re-entry
    // value = (filled_side: 0=Buy,1=Sell, timestamp)
    pub src_recent_fills: Arc<RwLock<AHashMap<String, (i32, Instant)>>>, 

    // Track open mirrored orders per destination+contract to pause market-based sync while pending
    pub open_mirrored: Arc<RwLock<AHashMap<(i32, String), usize>>>,
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
        order_catchup_ms: u64,
    ) -> Self {
        Self {
            src, dests, source_account_id, dest_account_ids, max_resync_step, source_poll_ms,
            enable_follower_drift_check,
            enable_order_copy,
            order_poll_ms,
            order_catchup_ms,
            src_pos: Arc::new(RwLock::new(AHashMap::new())),
            dest_pos: Arc::new(RwLock::new(AHashMap::new())),
            src_stable: Arc::new(RwLock::new(AHashMap::new())),
            src_recent_fills: Arc::new(RwLock::new(AHashMap::new())), 
            open_mirrored: Arc::new(RwLock::new(AHashMap::new())),
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

            // If we're mirroring orders and have open mapped orders for this dest+contract, pause market sync.
            if self.enable_order_copy {
                let pause = {
                    let om = self.open_mirrored.read().await;
                    om.get(&(dest, contract_id.to_string())).copied().unwrap_or(0) > 0
                };
                if pause {
                    info!("Sync pause: pending mirrored orders on {} for dest {}, skipping market sync", contract_id, dest);
                    continue;
                }
            }

            let diff = src_net - dest_net;
            if diff == 0 { continue; }

            let step = diff.clamp(-self.max_resync_step, self.max_resync_step);
            if step == 0 { continue; }

            // Recent-fill lockout: if this action would increase absolute exposure in the
            // opposite direction right after a leader protective fill, skip for a short window.
            if self.enable_order_copy {
                let pre_abs = dest_net.abs();
                let post_abs = (dest_net + step).abs();
                if post_abs > pre_abs {
                    let step_side = if step > 0 { 0 } else { 1 }; // 0=buy,1=sell
                    let block = {
                        let rf = self.src_recent_fills.read().await;
                        if let Some((filled_side, ts)) = rf.get(contract_id) {
                            let within = Instant::now().saturating_duration_since(*ts) < Duration::from_millis(self.order_catchup_ms);
                            // block opening in the opposite direction immediately after protective fill
                            within && ((step_side == 0 && *filled_side == 1) || (step_side == 1 && *filled_side == 0))
                        } else { false }
                    };
                    if block {
                        info!("Recent-fill lockout: deferring {} step {} on dest {} (dest_net {}, src_net {})", contract_id, step, dest, dest_net, src_net);
                        continue;
                    }
                }
            }

            // Anti-churn stability gate: when ORDER_COPY is enabled, avoid increasing follower exposure
            // unless the leader net has been stable for at least 2 consecutive polls.
            if self.enable_order_copy {
                let pre_abs = dest_net.abs();
                let post_abs = (dest_net + step).abs();
                if post_abs > pre_abs {
                    let stable_ok = {
                        let st = self.src_stable.read().await;
                        if let Some((last_net, cnt)) = st.get(contract_id) {
                            *last_net == src_net && *cnt >= 2
                        } else { false }
                    };
                    if !stable_ok {
                        info!("Stability gate: deferring increase on {} for dest {} (src_net {}, dest_net {}, step {})", contract_id, dest, src_net, dest_net, step);
                        continue;
                    }
                }
            }

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
                stop_loss_bracket: None,
                take_profit_bracket: None,
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
                stop_loss_bracket: None,
                take_profit_bracket: None,
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
            let src_pos = self.src_pos.clone();
            let dest_pos = self.dest_pos.clone();
            let order_catchup_ms = self.order_catchup_ms;
            let src_stable = self.src_stable.clone();
            let src_recent_fills = self.src_recent_fills.clone();
            let open_mirrored = self.open_mirrored.clone();

            #[derive(Clone, Debug, PartialEq)]
            struct OrderSnap {
                contract_id: String,
                r#type: i32,
                side: i32,
                size: i32,
                limit_price: Option<f64>,
                stop_price: Option<f64>,
                trail_price: Option<f64>,
                linked_order_id: Option<i64>,
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
                    linked_order_id: o.linked_order_id,
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

                            // New orders (two-pass: unlinked first, then linked if parent mapped)
                            let mut new_unlinked: Vec<(i64, OrderSnap)> = Vec::new();
                            let mut new_linked: Vec<(i64, OrderSnap)> = Vec::new();
                            for (oid, snap) in current.iter() {
                                if !last.contains_key(oid) {
                                    if snap.linked_order_id.is_some() {
                                        new_linked.push((*oid, snap.clone()));
                                    } else {
                                        new_unlinked.push((*oid, snap.clone()));
                                    }
                                }
                            }
                            let n = dest_clients.len().min(dest_ids.len());
                            // Pass 1: place unlinked orders
                            for (oid, snap) in new_unlinked.into_iter() {
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
                                        stop_loss_bracket: None,
                                        take_profit_bracket: None,
                                    };
                                    match client.place_order(&req).await {
                                        Ok(r) => {
                                            info!("Order copy: placed dest {} for leader order {} => dest order {}", dest, oid, r.order_id);
                                            map_dest_order.insert((dest, oid), r.order_id);
                                            {
                                                let mut om = open_mirrored.write().await;
                                                *om.entry((dest, snap.contract_id.clone())).or_insert(0) += 1;
                                            }
                                        }
                                        Err(e) => warn!("Order copy: failed to place for leader order {} on dest {}: {}", oid, dest, e),
                                    }
                                }
                            }
                            // Pass 2: place linked orders only when parent mapping exists
                            for (oid, snap) in new_linked.into_iter() {
                                for i in 0..n {
                                    let dest = dest_ids[i];
                                    let client = &dest_clients[i];
                                    if let Some(parent_leader_id) = snap.linked_order_id {
                                        if let Some(&parent_dest_id) = map_dest_order.get(&(dest, parent_leader_id)) {
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
                                                linked_order_id: Some(parent_dest_id),
                                                stop_loss_bracket: None,
                                                take_profit_bracket: None,
                                            };
                                            match client.place_order(&req).await {
                                                Ok(r) => {
                                                    info!("Order copy: placed LINKED dest {} for leader order {} (parent {}) => dest order {} (parent {})",
                                                        dest, oid, parent_leader_id, r.order_id, parent_dest_id);
                                                    map_dest_order.insert((dest, oid), r.order_id);
                                                    {
                                                        let mut om = open_mirrored.write().await;
                                                        *om.entry((dest, snap.contract_id.clone())).or_insert(0) += 1;
                                                    }
                                                }
                                                Err(e) => warn!("Order copy: failed to place linked order {} on dest {}: {}", oid, dest, e),
                                            }
                                        } else {
                                            // Parent not mapped yet on this dest; place without linkage to ensure protective leg exists
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
                                                stop_loss_bracket: None,
                                                take_profit_bracket: None,
                                            };
                                            match client.place_order(&req).await {
                                                Ok(r) => {
                                                    info!("Order copy: placed UNLINKED protective order on dest {} for leader {} => dest order {} (parent {} not mapped)", dest, oid, r.order_id, parent_leader_id);
                                                    map_dest_order.insert((dest, oid), r.order_id);
                                                    {
                                                        let mut om = open_mirrored.write().await;
                                                        *om.entry((dest, snap.contract_id.clone())).or_insert(0) += 1;
                                                    }
                                                }
                                                Err(e) => warn!("Order copy: failed to place unlinked protective order {} on dest {}: {}", oid, dest, e),
                                            }
                                        }
                                    }
                                }
                            }

                            // Pull recent fills from history to detect leader-filled orders
                            let mut filled_leader_oids: std::collections::HashSet<i64> = std::collections::HashSet::new();
                            // Maintain a sliding window based on local time
                            use chrono::{Duration, SecondsFormat, Utc};
                            static mut LAST_HIST_TS: Option<chrono::DateTime<chrono::Utc>> = None;
                            let start_ts = unsafe {
                                let prev = LAST_HIST_TS.unwrap_or_else(|| Utc::now() - Duration::milliseconds(poll_ms as i64));
                                // slight overlap to avoid gaps
                                (prev - Duration::milliseconds(200))
                            };
                            let start_str = start_ts.to_rfc3339_opts(SecondsFormat::Micros, true);
                            match src.search_orders(source_account_id, &start_str, None).await {
                                Ok(hist) => {
                                    for o in hist.orders {
                                        if o.status == 2 || o.fill_volume.unwrap_or(0) > 0 {
                                            filled_leader_oids.insert(o.id);
                                        }
                                    }
                                    unsafe { LAST_HIST_TS = Some(Utc::now()); }
                                }
                                Err(e) => {
                                    warn!("Order copy: Order/search failed: {}", e);
                                }
                            }

                            // Removed orders
                            for (oid, snap_prev) in last.iter() {
                                if !current.contains_key(oid) {
                                    let n = dest_clients.len().min(dest_ids.len());
                                    for i in 0..n {
                                        let dest = dest_ids[i];
                                        if let Some(&d_oid) = map_dest_order.get(&(dest, *oid)) {
                                            let client = &dest_clients[i];

                                            // If the leader removal corresponds to a fill, schedule catch-up: cancel + market-sync after delay
                                            if filled_leader_oids.contains(oid) {
                                                // Record recent leader fill to avoid opposite-direction re-entry
                                                {
                                                    let mut rf = src_recent_fills.write().await;
                                                    rf.insert(snap_prev.contract_id.clone(), (snap_prev.side, Instant::now()));
                                                }
                                                let contract = snap_prev.contract_id.clone();
                                                let src_pos_clone = src_pos.clone();
                                                let dest_pos_clone = dest_pos.clone();
                                                let client_clone = client.clone();
                                                let dest_id_clone = dest;
                                                let source_account_id_clone = source_account_id;
                                                let order_id_clone = d_oid;
                                                let src_stable_clone = src_stable.clone();
                                                let src_recent_fills_clone = src_recent_fills.clone();
                                                let open_mirrored_clone = open_mirrored.clone();
                                                tokio::spawn(async move {
                                                    tokio::time::sleep(std::time::Duration::from_millis(order_catchup_ms)).await;
                                                    // attempt to cancel the lingering follower order
                                                    let _ = client_clone.cancel_order(dest_id_clone, order_id_clone).await;
                                                    {
                                                        let mut om = open_mirrored_clone.write().await;
                                                        if let Some(cnt) = om.get_mut(&(dest_id_clone, contract.clone())) {
                                                            if *cnt > 0 { *cnt -= 1; }
                                                            if *cnt == 0 { om.remove(&(dest_id_clone, contract.clone())); }
                                                        }
                                                    }
                                                    // compute diff vs leader and market-sync if needed
                                                    let leader_net = {
                                                        let sp = src_pos_clone.read().await;
                                                        *sp.get(&(source_account_id_clone, contract.clone())).unwrap_or(&0)
                                                    };
                                                    let dest_net = {
                                                        let dp = dest_pos_clone.read().await;
                                                        *dp.get(&(dest_id_clone, contract.clone())).unwrap_or(&0)
                                                    };
                                                    let diff = leader_net - dest_net;
                                                    if diff != 0 {
                                                        // Apply recent-fill lockout first to avoid opposite-direction re-entry immediately after fill
                                                        let pre_abs = dest_net.abs();
                                                        let post_abs = (dest_net + diff).abs();
                                                        if post_abs > pre_abs {
                                                            let step_side = if diff > 0 { 0 } else { 1 };
                                                            let block = {
                                                                let rf = src_recent_fills_clone.read().await;
                                                                if let Some((filled_side, ts)) = rf.get(&contract) {
                                                                    let within = Instant::now().saturating_duration_since(*ts) < std::time::Duration::from_millis(order_catchup_ms);
                                                                    within && ((step_side == 0 && *filled_side == 1) || (step_side == 1 && *filled_side == 0))
                                                                } else { false }
                                                            };
                                                            if block {
                                                                info!("Catch-up lockout: skipping increase on {} for dest {} (leader_net {}, dest_net {}, diff {})", contract, dest_id_clone, leader_net, dest_net, diff);
                                                                return;
                                                            }
                                                        }
                                                        // Apply stability gate to avoid accidental re-entry while leader cache hasn't refreshed
                                                        let pre_abs = dest_net.abs();
                                                        let post_abs = (dest_net + diff).abs();
                                                        if post_abs > pre_abs {
                                                            let stable_ok = {
                                                                let st = src_stable_clone.read().await;
                                                                if let Some((last_net, cnt)) = st.get(&contract) {
                                                                    *last_net == leader_net && *cnt >= 2
                                                                } else { false }
                                                            };
                                                            if !stable_ok {
                                                                info!("Catch-up gate: skipping increase on {} for dest {} (leader_net {}, dest_net {}, diff {}) until leader stabilizes", contract, dest_id_clone, leader_net, dest_net, diff);
                                                                return; // do not increase exposure here; main loop will resync when stable
                                                            }
                                                        }
                                                        let side = if diff > 0 { 0 } else { 1 };
                                                        let size = diff.abs();
                                                        let tag = format!("TCX:CATCHUP:{}:{}:{}", contract, dest_id_clone, diff);
                                                        let req = PlaceOrderReq {
                                                            account_id: dest_id_clone,
                                                            contract_id: &contract,
                                                            r#type: 2,
                                                            side,
                                                            size,
                                                            limit_price: None,
                                                            stop_price: None,
                                                            trail_price: None,
                                                            custom_tag: Some(tag),
                                                            linked_order_id: None,
                                                            stop_loss_bracket: None,
                                                            take_profit_bracket: None,
                                                        };
                                                        match client_clone.place_order(&req).await {
                                                            Ok(_r) => {
                                                                // update cache to leader net
                                                                let mut dpw = dest_pos_clone.write().await;
                                                                if leader_net == 0 {
                                                                    dpw.remove(&(dest_id_clone, contract.clone()));
                                                                } else {
                                                                    dpw.insert((dest_id_clone, contract.clone()), leader_net);
                                                                }
                                                                info!("Catch-up: synced {} on dest {} to leader {} via market", contract, dest_id_clone, leader_net);
                                                            }
                                                            Err(e) => warn!("Catch-up: failed to place market on dest {} for {}: {}", dest_id_clone, contract, e),
                                                        }
                                                    }
                                                });
                                            }

                                            // Always try to cancel mapped follower order when leader removes it
                                            if let Err(e) = client.cancel_order(dest, d_oid).await {
                                                warn!("Order copy: failed to cancel dest {} for leader order {}: {}", dest, oid, e);
                                            } else {
                                                info!("Order copy: cancelled dest {} order {} (leader order {} gone)", dest, d_oid, oid);
                                                map_dest_order.remove(&(dest, *oid));
                                                {
                                                    let mut om = open_mirrored.write().await;
                                                    if let Some(cnt) = om.get_mut(&(dest, snap_prev.contract_id.clone())) {
                                                        if *cnt > 0 { *cnt -= 1; }
                                                        if *cnt == 0 { om.remove(&(dest, snap_prev.contract_id.clone())); }
                                                    }
                                                }
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
                                                // No mapping (e.g., restart) — place new; use linkage if parent is mapped, else place unlinked
                                                let tag = Some(format!("TCX:ORD:{}", oid));
                                                let mut linked: Option<i64> = None;
                                                if let Some(parent_leader_id) = snap_now.linked_order_id {
                                                    if let Some(&parent_dest_id) = map_dest_order.get(&(dest, parent_leader_id)) {
                                                        linked = Some(parent_dest_id);
                                                    } else {
                                                        // parent not mapped; we'll place unlinked
                                                        linked = None;
                                                    }
                                                }
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
                                                    linked_order_id: linked,
                                                    stop_loss_bracket: None,
                                                    take_profit_bracket: None,
                                                };
                                                if let Ok(r) = client.place_order(&req).await {
                                                    map_dest_order.insert((dest, *oid), r.order_id);
                                                    {
                                                        let mut om = open_mirrored.write().await;
                                                        *om.entry((dest, snap_now.contract_id.clone())).or_insert(0) += 1;
                                                    }
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

                    // Update leader stability counts (consecutive identical snapshots)
                    {
                        let mut st = self.src_stable.write().await;
                        for (cid, &net) in current.iter() {
                            match st.get_mut(cid) {
                                Some((last_net, cnt)) => {
                                    if *last_net == net {
                                        *cnt = cnt.saturating_add(1);
                                    } else {
                                        *last_net = net;
                                        *cnt = 1;
                                    }
                                }
                                None => {
                                    st.insert(cid.clone(), (net, 1));
                                }
                            }
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