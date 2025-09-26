# TradeCopierX: Rust-based trade copier for ProjectX.
TradeCopierX syncs positions from a leader account to one or more follower accounts.
- Leader (source) = the account whose positions are mirrored.
- Followers (destinations) = accounts that automatically copy trades from the leader.
- Each follower firm can only specify 1 leader account, use the projectX internal copy trader to mirror to multiple accounts.

## Features
- REST-only design — avoids duplicate logins and CME rule violations by not using realtime hubs.
- Startup reconcile — aligns follower accounts to the leader’s open positions before running.
- Polling — leader positions are polled once every 1.5s (default), well within rate limits.
- Safe follower state — follower positions are tracked locally from the orders we place.
- Optional drift check — can poll follower positions to ensure they haven’t been manually altered.
- Optional order mirroring — can poll leader open orders and mirror create/modify/cancel to followers (OFF by default). When enabled, entry orders are intentionally NOT mirrored to avoid double-sizing with market-based position sync; only protective/exit orders (e.g., stops/targets) are mirrored.
- Bracket/OCO awareness — when the leader exposes linked_order_id, follower orders are placed with linkage to preserve OCO behavior. If linkage isn’t available from the API, we still mirror both legs individually and cancel the sibling leg as soon as the leader removes it (small risk window equal to poll interval). The API also supports placing brackets via takeProfitBracket/stopLossBracket on Order/place; our client now supports these fields, but since we do not mirror entry orders, we currently mirror protective legs as standalone orders (with OCO linkage when possible) rather than attaching bracket objects to entries.
- Fill catch-up — tracks recent leader fills via /api/Order/search; if a mirrored protective order fills on the leader but not on a follower, after ORDER_CATCHUP_MS we cancel the follower’s pending order and market-sync the position to the leader to prevent desyncs.
- Rate-limit aware — avoids hitting ProjectX’s API caps (200 requests/minute).
- Optional Order copying for Limit and Stop orders (Totally untested)

## Requirements
- Rust (≥ 1.76.0 recommended)
- Cargo
- Access to ProjectX API (source + follower accounts)

## Installation

Clone the repo:
```bash
git clone https://github.com/yourusername/TradeCopierX.git
cd TradeCopierX
```
Build
```bash
cargo build --release
```

## Configuration
All settings are provided via a .env file in the project root.
Example:
```dotenv
# ========= Leader (source) =========
SRC_API_BASE=https://api.topstepx.com
SRC_USERNAME=your-leader-username
SRC_API_KEY=your-leader-api-key
SOURCE_ACCOUNT=EXPRESS-V2-12345-67890123   # or numeric account ID

# ========= Followers (destinations) =========
# Provide aligned comma-separated lists. Index 0 in each list refers to the same destination firm.
# IMPORTANT: Exactly ONE account per destination API. If you want to fan out to many accounts
# within a firm, configure that firm’s internal copy trader to follow the specified account.
DEST_API_BASES=https://api.alphaticks.projectx.com,https://api.tradeify.projectx.com
DEST_USERNAMES=your-follower-username-1,your-follower-username-2
DEST_API_KEYS=your-follower-api-key-1,your-follower-api-key-2
# One account per destination API, aligned by index:
DEST_ACCOUNTS=202509092164,LEADER-ACCOUNT-NAME-OR-ID

# ========= Polling cadence (leader only) =========
# 1500 ms = ~40 req/min (leaving ~160/min for orders)
SOURCE_POLL_MS=1500

# ========= Optional follower drift polling (OFF by default) =========
# Set to 1 to poll follower positions every ~3s
FOLLOWER_DRIFT_CHECK=0

# ========= Optional: order mirroring (OFF by default) =========
# Set to 1/true to poll leader open orders and mirror create/modify/cancel
ORDER_COPY=0
# Optionally override order polling cadence (defaults to SOURCE_POLL_MS)
ORDER_POLL_MS=1500
ORDER_CATCHUP_MS=2000
# ========= Optional tuning =========
PX_MAX_RESYNC=2147483647
PX_REST_CONCURRENCY=1
PX_HTTP_BACKOFF_BASE_MS=250
PX_HTTP_MAX_RETRIES=5
RUST_LOG=info
```
## Usage
Start the copier:
```bash
cargo run --release
```

On startup:
1.	The copier logs into leader and follower APIs.
2.	Performs a one-time reconcile:
   - Aligns all follower positions to leader.
   - Closes contracts on followers where the leader is flat.
3.	Begins polling the leader’s open positions every SOURCE_POLL_MS ms.
4.	Applies changes to followers using market orders.

⸻

## Rate Limits

### Per ProjectX API docs:
- 200 requests / 60s for most endpoints.
- 50 requests / 30s for history endpoints (not used by TradeCopierX).

### TradeCopierX design:
- Leader poll: ~40 requests/min
- Leaves ~160 requests/min for follower orders
- Backoff + retry ensures resilience to 429 responses

⸻

## Notes
- Only user hub endpoints are used (REST). Market hub is not used to avoid duplicate data logins.
- Follower state is maintained locally. Drift polling is optional.
- Errors (e.g. order rejections, API throttling) are logged via tracing.

## Roadmap
- Multiple leader accounts
- Configurable resync strategies
- Order type flexibility (limit, stop)