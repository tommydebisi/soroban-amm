//! Constant-product AMM (x * y = k) on Soroban.
//!
//! Flow:
//!   1. Deploy this contract + two asset token contracts.
//!   2. Call `initialize` with both token addresses.
//!   3. First LP calls `add_liquidity` to seed the pool.
//!   4. Traders call `swap` to exchange tokens.
//!   5. LPs call `remove_liquidity` to redeem their share.

#![no_std]

use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short, Address, Env, Symbol,
};
// Standard SEP-41 interface for pool tokens (token_a, token_b)
use soroban_sdk::token::Client as SepTokenClient;
// Our custom LP token client (has mint + burn)
use token::LpTokenClient;

// ── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    TokenA,
    TokenB,
    LpToken,
    ReserveA,
    ReserveB,
    TotalShares,
    Shares(Address),
    FeeBps, // fee in basis points, e.g. 30 = 0.30 %
}

// ── Pool info returned by `get_info` ─────────────────────────────────────────

#[contracttype]
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

    /// Initialize the pool.
    ///
    /// `lp_token` must already be deployed and its admin set to this contract.
    pub fn initialize(
        env: Env,
        token_a: Address,
        token_b: Address,
        lp_token: Address,
        fee_bps: i128, // recommended: 30 (0.30 %)
    ) {
        if env.storage().instance().has(&DataKey::TokenA) {
            panic!("already initialized");
        }
        assert!(token_a != token_b, "tokens must differ");
        assert!(fee_bps >= 0 && fee_bps <= 10_000, "invalid fee");

        env.storage().instance().set(&DataKey::TokenA, &token_a);
        env.storage().instance().set(&DataKey::TokenB, &token_b);
        env.storage().instance().set(&DataKey::LpToken, &lp_token);
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        env.storage().instance().set(&DataKey::ReserveA, &0_i128);
        env.storage().instance().set(&DataKey::ReserveB, &0_i128);
        env.storage().instance().set(&DataKey::TotalShares, &0_i128);
    }

    // ── Liquidity ─────────────────────────────────────────────────────────────

    /// Deposit `amount_a` of token_a and `amount_b` of token_b.
    ///
    /// On the first deposit any ratio is accepted. Subsequent deposits must
    /// match the current pool ratio (within rounding); excess is *not* refunded
    /// automatically — callers should compute amounts off-chain first.
    pub fn add_liquidity(
        env: Env,
        provider: Address,
        amount_a: i128,
        amount_b: i128,
        min_shares: i128,
    ) -> i128 {
        provider.require_auth();
        assert!(amount_a > 0 && amount_b > 0, "amounts must be positive");

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

        assert!(shares >= min_shares, "slippage: insufficient shares minted");

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

        // Credit LP shares.
        let prev = Self::shares_of(env.clone(), provider.clone());
        env.storage()
            .persistent()
            .set(&DataKey::Shares(provider.clone()), &(prev + shares));

        // Mint LP tokens.
        let lp_client = LpTokenClient::new(&env, &lp_token);
        lp_client.mint(&provider, &shares);

        env.events().publish(
            (Symbol::new(&env, "add_liquidity"), provider),
            (amount_a, amount_b, shares),
        );

        shares
    }

    /// Burn `shares` LP tokens and receive back a proportional amount of each token.
    pub fn remove_liquidity(
        env: Env,
        provider: Address,
        shares: i128,
        min_a: i128,
        min_b: i128,
    ) -> (i128, i128) {
        provider.require_auth();
        assert!(shares > 0, "shares must be positive");

        let owned = Self::shares_of(env.clone(), provider.clone());
        assert!(owned >= shares, "insufficient LP shares");

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();

        let reserve_a = Self::get_reserve_a(env.clone());
        let reserve_b = Self::get_reserve_b(env.clone());
        let total_shares = Self::get_total_shares(env.clone());

        let out_a = shares * reserve_a / total_shares;
        let out_b = shares * reserve_b / total_shares;

        assert!(out_a >= min_a, "slippage: insufficient token_a out");
        assert!(out_b >= min_b, "slippage: insufficient token_b out");

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
        env.storage()
            .persistent()
            .set(&DataKey::Shares(provider.clone()), &(owned - shares));

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

    /// Swap an exact `amount_in` of `token_in` for at least `min_out` of the other token.
    ///
    /// Uses the constant-product formula with fee deducted from `amount_in`.
    pub fn swap(
        env: Env,
        trader: Address,
        token_in: Address,
        amount_in: i128,
        min_out: i128,
    ) -> i128 {
        trader.require_auth();
        assert!(amount_in > 0, "amount_in must be positive");

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();

        let (reserve_in, reserve_out, token_out) = if token_in == token_a {
            (Self::get_reserve_a(env.clone()), Self::get_reserve_b(env.clone()), token_b.clone())
        } else if token_in == token_b {
            (Self::get_reserve_b(env.clone()), Self::get_reserve_a(env.clone()), token_a.clone())
        } else {
            panic!("token_in is not part of this pool");
        };

        assert!(reserve_in > 0 && reserve_out > 0, "pool is empty");

        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap();

        // amount_in after fee
        let amount_in_with_fee = amount_in * (10_000 - fee_bps);
        // constant-product: out = (amount_in_with_fee * reserve_out) / (reserve_in * 10_000 + amount_in_with_fee)
        let amount_out =
            amount_in_with_fee * reserve_out / (reserve_in * 10_000 + amount_in_with_fee);

        assert!(amount_out >= min_out, "slippage: insufficient output amount");
        assert!(amount_out < reserve_out, "insufficient liquidity");

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

    /// Quote how much `token_out` you receive for `amount_in` of `token_in`.
    pub fn get_amount_out(env: Env, token_in: Address, amount_in: i128) -> i128 {
        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap();

        let (reserve_in, reserve_out) = if token_in == token_a {
            (Self::get_reserve_a(env.clone()), Self::get_reserve_b(env.clone()))
        } else if token_in == token_b {
            (Self::get_reserve_b(env.clone()), Self::get_reserve_a(env.clone()))
        } else {
            panic!("unknown token");
        };

        let amount_in_with_fee = amount_in * (10_000 - fee_bps);
        amount_in_with_fee * reserve_out / (reserve_in * 10_000 + amount_in_with_fee)
    }

    /// Return full pool state.
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

    pub fn shares_of(env: Env, provider: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Shares(provider))
            .unwrap_or(0)
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    fn get_reserve_a(env: Env) -> i128 {
        env.storage().instance().get(&DataKey::ReserveA).unwrap_or(0)
    }

    fn get_reserve_b(env: Env) -> i128 {
        env.storage().instance().get(&DataKey::ReserveB).unwrap_or(0)
    }

    fn get_total_shares(env: Env) -> i128 {
        env.storage().instance().get(&DataKey::TotalShares).unwrap_or(0)
    }

    /// Integer square root via Newton's method.
    fn sqrt(n: i128) -> i128 {
        if n < 0 {
            panic!("sqrt of negative");
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

    /// Register a Stellar Asset Contract and return (TokenClient, StellarAssetClient).
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

    fn setup() -> (Env, Address, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);

        // LP token: custom contract, admin = AMM so it can mint/burn
        let lp_addr = env.register_contract(None, LpToken);
        let lp_init = token::LpTokenClient::new(&env, &lp_addr);
        lp_init.initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        (env, admin, amm_addr, lp_addr, admin)
    }

    #[test]
    fn test_add_and_swap() {
        let (env, admin, amm_addr, lp_addr, _) = setup();

        // Pool tokens: use SAC for easy minting in tests
        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(&ta_client.address, &tb_client.address, &lp_addr, &30_i128);

        // Mint tokens to provider
        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);

        // Add initial liquidity
        let shares = amm.add_liquidity(&provider, &1_000_000_i128, &2_000_000_i128, &0_i128);
        assert!(shares > 0);

        let info = amm.get_info();
        assert_eq!(info.reserve_a, 1_000_000);
        assert_eq!(info.reserve_b, 2_000_000);

        // Swap 100_000 A → B
        let trader = Address::generate(&env);
        ta_sac.mint(&trader, &100_000_i128);

        let out = amm.swap(&trader, &ta_client.address, &100_000_i128, &0_i128);
        assert!(out > 0);
        assert!(out < 200_000); // slightly less than 2x due to fee + price impact
    }

    #[test]
    fn test_remove_liquidity() {
        let (env, admin, amm_addr, lp_addr, _) = setup();

        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(&ta_client.address, &tb_client.address, &lp_addr, &30_i128);

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);

        let shares = amm.add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &0_i128);

        let (out_a, out_b) = amm.remove_liquidity(&provider, &shares, &0_i128, &0_i128);
        assert!(out_a > 0 && out_b > 0);

        let info = amm.get_info();
        assert_eq!(info.total_shares, 0);
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
        let (out_a, out_b) = amm.remove_liquidity(&provider, &shares, &0_i128, &0_i128);

        // Find the rm_liq event among all published events
        let events = env.events().all();
        let rm_liq_event = events.iter().find(|(_, topics, _)| {
            topics == &vec![&env, symbol_short!("rm_liq").into_val(&env)]
        });

        assert!(rm_liq_event.is_some(), "rm_liq event not emitted");

        let (_, _, data) = rm_liq_event.unwrap();
        let expected: soroban_sdk::Val =
            (provider.clone(), shares, out_a, out_b).into_val(&env);
        assert_eq!(data, expected);
    }
}
