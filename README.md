# Soroban AMM

A constant-product Automated Market Maker (AMM) built as a Soroban smart contract on the Stellar blockchain. It implements the classic `x * y = k` bonding curve model — the same design used by Uniswap v2 — providing decentralized token swaps and liquidity provisioning.

---

## Table of Contents

- [Overview](#overview)
- [Architecture](#architecture)
- [Contracts](#contracts)
  - [AMM Pool Contract](#amm-pool-contract)
  - [LP Token Contract](#lp-token-contract)
- [Math & Formulas](#math--formulas)
- [Getting Started](#getting-started)
  - [Prerequisites](#prerequisites)
  - [Build](#build)
  - [Test](#test)
- [Usage](#usage)
  - [Deploy](#deploy)
  - [Add Liquidity](#add-liquidity)
  - [Swap Tokens](#swap-tokens)
  - [Remove Liquidity](#remove-liquidity)
  - [Query the Pool](#query-the-pool)
- [Contributing](#contributing)
- [License](#license)

---

## Overview

This AMM lets users:

- **Provide liquidity** — deposit two tokens into a pool and receive LP (Liquidity Provider) tokens representing their share.
- **Swap tokens** — exchange one pool token for the other at a price determined by the constant-product formula.
- **Redeem liquidity** — burn LP tokens to withdraw a proportional share of the pool's reserves.

All operations include slippage protection parameters. Fees are configurable in basis points at deployment and are distributed to liquidity providers by growing the pool reserves.

---

## Architecture

The project is a Cargo workspace with two contracts:

```
soroban-amm/
├── Cargo.toml                  # Workspace root
└── contracts/
    ├── amm/                    # Core AMM pool contract
    │   └── src/lib.rs
    └── token/                  # SEP-41 LP token contract
        └── src/lib.rs
```

The AMM contract depends on the token contract. When liquidity is added or removed, the AMM calls the LP token contract to mint or burn shares on behalf of the provider.

---

## Contracts

### AMM Pool Contract

Located in [contracts/amm/src/lib.rs](contracts/amm/src/lib.rs).

#### Storage

| Key | Type | Description |
|---|---|---|
| `TokenA` | `Address` | First pool asset |
| `TokenB` | `Address` | Second pool asset |
| `LpToken` | `Address` | LP token contract |
| `ReserveA` | `i128` | Pool's current balance of TokenA |
| `ReserveB` | `i128` | Pool's current balance of TokenB |
| `TotalShares` | `i128` | Total LP shares outstanding |
| `Shares(Address)` | `i128` | LP shares held by a specific provider |
| `FeeBps` | `i128` | Swap fee in basis points (e.g. `30` = 0.30%) |

#### Public Interface

| Function | Description |
|---|---|
| `initialize(token_a, token_b, lp_token, fee_bps)` | One-time pool setup |
| `add_liquidity(provider, amount_a, amount_b, min_shares) → shares` | Deposit tokens, receive LP shares |
| `remove_liquidity(provider, shares, min_a, min_b) → (a, b)` | Burn LP shares, withdraw tokens |
| `swap(trader, token_in, amount_in, min_out) → amount_out` | Exchange tokens |
| `get_amount_out(token_in, amount_in) → amount_out` | Quote a swap without executing it |
| `get_info() → PoolInfo` | Read pool state (reserves, fee, shares) |
| `shares_of(provider) → shares` | Read an LP's share balance |

### LP Token Contract

Located in [contracts/token/src/lib.rs](contracts/token/src/lib.rs).

A minimal SEP-41 compliant fungible token used exclusively as the LP share token. The AMM contract is set as admin at deployment and is the only caller permitted to `mint` and `burn`.

#### Public Interface

| Function | Description |
|---|---|
| `initialize(admin, name, symbol, decimals)` | One-time token setup |
| `mint(to, amount)` | Mint tokens — admin only |
| `burn(from, amount)` | Burn tokens — admin only |
| `transfer(from, to, amount)` | Transfer between accounts |
| `transfer_from(spender, from, to, amount)` | Spend an approved allowance |
| `approve(from, spender, amount)` | Approve a spender |
| `balance(id) → i128` | Read account balance |
| `allowance(from, spender) → i128` | Read spending allowance |
| `total_supply() → i128` | Read total tokens minted |

---

## Math & Formulas

### Constant-Product Invariant

Every swap must satisfy:

```
reserve_a * reserve_b = k   (constant)
```

### Swap Output

Fees are deducted from the input before applying the formula:

```
amount_in_with_fee = amount_in * (10_000 - fee_bps)

amount_out = (amount_in_with_fee * reserve_out)
           / (reserve_in * 10_000 + amount_in_with_fee)
```

### Initial LP Shares (First Deposit)

Uses the geometric mean of the deposited amounts:

```
shares = sqrt(amount_a * amount_b)
```

### Subsequent LP Shares

Uses the lesser of the two proportional contributions to prevent imbalanced deposits:

```
shares = min(
    amount_a * total_shares / reserve_a,
    amount_b * total_shares / reserve_b
)
```

### Liquidity Removal

Proportional to pool ownership at the time of withdrawal:

```
out_a = shares * reserve_a / total_shares
out_b = shares * reserve_b / total_shares
```

---

## Getting Started

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (stable toolchain)
- `wasm32-unknown-unknown` compilation target:
  ```sh
  rustup target add wasm32-unknown-unknown
  ```
- [Stellar CLI](https://developers.stellar.org/docs/tools/stellar-cli) (`stellar`) for deployment:
  ```sh
  cargo install --locked stellar-cli --features opt
  ```

### Setup

1. **Clone the repository:**

   ```sh
   git clone https://github.com/your-org/soroban-amm.git
   cd soroban-amm
   ```

2. **Verify the toolchain and target are installed:**

   ```sh
   rustup show                          # confirm stable toolchain is active
   rustup target list --installed       # should include wasm32-unknown-unknown
   ```

   If the WASM target is missing:

   ```sh
   rustup target add wasm32-unknown-unknown
   ```

3. **Configure the Stellar CLI for your target network** (testnet shown):

   ```sh
   stellar network add testnet \
     --rpc-url https://soroban-testnet.stellar.org \
     --network-passphrase "Test SDF Network ; September 2015"
   ```

4. **Create or import an account identity:**

   ```sh
   # Generate a new keypair and fund it via Friendbot
   stellar keys generate --default-seed mykey
   stellar keys fund mykey --network testnet
   ```

   Or import an existing secret key:

   ```sh
   stellar keys add mykey --secret-key
   # paste your secret key when prompted
   ```

5. **Confirm everything is wired up:**

   ```sh
   stellar keys address mykey           # should print your public key
   ```

You are now ready to build, test, and deploy.

### Build

Build all contracts as optimised WASM binaries:

```sh
cargo wasm
```

`wasm` is a Cargo alias defined in [.cargo/config.toml](.cargo/config.toml) that expands to:

```sh
cargo build --release --target wasm32-unknown-unknown
```

Output files:

```
target/wasm32-unknown-unknown/release/amm.wasm
target/wasm32-unknown-unknown/release/token.wasm
```

### Test

Run the full test suite:

```sh
cargo test
```

Tests are located in [contracts/amm/src/lib.rs](contracts/amm/src/lib.rs) and cover adding liquidity, swapping, and removing liquidity.

---

## Usage

### Deploy

Deploy the LP token contract first, then the AMM pool. The AMM contract address becomes the LP token's admin.

```sh
# Deploy the LP token
stellar contract deploy \
  --wasm target/wasm32-unknown-unknown/release/token.wasm \
  --network testnet \
  --source <YOUR_KEY>

# Deploy the AMM pool
stellar contract deploy \
  --wasm target/wasm32-unknown-unknown/release/amm.wasm \
  --network testnet \
  --source <YOUR_KEY>
```

Initialize the LP token (admin = AMM contract address):

```sh
stellar contract invoke \
  --id <LP_TOKEN_CONTRACT_ID> \
  --network testnet \
  --source <YOUR_KEY> \
  -- initialize \
  --admin <AMM_CONTRACT_ID> \
  --name "Pool LP Token" \
  --symbol "AMMLP" \
  --decimals 7
```

Initialize the AMM pool (fee of 30 bps = 0.30%):

```sh
stellar contract invoke \
  --id <AMM_CONTRACT_ID> \
  --network testnet \
  --source <YOUR_KEY> \
  -- initialize \
  --token_a <TOKEN_A_CONTRACT_ID> \
  --token_b <TOKEN_B_CONTRACT_ID> \
  --lp_token <LP_TOKEN_CONTRACT_ID> \
  --fee_bps 30
```

### Add Liquidity

```sh
stellar contract invoke \
  --id <AMM_CONTRACT_ID> \
  --network testnet \
  --source <YOUR_KEY> \
  -- add_liquidity \
  --provider <PROVIDER_ADDRESS> \
  --amount_a 1000000 \
  --amount_b 2000000 \
  --min_shares 0
```

`min_shares` is the minimum LP tokens you are willing to accept. Set to `0` to skip slippage protection during initial seeding.

### Swap Tokens

```sh
stellar contract invoke \
  --id <AMM_CONTRACT_ID> \
  --network testnet \
  --source <YOUR_KEY> \
  -- swap \
  --trader <TRADER_ADDRESS> \
  --token_in <TOKEN_A_CONTRACT_ID> \
  --amount_in 100000 \
  --min_out 0
```

Use `get_amount_out` first to compute an appropriate `min_out`.

### Remove Liquidity

```sh
stellar contract invoke \
  --id <AMM_CONTRACT_ID> \
  --network testnet \
  --source <YOUR_KEY> \
  -- remove_liquidity \
  --provider <PROVIDER_ADDRESS> \
  --shares <LP_SHARE_AMOUNT> \
  --min_a 0 \
  --min_b 0
```

### Query the Pool

```sh
# Full pool info
stellar contract invoke --id <AMM_CONTRACT_ID> -- get_info

# Quote a swap
stellar contract invoke --id <AMM_CONTRACT_ID> \
  -- get_amount_out \
  --token_in <TOKEN_A_CONTRACT_ID> \
  --amount_in 100000

# LP share balance
stellar contract invoke --id <AMM_CONTRACT_ID> \
  -- shares_of --provider <PROVIDER_ADDRESS>
```

---

## Contributing

Contributions are welcome. Please follow the guidelines below to keep the codebase consistent and review cycles short.

### Reporting Issues

- Search existing issues before opening a new one.
- Include the Rust / `soroban-sdk` version, the steps to reproduce, and the expected vs. actual behavior.
- For security vulnerabilities, **do not open a public issue** — contact the maintainers directly.

### Development Workflow

1. **Fork** the repository and create a branch from `main`:

   ```sh
   git checkout -b feat/my-feature
   ```

   Branch naming conventions:
   | Prefix | Use for |
   |---|---|
   | `feat/` | New features |
   | `fix/` | Bug fixes |
   | `refactor/` | Code restructuring without behavior change |
   | `test/` | Adding or improving tests |
   | `docs/` | Documentation only |
   | `chore/` | Build scripts, tooling, dependencies |

2. **Make your changes**, then ensure the build and tests pass:

   ```sh
   cargo build --release --target wasm32-unknown-unknown
   cargo test
   ```

3. **Write tests** for any new behavior. All public functions should have at least one test. Tests live alongside the implementation in `src/lib.rs` under a `#[cfg(test)]` module.

4. **Keep commits focused.** One logical change per commit. Use the [Conventional Commits](https://www.conventionalcommits.org/) format:

   ```
   feat: add time-weighted average price accumulator
   fix: prevent zero-share mint on initial deposit
   test: cover swap with maximum fee setting
   ```

5. **Open a Pull Request** against `main`. In the PR description:
   - Explain _what_ changed and _why_.
   - Reference any related issues with `Closes #<issue>` or `Related to #<issue>`.
   - If the change affects contract behavior, include before/after output or test coverage evidence.

### Code Style

- An [`.editorconfig`](.editorconfig) at the workspace root defines shared formatting rules (UTF-8, LF line endings, 4-space indentation, trailing-whitespace trimming). Most editors apply it automatically; install the [EditorConfig plugin](https://editorconfig.org/#download) if yours does not.
- Run `cargo fmt` before committing — the project uses default `rustfmt` settings.
- Run `cargo clippy -- -D warnings` and resolve any warnings before opening a PR.
- Prefer explicit arithmetic with overflow checks over silent wrapping. The release profile already enables `overflow-checks = true`.
- Avoid unsafe code. There is no reason to use `unsafe` in a Soroban contract.
- Do not add dependencies without discussion. The contract binary size and attack surface matter.

### Pull Request Checklist

Before requesting review, confirm:

- [ ] `cargo fmt` has been run
- [ ] `cargo clippy -- -D warnings` passes
- [ ] `cargo test` passes
- [ ] New behavior is covered by tests
- [ ] Public interface changes are reflected in this README
- [ ] Commit messages follow the Conventional Commits format

### Versioning

This project follows [Semantic Versioning](https://semver.org/). Breaking changes to the on-chain interface (function signatures, storage layout, error codes) constitute a major version bump.

---

## License

This project is licensed under the [MIT License](LICENSE).
