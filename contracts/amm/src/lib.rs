//! Constant-product AMM (x * y = k) on Soroban.
//!
//! Flow:
//!   1. Deploy this contract + two asset token contracts.
//!   2. Call `initialize` with both token addresses.
//!   3. First LP calls `add_liquidity` to seed the pool.
//!   4. Traders call `swap` to exchange tokens.
//!   5. LPs call `remove_liquidity` to redeem their share.

#![no_std]

use soroban_sdk::{contract, contractimpl, contracttype, symbol_short, Address, Env, Symbol};
// Standard SEP-41 interface for pool tokens (token_a, token_b)
use soroban_sdk::token::Client as SepTokenClient;

/// Interface for the LP token contract.
///
/// We define this locally rather than importing the `token` crate to avoid
/// duplicate symbol errors during the WASM build.
#[soroban_sdk::contractclient(name = "LpTokenClient")]
pub trait LpTokenInterface {
    fn initialize(
        env: Env,
        admin: Address,
        name: soroban_sdk::String,
        symbol: soroban_sdk::String,
        decimals: u32,
    );
    fn mint(env: Env, to: Address, amount: i128);
    fn burn(env: Env, from: Address, amount: i128);
    fn balance(env: Env, id: Address) -> i128;
}

// ── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    TokenA,
    TokenB,
    LpToken,
    ReserveA,
    ReserveB,
    TotalShares,
    FeeBps, // fee in basis points, e.g. 30 = 0.30 %
}

// ── Pool info returned by `get_info` ─────────────────────────────────────────

#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct PoolInfo {
    pub token_a: Address,
    pub token_b: Address,
    pub reserve_a: i128,
    pub reserve_b: i128,
    pub total_shares: i128,
    pub fee_bps: i128,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct AmmPool;

#[contractimpl]
impl AmmPool {
    // ── Admin / Setup ─────────────────────────────────────────────────────────

    /// Initialize the AMM pool with two tokens, an LP token, and a swap fee.
    ///
    /// Must be called exactly once after deployment. The LP token contract must
    /// already be deployed with this contract set as its admin so it can mint
    /// and burn shares on behalf of liquidity providers.
    ///
    /// # Parameters
    /// - `token_a` – Address of the first pool token (SEP-41 compliant).
    /// - `token_b` – Address of the second pool token (SEP-41 compliant).
    /// - `lp_token` – Address of the LP token contract used to represent pool shares.
    /// - `fee_bps` – Swap fee in basis points (e.g. `30` = 0.30 %). Must be in `[0, 10_000]`.
    ///
    /// # Panics
    /// - If the pool has already been initialized.
    /// - If `token_a == token_b`.
    /// - If `fee_bps` is outside the range `[0, 10_000]`.
    pub fn initialize(
        env: Env,
        token_a: Address,
        token_b: Address,
        lp_token: Address,
        fee_bps: i128, // recommended: 30 (0.30 %)
    ) {
        if env.storage().instance().has(&DataKey::TokenA) {
            panic!(
                "already initialized: contract {:?}",
                env.current_contract_address()
            );
        }
        assert!(
            token_a != token_b,
            "tokens must differ: token_a={token_a:?}, token_b={token_b:?}"
        );
        assert!(
            (0..=10_000).contains(&fee_bps),
            "invalid fee: {fee_bps} is outside 0..=10_000"
        );

        env.storage().instance().set(&DataKey::TokenA, &token_a);
        env.storage().instance().set(&DataKey::TokenB, &token_b);
        env.storage().instance().set(&DataKey::LpToken, &lp_token);
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        env.storage().instance().set(&DataKey::ReserveA, &0_i128);
        env.storage().instance().set(&DataKey::ReserveB, &0_i128);
        env.storage().instance().set(&DataKey::TotalShares, &0_i128);
    }

    // ── Liquidity ─────────────────────────────────────────────────────────────

    /// Deposit tokens into the pool and receive LP shares in return.
    ///
    /// On the first deposit any ratio is accepted and the initial share supply is
    /// set to the geometric mean of the two amounts. Subsequent deposits must
    /// match the current pool ratio (within integer rounding); excess tokens are
    /// **not** refunded automatically — callers should compute amounts off-chain
    /// before calling.
    ///
    /// Requires `provider` to have authorized this call.
    ///
    /// # Parameters
    /// - `provider` – Address of the liquidity provider funding the deposit.
    /// - `amount_a` – Amount of `token_a` to deposit. Must be positive.
    /// - `amount_b` – Amount of `token_b` to deposit. Must be positive.
    /// - `min_shares` – Minimum number of LP shares the caller is willing to
    ///   receive; the transaction panics if fewer would be minted (slippage guard).
    ///
    /// # Returns
    /// The number of LP shares minted to `provider`.
    ///
    /// # Panics
    /// - If either `amount_a` or `amount_b` is not positive.
    /// - If the shares that would be minted are less than `min_shares`.
    pub fn add_liquidity(
        env: Env,
        provider: Address,
        amount_a: i128,
        amount_b: i128,
        min_shares: i128,
    ) -> i128 {
        provider.require_auth();
        assert!(
            amount_a > 0 && amount_b > 0,
            "amounts must be positive: amount_a={amount_a}, amount_b={amount_b}"
        );

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();

        let reserve_a: i128 = Self::get_reserve_a(env.clone());
        let reserve_b: i128 = Self::get_reserve_b(env.clone());
        let total_shares: i128 = Self::get_total_shares(env.clone());

        // Pull tokens from provider into the pool contract.
        let client_a = SepTokenClient::new(&env, &token_a);
        let client_b = SepTokenClient::new(&env, &token_b);
        client_a.transfer(&provider, &env.current_contract_address(), &amount_a);
        client_b.transfer(&provider, &env.current_contract_address(), &amount_b);

        // Compute shares to mint.
        let shares = if total_shares == 0 {
            // Initial liquidity: geometric mean of deposits (scaled by 1e7).
            Self::sqrt(amount_a * amount_b)
        } else {
            // Proportional shares — use the lesser of the two ratios.
            let shares_a = amount_a * total_shares / reserve_a;
            let shares_b = amount_b * total_shares / reserve_b;
            shares_a.min(shares_b)
        };

        assert!(
            shares > 0,
            "amounts too small: computed shares would be zero"
        );
        assert!(
            shares >= min_shares,
            "slippage: insufficient shares minted: computed={shares}, minimum={min_shares}"
        );

        // Update reserves.
        env.storage()
            .instance()
            .set(&DataKey::ReserveA, &(reserve_a + amount_a));
        env.storage()
            .instance()
            .set(&DataKey::ReserveB, &(reserve_b + amount_b));
        env.storage()
            .instance()
            .set(&DataKey::TotalShares, &(total_shares + shares));

        // Mint LP tokens.
        let lp_client = LpTokenClient::new(&env, &lp_token);
        lp_client.mint(&provider, &shares);

        env.events().publish(
            (Symbol::new(&env, "add_liquidity"), provider),
            (amount_a, amount_b, shares),
        );

        shares
    }

    /// Withdraw liquidity from the pool by burning LP shares.
    ///
    /// Burns exactly `shares` LP tokens held by `provider` and transfers a
    /// proportional amount of both pool tokens back to the provider. The
    /// proportion is `shares / total_shares` at the time of the call.
    ///
    /// Requires `provider` to have authorized this call.
    ///
    /// # Parameters
    /// - `provider` – Address of the liquidity provider redeeming shares.
    /// - `shares` – Number of LP shares to burn. Must be positive and ≤ the
    ///   provider's current balance.
    /// - `min_a` – Minimum amount of `token_a` the caller is willing to receive
    ///   (slippage guard).
    /// - `min_b` – Minimum amount of `token_b` the caller is willing to receive
    ///   (slippage guard).
    ///
    /// # Returns
    /// A tuple `(amount_a, amount_b)` — the token amounts transferred back to
    /// the provider.
    ///
    /// # Panics
    /// - If `shares` is not positive.
    /// - If `provider` owns fewer shares than `shares`.
    /// - If the computed `token_a` output would be less than `min_a`.
    /// - If the computed `token_b` output would be less than `min_b`.
    pub fn remove_liquidity(
        env: Env,
        provider: Address,
        shares: i128,
        min_a: i128,
        min_b: i128,
    ) -> (i128, i128) {
        provider.require_auth();
        assert!(shares > 0, "shares must be positive: got {shares}");

        let owned = Self::shares_of(env.clone(), provider.clone());
        assert!(
            owned >= shares,
            "insufficient LP shares: owned={owned}, requested={shares}"
        );

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();

        let reserve_a = Self::get_reserve_a(env.clone());
        let reserve_b = Self::get_reserve_b(env.clone());
        let total_shares = Self::get_total_shares(env.clone());

        let out_a = shares * reserve_a / total_shares;
        let out_b = shares * reserve_b / total_shares;

        assert!(
            out_a >= min_a,
            "slippage: insufficient token_a out: got={out_a}, min={min_a}"
        );
        assert!(
            out_b >= min_b,
            "slippage: insufficient token_b out: got={out_b}, min={min_b}"
        );

        // Burn LP tokens.
        let lp_client = LpTokenClient::new(&env, &lp_token);
        lp_client.burn(&provider, &shares);

        // Update state.
        env.storage()
            .instance()
            .set(&DataKey::ReserveA, &(reserve_a - out_a));
        env.storage()
            .instance()
            .set(&DataKey::ReserveB, &(reserve_b - out_b));
        env.storage()
            .instance()
            .set(&DataKey::TotalShares, &(total_shares - shares));

        // Return tokens.
        let client_a = SepTokenClient::new(&env, &token_a);
        let client_b = SepTokenClient::new(&env, &token_b);
        client_a.transfer(&env.current_contract_address(), &provider, &out_a);
        client_b.transfer(&env.current_contract_address(), &provider, &out_b);

        env.events().publish(
            (symbol_short!("rm_liq"),),
            (provider.clone(), shares, out_a, out_b),
        );

        (out_a, out_b)
    }

    // ── Swap ──────────────────────────────────────────────────────────────────

    /// Swap an exact amount of one pool token for the other.
    ///
    /// Transfers `amount_in` of `token_in` from `trader` into the pool and
    /// sends back the calculated output amount of the opposite token, computed
    /// via the constant-product formula `x * y = k` with the pool fee deducted
    /// from `amount_in` before the calculation.
    ///
    /// Requires `trader` to have authorized this call.
    ///
    /// # Parameters
    /// - `trader` – Address of the account initiating the swap.
    /// - `token_in` – Address of the token being sold; must be either `token_a`
    ///   or `token_b` of this pool.
    /// - `amount_in` – Exact amount of `token_in` to sell. Must be positive.
    /// - `min_out` – Minimum amount of the output token the caller is willing to
    ///   accept (slippage guard).
    ///
    /// # Returns
    /// The amount of the output token transferred to `trader`.
    ///
    /// # Panics
    /// - If `amount_in` is not positive.
    /// - If `token_in` is not one of the two pool tokens.
    /// - If either pool reserve is zero (pool is empty).
    /// - If the computed output would be less than `min_out`.
    /// - If the computed output equals or exceeds the output reserve (insufficient liquidity).
    pub fn swap(
        env: Env,
        trader: Address,
        token_in: Address,
        amount_in: i128,
        min_out: i128,
    ) -> i128 {
        trader.require_auth();
        assert!(amount_in > 0, "amount_in must be positive: got {amount_in}");

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();

        let (reserve_in, reserve_out, token_out) = if token_in == token_a {
            (
                Self::get_reserve_a(env.clone()),
                Self::get_reserve_b(env.clone()),
                token_b.clone(),
            )
        } else if token_in == token_b {
            (
                Self::get_reserve_b(env.clone()),
                Self::get_reserve_a(env.clone()),
                token_a.clone(),
            )
        } else {
            panic!("token_in is not part of this pool: {token_in:?}");
        };

        assert!(
            reserve_in > 0 && reserve_out > 0,
            "pool is empty: reserve_in={reserve_in}, reserve_out={reserve_out}"
        );

        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap();

        // amount_in after fee
        let amount_in_with_fee = amount_in * (10_000 - fee_bps);
        // constant-product: out = (amount_in_with_fee * reserve_out) / (reserve_in * 10_000 + amount_in_with_fee)
        let amount_out =
            amount_in_with_fee * reserve_out / (reserve_in * 10_000 + amount_in_with_fee);

        assert!(
            amount_out >= min_out,
            "slippage: insufficient output amount: got={amount_out}, min={min_out}"
        );
        assert!(
            amount_out < reserve_out,
            "insufficient liquidity: amount_out={amount_out} >= reserve_out={reserve_out}"
        );

        // Transfer in.
        let client_in = SepTokenClient::new(&env, &token_in);
        client_in.transfer(&trader, &env.current_contract_address(), &amount_in);

        // Transfer out.
        let client_out = SepTokenClient::new(&env, &token_out);
        client_out.transfer(&env.current_contract_address(), &trader, &amount_out);

        // Update reserves.
        if token_in == token_a {
            env.storage()
                .instance()
                .set(&DataKey::ReserveA, &(reserve_in + amount_in));
            env.storage()
                .instance()
                .set(&DataKey::ReserveB, &(reserve_out - amount_out));
        } else {
            env.storage()
                .instance()
                .set(&DataKey::ReserveB, &(reserve_in + amount_in));
            env.storage()
                .instance()
                .set(&DataKey::ReserveA, &(reserve_out - amount_out));
        }

        env.events().publish(
            (Symbol::new(&env, "swap"), trader),
            (token_in, amount_in, amount_out),
        );

        amount_out
    }

    // ── Quotes (read-only) ────────────────────────────────────────────────────

    /// Return the current spot price of each token in terms of the other,
    /// scaled by 1_000_000.
    ///
    /// Returns `(price_a, price_b)` where:
    /// - `price_a` = price of token_a in terms of token_b (reserve_b * 1_000_000 / reserve_a)
    /// - `price_b` = price of token_b in terms of token_a (reserve_a * 1_000_000 / reserve_b)
    ///
    /// Panics if either reserve is zero (pool is empty).
    pub fn price_ratio(env: Env) -> (i128, i128) {
        let reserve_a = Self::get_reserve_a(env.clone());
        let reserve_b = Self::get_reserve_b(env);
        assert!(reserve_a > 0 && reserve_b > 0, "pool is empty");
        let price_a = reserve_b * 1_000_000 / reserve_a;
        let price_b = reserve_a * 1_000_000 / reserve_b;
        (price_a, price_b)
    }

    /// Quote how much `token_out` you receive for `amount_in` of `token_in`.
    /// Calculate the output amount for a hypothetical swap without executing it.
    ///
    /// Applies the same constant-product formula and fee as `swap` but
    /// makes no state changes. Useful for quoting prices off-chain or in other
    /// contracts before committing to a swap.
    ///
    /// # Parameters
    /// - `token_in` – Address of the token being sold; must be either `token_a`
    ///   or `token_b` of this pool.
    /// - `amount_in` – Hypothetical amount of `token_in` to sell.
    ///
    /// # Returns
    /// The amount of the output token that would be received for `amount_in`,
    /// after the pool fee is applied.
    ///
    /// # Panics
    /// - If `token_in` is not one of the two pool tokens.
    pub fn get_amount_out(env: Env, token_in: Address, amount_in: i128) -> i128 {
        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap();

        let (reserve_in, reserve_out) = if token_in == token_a {
            (
                Self::get_reserve_a(env.clone()),
                Self::get_reserve_b(env.clone()),
            )
        } else if token_in == token_b {
            (
                Self::get_reserve_b(env.clone()),
                Self::get_reserve_a(env.clone()),
            )
        } else {
            panic!("unknown token_in: {token_in:?}");
        };

        assert!(
            reserve_in > 0 && reserve_out > 0,
            "pool is empty: reserve_in={reserve_in}, reserve_out={reserve_out}"
        );
        let amount_in_with_fee = amount_in * (10_000 - fee_bps);
        amount_in_with_fee * reserve_out / (reserve_in * 10_000 + amount_in_with_fee)
    }

    /// Quote how much `token_in` is required to receive exactly `amount_out` of `token_out`.
    pub fn get_amount_in(env: Env, token_out: Address, amount_out: i128) -> i128 {
        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap();

        let (reserve_in, reserve_out) = if token_out == token_a {
            (
                Self::get_reserve_b(env.clone()),
                Self::get_reserve_a(env.clone()),
            )
        } else if token_out == token_b {
            (
                Self::get_reserve_a(env.clone()),
                Self::get_reserve_b(env.clone()),
            )
        } else {
            panic!("unknown token");
        };

        assert!(reserve_in > 0 && reserve_out > 0, "zero reserve");
        assert!(amount_out < reserve_out, "amount_out >= reserve_out");

        (reserve_in * amount_out * 10_000) / ((reserve_out - amount_out) * (10_000 - fee_bps)) + 1
    }

    /// Return full pool state.
    /// Return a snapshot of the full pool state.
    ///
    /// This is a read-only view function; it makes no state changes.
    ///
    /// # Returns
    /// A [`PoolInfo`] struct containing:
    /// - `token_a` / `token_b` — addresses of the two pool tokens.
    /// - `reserve_a` / `reserve_b` — current token reserves held by the pool.
    /// - `total_shares` — total outstanding LP shares.
    /// - `fee_bps` — the swap fee in basis points.
    pub fn get_info(env: Env) -> PoolInfo {
        PoolInfo {
            token_a: env.storage().instance().get(&DataKey::TokenA).unwrap(),
            token_b: env.storage().instance().get(&DataKey::TokenB).unwrap(),
            reserve_a: Self::get_reserve_a(env.clone()),
            reserve_b: Self::get_reserve_b(env.clone()),
            total_shares: Self::get_total_shares(env.clone()),
            fee_bps: env.storage().instance().get(&DataKey::FeeBps).unwrap(),
        }
    }

    /// Return the number of LP shares currently held by a given provider.
    ///
    /// This is a read-only view function; it makes no state changes.
    ///
    /// # Parameters
    /// - `provider` – Address of the liquidity provider to query.
    ///
    /// # Returns
    /// The LP share balance of `provider`, or `0` if the address has never
    /// provided liquidity to this pool.
    pub fn shares_of(env: Env, provider: Address) -> i128 {
        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();
        LpTokenClient::new(&env, &lp_token).balance(&provider)
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    fn get_reserve_a(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::ReserveA)
            .unwrap_or(0)
    }

    fn get_reserve_b(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::ReserveB)
            .unwrap_or(0)
    }

    fn get_total_shares(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalShares)
            .unwrap_or(0)
    }

    /// Integer square root via Newton's method.
    fn sqrt(n: i128) -> i128 {
        if n < 0 {
            panic!("sqrt of negative: {n}");
        }
        if n == 0 {
            return 0;
        }
        let mut x = n;
        let mut y = (x + 1) / 2;
        while y < x {
            x = y;
            y = (x + n / x) / 2;
        }
        x
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::Address as _,
        token::{StellarAssetClient, TokenClient as StellarTokenClient},
        Env,
    };
    use token::LpToken;

    fn create_sac<'a>(
        env: &'a Env,
        admin: &Address,
    ) -> (StellarTokenClient<'a>, StellarAssetClient<'a>) {
        let contract = env.register_stellar_asset_contract_v2(admin.clone());
        (
            StellarTokenClient::new(env, &contract.address()),
            StellarAssetClient::new(env, &contract.address()),
        )
    }

    struct TestSetup {
        env: Env,
        amm_addr: Address,
        lp_addr: Address,
        ta_addr: Address,
        tb_addr: Address,
        #[allow(dead_code)]
        admin: Address,
    }

    /// Minimal setup: env + uninitialized AMM + LP token. Tokens are created by
    /// individual tests so each test can control the pool ratio independently.
    fn setup() -> (Env, Address, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );
        (env, admin.clone(), amm_addr, lp_addr, admin)
    }

    fn setup_pool(fee_bps: i128) -> TestSetup {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);

        AmmPoolClient::new(&env, &amm_addr).initialize(
            &ta.address,
            &tb.address,
            &lp_addr,
            &fee_bps,
        );

        let ta_addr = ta.address.clone();
        let tb_addr = tb.address.clone();
        drop((ta, ta_sac, tb, tb_sac));

        TestSetup {
            env,
            amm_addr,
            lp_addr,
            ta_addr,
            tb_addr,
            admin,
        }
    }

    // ── Initialization ────────────────────────────────────────────────────────

    #[test]
    fn test_add_and_swap() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);

        let shares = amm.add_liquidity(&provider, &1_000_000_i128, &2_000_000_i128, &0_i128);
        assert!(shares > 0);

        let info = amm.get_info();
        assert_eq!(info.reserve_a, 1_000_000);
        assert_eq!(info.reserve_b, 2_000_000);

        let trader = Address::generate(env);
        ta_sac.mint(&trader, &100_000_i128);
        let out = amm.swap(&trader, &ts.ta_addr, &100_000_i128, &0_i128);
        assert!(out > 0);
        assert!(out < 200_000);
    }

    #[test]
    fn test_price_ratio() {
        let (env, admin, amm_addr, lp_addr, _) = setup();

        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(&ta_client.address, &tb_client.address, &lp_addr, &30_i128);

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &2_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);

        amm.add_liquidity(&provider, &2_000_000_i128, &1_000_000_i128, &0_i128);

        // reserve_a = 2_000_000, reserve_b = 1_000_000
        // price_a = 1_000_000 * 1_000_000 / 2_000_000 = 500_000
        // price_b = 2_000_000 * 1_000_000 / 1_000_000 = 2_000_000
        let (price_a, price_b) = amm.price_ratio();
        assert_eq!(price_a, 500_000);
        assert_eq!(price_b, 2_000_000);
    }

    #[test]
    #[should_panic(expected = "pool is empty")]
    fn test_price_ratio_panics_on_empty_pool() {
        let (env, admin, amm_addr, lp_addr, _) = setup();

        let (ta_client, _) = create_sac(&env, &admin);
        let (tb_client, _) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(&ta_client.address, &tb_client.address, &lp_addr, &30_i128);

        // No liquidity added — reserves are zero, should panic
        amm.price_ratio();
    }

    #[test]
    fn test_remove_liquidity() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);

        let shares = amm.add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &0_i128);
        let (out_a, out_b) = amm.remove_liquidity(&provider, &shares, &0_i128, &0_i128);
        assert!(out_a > 0 && out_b > 0);
        assert_eq!(amm.get_info().total_shares, 0);
    }

    #[test]
    fn test_initialize_twice_panics() {
        let ts = setup_pool(30);
        let amm = AmmPoolClient::new(&ts.env, &ts.amm_addr);
        let result = amm.try_initialize(&ts.ta_addr, &ts.tb_addr, &ts.lp_addr, &30_i128);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_fee_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "LP"),
            &soroban_sdk::String::from_str(&env, "LP"),
            &7u32,
        );
        let (ta, _) = create_sac(&env, &admin);
        let (tb, _) = create_sac(&env, &admin);
        let result = AmmPoolClient::new(&env, &amm_addr).try_initialize(
            &ta.address,
            &tb.address,
            &lp_addr,
            &10_001_i128,
        );
        assert!(result.is_err());
    }

    // ── Swap ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_swap_b_to_a() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &0_i128);

        let trader = Address::generate(env);
        tb_sac.mint(&trader, &100_000_i128);
        let out = amm.swap(&trader, &ts.tb_addr, &100_000_i128, &0_i128);
        assert!(out > 0 && out < 100_000);

        let info = amm.get_info();
        assert_eq!(info.reserve_b, 1_100_000);
        assert_eq!(info.reserve_a, 1_000_000 - out);
    }

    #[test]
    fn test_swap_slippage_panics() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &0_i128);

        let trader = Address::generate(env);
        ta_sac.mint(&trader, &100_000_i128);
        let result = amm.try_swap(&trader, &ts.ta_addr, &100_000_i128, &200_000_i128);
        assert!(result.is_err());
    }

    #[test]
    fn test_fee_accrues_to_reserves() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &0_i128);

        let trader = Address::generate(env);
        let amount_in = 100_000_i128;
        ta_sac.mint(&trader, &amount_in);
        let out = amm.swap(&trader, &ts.ta_addr, &amount_in, &0_i128);

        let info = amm.get_info();
        assert_eq!(info.reserve_a, 1_000_000 + amount_in);
        assert_eq!(info.reserve_b, 1_000_000 - out);
        // k must grow because fee stays in pool
        assert!(info.reserve_a * info.reserve_b > 1_000_000 * 1_000_000);
    }

    // ── Liquidity ─────────────────────────────────────────────────────────────

    #[test]
    fn test_add_liquidity_slippage_panics() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        let result = amm.try_add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &i128::MAX);
        assert!(result.is_err());
    }

    #[test]
    fn test_remove_liquidity_slippage_panics() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        let shares = amm.add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &0_i128);
        let result = amm.try_remove_liquidity(&provider, &shares, &i128::MAX, &0_i128);
        assert!(result.is_err());
    }

    #[test]
    fn test_lp_token_transfer_enables_remove() {
        // Verify fix: LP token is the single source of truth for share ownership.
        // Before fix, AMM had a stale internal Shares map that didn't update on transfers.
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let lp = token::LpTokenClient::new(env, &ts.lp_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        let shares = amm.add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &0_i128);

        let recipient = Address::generate(env);
        lp.transfer(&provider, &recipient, &shares);

        assert_eq!(amm.shares_of(&provider), 0);
        assert_eq!(amm.shares_of(&recipient), shares);

        let (out_a, out_b) = amm.remove_liquidity(&recipient, &shares, &0_i128, &0_i128);
        assert!(out_a > 0 && out_b > 0);
        assert_eq!(amm.get_info().total_shares, 0);
    }

    #[test]
    fn test_multiple_lps() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let lp1 = Address::generate(env);
        ta_sac.mint(&lp1, &1_000_000_i128);
        tb_sac.mint(&lp1, &1_000_000_i128);
        let shares1 = amm.add_liquidity(&lp1, &1_000_000_i128, &1_000_000_i128, &0_i128);

        let lp2 = Address::generate(env);
        ta_sac.mint(&lp2, &500_000_i128);
        tb_sac.mint(&lp2, &500_000_i128);
        let shares2 = amm.add_liquidity(&lp2, &500_000_i128, &500_000_i128, &0_i128);

        assert_eq!(amm.get_info().total_shares, shares1 + shares2);

        amm.remove_liquidity(&lp1, &shares1, &0_i128, &0_i128);
        amm.remove_liquidity(&lp2, &shares2, &0_i128, &0_i128);
        assert_eq!(amm.get_info().total_shares, 0);
    }

    // ── Quotes ────────────────────────────────────────────────────────────────

    #[test]
    fn test_get_amount_out_matches_swap() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &0_i128);

        let amount_in = 50_000_i128;
        let quoted = amm.get_amount_out(&ts.ta_addr, &amount_in);

        let trader = Address::generate(env);
        ta_sac.mint(&trader, &amount_in);
        let actual = amm.swap(&trader, &ts.ta_addr, &amount_in, &0_i128);

        assert_eq!(quoted, actual);
    }

    #[test]
    fn test_sequential_swaps_invariant() {
        let ts = setup_pool(30); // 0.30% fee
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        // 1. Initial liquidity
        let provider = Address::generate(env);
        let initial_amt = 1_000_000_i128;
        ta_sac.mint(&provider, &initial_amt);
        tb_sac.mint(&provider, &initial_amt);
        amm.add_liquidity(&provider, &initial_amt, &initial_amt, &0_i128);

        let info = amm.get_info();
        let initial_k = info.reserve_a * info.reserve_b;
        let mut current_k = initial_k;

        // 2. Perform 10 alternating swaps
        let trader = Address::generate(env);
        let swap_amt = 10_000_i128;

        for i in 0..10 {
            if i % 2 == 0 {
                // A -> B
                ta_sac.mint(&trader, &swap_amt);
                amm.swap(&trader, &ts.ta_addr, &swap_amt, &0_i128);
            } else {
                // B -> A
                tb_sac.mint(&trader, &swap_amt);
                amm.swap(&trader, &ts.tb_addr, &swap_amt, &0_i128);
            }

            let new_info = amm.get_info();
            let new_k = new_info.reserve_a * new_info.reserve_b;

            // Invariant must hold: new_k >= initial_k
            assert!(
                new_k >= initial_k,
                "Invariant violated: new_k ({new_k}) < initial_k ({initial_k}) at swap {i}"
            );

            // k must grow (or stay same if fee is 0, but here it's 30bps)
            assert!(
                new_k >= current_k,
                "k decreased: new_k ({new_k}) < current_k ({current_k}) at swap {i}"
            );

            current_k = new_k;
        }

        // Final k should be strictly greater than initial k because of fees
        assert!(current_k > initial_k);
    }

    #[test]
    fn test_get_amount_in_round_trip() {
        let (env, admin, amm_addr, lp_addr, _) = setup();

        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(&ta_client.address, &tb_client.address, &lp_addr, &30_i128);

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);
        amm.add_liquidity(&provider, &1_000_000_i128, &2_000_000_i128, &0_i128);

        // Forward: how much B do we get for 100_000 A?
        let amount_in = 100_000_i128;
        let amount_out = amm.get_amount_out(&ta_client.address, &amount_in);
        assert!(amount_out > 0);

        // Reverse: how much A is needed to get exactly amount_out of B?
        let amount_in_reverse = amm.get_amount_in(&tb_client.address, &amount_out);

        // Due to integer rounding (+1 in get_amount_in), the reverse quote
        // should be >= the original input and at most 1 unit more.
        assert!(
            amount_in_reverse >= amount_in,
            "reverse quote should be >= original input"
        );
        assert!(
            amount_in_reverse <= amount_in + 1,
            "reverse quote should be at most 1 unit above original input"
        );
    }

    #[test]
    fn test_remove_liquidity_emits_event() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::{symbol_short, vec, IntoVal};

        let (env, admin, amm_addr, lp_addr, _) = setup();

        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(&ta_client.address, &tb_client.address, &lp_addr, &30_i128);

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);

        let shares = amm.add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &0_i128);
        amm.remove_liquidity(&provider, &shares, &0_i128, &0_i128);

        // Find the rm_liq event among all published events
        let events = env.events().all();
        let rm_liq_event = events
            .iter()
            .find(|(_, topics, _)| topics == &vec![&env, symbol_short!("rm_liq").into_val(&env)]);

        assert!(rm_liq_event.is_some(), "rm_liq event not emitted");
    }

    // ── Edge cases: zero-reserve guard ───────────────────────────────────────────

    #[test]
    fn test_swap_on_empty_pool_panics() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);

        let trader = Address::generate(env);
        ta_sac.mint(&trader, &1_000_i128);
        let result = amm.try_swap(&trader, &ts.ta_addr, &1_000_i128, &0_i128);
        assert!(result.is_err());
    }

    // ── Edge cases: fee boundary ──────────────────────────────────────────────────

    #[test]
    fn test_fee_bps_zero_succeeds() {
        let ts = setup_pool(0);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &0_i128);

        let trader = Address::generate(env);
        let amount_in = 100_000_i128;
        ta_sac.mint(&trader, &amount_in);
        let out = amm.swap(&trader, &ts.ta_addr, &amount_in, &0_i128);
        // fee_bps=0 → no discount; pure constant-product formula
        let expected = amount_in * 1_000_000 / (1_000_000 + amount_in);
        assert_eq!(out, expected);
    }

    #[test]
    fn test_fee_bps_max_succeeds() {
        // fee_bps=10_000 is the inclusive upper bound; pool initializes successfully.
        // With 100% fee, amount_in_with_fee = 0, so amount_out = 0.
        let ts = setup_pool(10_000);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &0_i128);

        let trader = Address::generate(env);
        ta_sac.mint(&trader, &100_000_i128);
        let result = amm.try_swap(&trader, &ts.ta_addr, &100_000_i128, &0_i128);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().unwrap(), 0);
    }

    // ── Edge cases: minimum share precision ──────────────────────────────────────

    #[test]
    fn test_min_shares_exact_succeeds() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        // Initial deposit: shares = sqrt(1_000_000 * 1_000_000) = 1_000_000
        let shares =
            amm.add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &1_000_000_i128);
        assert_eq!(shares, 1_000_000);
    }

    #[test]
    fn test_min_shares_off_by_one_panics() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        // Expected = 1_000_000; requesting 1_000_001 triggers the slippage guard.
        let result =
            amm.try_add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &1_000_001_i128);
        assert!(result.is_err());
    }
}
