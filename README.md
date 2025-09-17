# TradeCopierX

## TradeCopierX is a Rust-based trade copier for ProjectX (Topstep / AlphaTicks).
It syncs positions from a leader account to one or more follower accounts.
•	Leader (source) = the account whose positions are mirrored.
•	Followers (destinations) = accounts that automatically copy trades from the leader.

## Features
•	REST-only design — avoids duplicate logins and CME rule violations by not using realtime hubs.
•	Startup reconcile — aligns follower accounts to the leader’s open positions before running.
•	Polling — leader positions are polled once every 1.5s (default), well within rate limits.
•	Safe follower state — follower positions are tracked locally from the orders we place.
•	Optional drift check — can poll follower positions to ensure they haven’t been manually altered.
•	Rate-limit aware — avoids hitting ProjectX’s API caps (200 requests/minute).

⸻

## Requirements
•	Rust (≥ 1.76.0 recommended)
•	Cargo
•	Access to ProjectX API (source + follower accounts)

⸻

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

Configuration

All settings are provided via a .env file in the project root.
Example:
```dotenv
# ========= Leader (source) =========
SRC_API_BASE=https://api.topstepx.com
SRC_USERNAME=your-leader-username
SRC_API_KEY=your-leader-api-key
SOURCE_ACCOUNT=EXPRESS-V2-12345-67890123   # or numeric account ID

# ========= Follower (destination) =========
DEST_API_BASE=https://api.alphaticks.projectx.com
DEST_USERNAME=your-follower-username
DEST_API_KEY=your-follower-api-key
DEST_ACCOUNTS=202509092164,202509092165    # comma-separated if multiple

# ========= Polling cadence (leader only) =========
# 1500 ms = ~40 req/min (leaving ~160/min for orders)
SOURCE_POLL_MS=1500

# ========= Optional follower drift polling (OFF by default) =========
# Set to 1 to poll follower positions every ~3s
FOLLOWER_DRIFT_CHECK=0

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
•	Aligns all follower positions to leader.
•	Closes contracts on followers where the leader is flat.
3.	Begins polling the leader’s open positions every SOURCE_POLL_MS ms.
4.	Applies changes to followers using market orders.

⸻

## Rate Limits

### Per ProjectX API docs:
•	200 requests / 60s for most endpoints.
•	50 requests / 30s for history endpoints (not used by TradeCopierX).

### TradeCopierX design:
•	Leader poll: ~40 requests/min
•	Leaves ~160 requests/min for follower orders
•	Backoff + retry ensures resilience to 429 responses

⸻

## Notes
•	Only user hub endpoints are used (REST). Market hub is not used to avoid duplicate data logins.
•	Follower state is maintained locally. Drift polling is optional.
•	Errors (e.g. order rejections, API throttling) are logged via tracing.

## Roadmap
•	Multiple leader accounts
•	Configurable resync strategies
•	Order type flexibility (limit, stop)