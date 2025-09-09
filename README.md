# TradeCopierX

**TradeCopierX** is a tool for copying trades in real time from one trading account ("source") to one or more destination accounts. It is designed for algorithmic trading, portfolio management, and scenarios where you want to mirror trades between accounts on the same or different brokers/exchanges.

## Warning
- Currently manually changing a trade on a follower will cause a synchronization error and will not be easily reconciled unless the leader is flattened
- Alpha version:  not completely tested (use ProjectX risk control as a safeguard max size, max drawdown)
- Only supports 1 follower prop firm, support for multiple firms to come.
- Support for contract multiplier on followers to come.
---

## Project Overview

TradeCopierX securely connects to your trading accounts using API keys and monitors the source account for new trades or orders. When a trade is detected, it automatically replicates the trade to one or more destination accounts, ensuring your portfolios stay synchronized.

---

## Secrets & Configuration

TradeCopierX relies on several environment variables for secure configuration. **Do not hardcode secrets in your source code.**

### Required Environment Variables

- `PX_SOURCE_USERNAME` – Username for the source account.
- `PX_SOURCE_API_KEY` – API key/token for the source account (the account to copy trades *from*).
- `PX_SOURCE_ACCOUNT_ID` – Account ID or username for the source account.
- `PX_DEST_USERNAME` – Username for the destination account.
- `PX_DEST_API_KEY` – API key/token for the destination account (the account to copy trades *to*).
- `PX_DEST_ACCOUNT_ID` – Account ID or username for the destination account.

### Optional Environment Variables

- `PX_POLL_MS` – How often (in milliseconds) to poll the source account for new trades. Default: `50` (50 milliseconds).
- `PX_DEST_ACCOUNTS` – (Optional) Comma-separated list of destination account IDs to copy trades to multiple accounts.

---

## How to Store Secrets

There are multiple secure ways to provide secrets to TradeCopierX:

1. **`.env` file** (recommended for local development). Create a file named `.env` in your project root:
   ```
   PX_SOURCE_USERNAME=...
   PX_SOURCE_API_KEY=...
   PX_SOURCE_ACCOUNT_ID=...
   PX_DEST_USERNAME=...
   PX_DEST_API_KEY=...
   PX_DEST_ACCOUNT_ID=...
   ```
   > **Note:** Make sure `.env` is listed in your `.gitignore` to avoid committing secrets.

2. **System environment variables**. Set variables in your shell before running the program:
   ```
   export PX_SOURCE_USERNAME=...
   export PX_SOURCE_API_KEY=...
   # etc.
   ```

3. **Secret managers**:
    - Use [direnv](https://direnv.net/) for per-directory environment loading.
    - Use Docker secrets if deploying in containers.
    - Use your OS keychain or a cloud secret manager for production.

---

## Running the Project

1. **Install Rust**
   https://www.rust-lang.org/tools/install

2. **Build the project:**
   ```
   cargo build --release
   ```

3. **Run the executable** with environment variables set (using a `.env` file, or exported in your shell):
   ```
   ./target/release/tradecopierx
   ```
   or (if using dotenv support):
   ```
   dotenv ./target/release/tradecopierx
   ```

---

## Options

- **Polling interval:** Adjust `PX_POLL_MS` to control how frequently the source account is checked for new trades. Lower values give faster copying but may hit API rate limits.
- **Multiple destinations:** Set `PX_DEST_ACCOUNTS` to a comma-separated list (e.g., `PX_DEST_ACCOUNTS=acc1,acc2,acc3`) and provide corresponding API keys/secrets for each destination account as needed.
- **Custom environments:** You can use different `.env` files for staging/production.

---

## Security Considerations

- **Never commit API keys or secrets** to version control.
- Rotate API keys regularly and immediately if you suspect compromise.
- Use secret managers or environment variables in production—**never** hardcode secrets in code or configuration files checked into git.
- Limit the permissions of API keys to only what is necessary (e.g., trade only, no withdrawal).

---

## Example Configuration (`.env`)

```env
# Example .env file for TradeCopierX (FAKE values – do not use in production!)
PX_SRC_API_BASE=https://api.topstepx.com
PX_SOURCE_USERNAME=
PX_SOURCE_API_KEY=
PX_SOURCE_ACCOUNT_ID=

PX_DEST_API_BASE=https://api.alphaticks.projectx.com
PX_DEST_USERNAME=
PX_DEST_API_KEY=
PX_DEST_ACCOUNT_ID=
PX_DEST_ACCOUNTS=
```

---

**Questions?** Open an issue or see the project documentation for more details.