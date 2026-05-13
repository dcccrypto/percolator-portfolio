//! End-to-end integration test harness.
//!
//! Loads BOTH `percolator-prog` and `percolator-portfolio` .so files into
//! one LiteSVM, initialises an actual percolator-prog market (mint, vault,
//! Pyth oracle, RiskParams), funds a test user with USDC, and exposes
//! helpers for the portfolio program's CPI-using instructions.
//!
//! **Why a separate harness from `tests/common/mod.rs`?** Most existing
//! tests load only the portfolio .so and exercise wrapper-validation
//! paths. This harness is for the (much heavier) tests that drive real
//! CPIs into percolator-prog and verify token balances, slab state, and
//! conservation invariants.
//!
//! ## What's covered
//!
//! - `InitVault` end-to-end against a real mint
//! - Direct percolator-prog interactions (sanity that the .so is loaded)
//! - Future: Deposit / Withdraw / Rebalance / EmergencyClose round-trips
//!   once `EnrollMarketAndInit` (the InitUser-via-CPI path) is wired
//!   into the portfolio program. The current `EnrollMarket` is state-
//!   only; for a real Deposit, the per-market account must be created
//!   with `engine.account.owner == portfolio_auth`, which only the
//!   portfolio program can do via `invoke_signed`.
//!
//! ## Pyth setup
//!
//! Pyth `PriceUpdateV2` accounts are constructed manually via
//! `make_pyth_data` (134-byte layout, Borsh-discriminant + PriceFeedMessage).
//! See `~/percolator-prog/tests/common/mod.rs:198` for the canonical
//! reference; we copy the bytes.

#![allow(dead_code)]

use bytemuck::from_bytes;
use litesvm::LiteSVM;
use percolator_portfolio::{
    constants::{PORTFOLIO_AUTH_SEED, PORTFOLIO_SEED, PORTFOLIO_VAULT_SEED},
    cpi as cpi_helpers,
    state::PortfolioAccount,
};
use solana_program_runtime::compute_budget::ComputeBudget;
use solana_sdk::{
    account::Account,
    clock::Clock,
    instruction::{AccountMeta, Instruction},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_program, sysvar,
    transaction::Transaction,
};

const PORTFOLIO_SO: &str = "target/deploy/percolator_portfolio.so";
const PERCOLATOR_SO: &str = "../percolator-prog/target/deploy/percolator_prog.so";

/// Pyth Solana Receiver program ID.
pub const PYTH_RECEIVER_PROGRAM_ID: Pubkey = Pubkey::new_from_array([
    0x0c, 0xb7, 0xfa, 0xbb, 0x52, 0xf7, 0xa6, 0x48, 0xbb, 0x5b, 0x31, 0x7d, 0x9a, 0x01, 0x8b, 0x90,
    0x57, 0xcb, 0x02, 0x47, 0x74, 0xfa, 0xfe, 0x01, 0xe6, 0xc4, 0xdf, 0x98, 0xcc, 0x38, 0x58, 0x81,
]);

/// Test feed ID (just 0xAB repeated 32×).
pub const TEST_FEED_ID: [u8; 32] = [0xABu8; 32];

/// Slab size — must match the .so's compile-time SLAB_LEN exactly. The
/// build at `~/percolator-prog/target/deploy/percolator_prog.so` is the
/// small-tier (MAX_ACCOUNTS=256). If percolator-prog is rebuilt without
/// `--features small`, this jumps to ~1.5MB (medium tier).
///
/// Re-verify on every upstream sync wave: read the
/// `sol_log_64(SLAB_LEN, data.len(), …)` from a failed InitMarket;
/// the first hex value (SLAB_LEN) is the new size.
pub const SLAB_LEN: usize = 111_504;

/// Token v3 Account size.
pub const TOKEN_ACCOUNT_LEN: usize = 165;

/// Canonical SPL Token program ID, derived from cpi_helpers.
pub const SPL_TOKEN: Pubkey = Pubkey::new_from_array(cpi_helpers::SPL_TOKEN_ID);

/// Canonical percolator-prog program ID. We load the .so AT THIS ADDRESS so
/// `verify_percolator_program` passes in CPI calls from our wrapper.
pub const PERCOLATOR_PROG: Pubkey = Pubkey::new_from_array(cpi_helpers::PERCOLATOR_PROGRAM_ID);

/// Canonical portfolio program ID, matching `declare_id!`.
pub fn portfolio_program_id() -> Pubkey {
    percolator_portfolio::id()
}

/// Construct PriceUpdateV2 account data for litesvm. 134 bytes; layout
/// verified against percolator-prog's harness.
pub fn make_pyth_data(
    feed_id: &[u8; 32],
    price: i64,
    expo: i32,
    conf: u64,
    publish_time: i64,
) -> Vec<u8> {
    let mut data = vec![0u8; 134];
    data[40] = 1; // VerificationLevel::Full discriminant
    data[41..73].copy_from_slice(feed_id);
    data[73..81].copy_from_slice(&price.to_le_bytes());
    data[81..89].copy_from_slice(&conf.to_le_bytes());
    data[89..93].copy_from_slice(&expo.to_le_bytes());
    data[93..101].copy_from_slice(&publish_time.to_le_bytes());
    data
}

/// Construct an SPL Token v3 mint account body.
pub fn make_mint_data() -> Vec<u8> {
    let mut data = vec![0u8; spl_token::state::Mint::LEN];
    let mint = spl_token::state::Mint {
        mint_authority: solana_sdk::program_option::COption::None,
        supply: 0,
        decimals: 6,
        is_initialized: true,
        freeze_authority: solana_sdk::program_option::COption::None,
    };
    spl_token::state::Mint::pack(mint, &mut data).unwrap();
    data
}

/// Construct an SPL Token v3 token account body.
pub fn make_token_account_data(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Vec<u8> {
    let mut data = vec![0u8; TOKEN_ACCOUNT_LEN];
    let mut account = spl_token::state::Account::default();
    account.mint = *mint;
    account.owner = *owner;
    account.amount = amount;
    account.state = spl_token::state::AccountState::Initialized;
    spl_token::state::Account::pack(account, &mut data).unwrap();
    data
}

/// Encode `percolator-prog::InitMarket` instruction body.
///
/// This mirrors the canonical encoder pattern. RiskParams uses sensible
/// defaults that pass the engine's resolvability invariant. See the
/// "Copy-Paste Init Sequence" in the agent's CPI surface map for the
/// authoritative field-by-field source.
pub fn encode_init_market_basic(admin: &Pubkey, mint: &Pubkey) -> Vec<u8> {
    let mut d = vec![0u8]; // tag = InitMarket
    d.extend_from_slice(admin.as_ref());
    d.extend_from_slice(mint.as_ref());
    d.extend_from_slice(&TEST_FEED_ID);
    d.extend_from_slice(&86_400u64.to_le_bytes()); // max_staleness_secs
    d.extend_from_slice(&500u16.to_le_bytes()); // conf_filter_bps
    d.push(0u8); // invert
    d.extend_from_slice(&0u32.to_le_bytes()); // unit_scale = 0 (1:1)
    d.extend_from_slice(&0u64.to_le_bytes()); // initial_mark_price_e6 (non-Hyperp)
    d.extend_from_slice(&0u128.to_le_bytes()); // legacy maintenance_fee_per_slot

    // RiskParams (in declaration order):
    d.extend_from_slice(&1u64.to_le_bytes()); // h_min
    d.extend_from_slice(&500u64.to_le_bytes()); // maintenance_margin_bps
    d.extend_from_slice(&1000u64.to_le_bytes()); // initial_margin_bps
    d.extend_from_slice(&0u64.to_le_bytes()); // trading_fee_bps
    d.extend_from_slice(&256u64.to_le_bytes()); // max_accounts
    d.extend_from_slice(&1u128.to_le_bytes()); // new_account_fee
    d.extend_from_slice(&0u128.to_le_bytes()); // insurance_floor
    d.extend_from_slice(&1u64.to_le_bytes()); // h_max
    d.extend_from_slice(&50u64.to_le_bytes()); // max_crank_staleness_slots
    d.extend_from_slice(&50u64.to_le_bytes()); // liquidation_fee_bps
    d.extend_from_slice(&1_000_000_000_000u128.to_le_bytes()); // liquidation_fee_cap
    d.extend_from_slice(&100u64.to_le_bytes()); // resolve_price_deviation_bps
    d.extend_from_slice(&0u128.to_le_bytes()); // min_liquidation_abs
    d.extend_from_slice(&21u128.to_le_bytes()); // min_nonzero_mm_req
    d.extend_from_slice(&22u128.to_le_bytes()); // min_nonzero_im_req

    // Extended tail:
    d.extend_from_slice(&0u16.to_le_bytes()); // insurance_withdraw_max_bps
    d.extend_from_slice(&0u64.to_le_bytes()); // insurance_withdraw_cooldown_slots
    d.extend_from_slice(&200u64.to_le_bytes()); // permissionless_resolve_stale_slots
    d.extend_from_slice(&500u64.to_le_bytes()); // funding_horizon_slots
    d.extend_from_slice(&100u64.to_le_bytes()); // funding_k_bps
    d.extend_from_slice(&500i64.to_le_bytes()); // funding_max_premium_bps
    d.extend_from_slice(&1_000i64.to_le_bytes()); // funding_max_e9_per_slot
    d.extend_from_slice(&0u64.to_le_bytes()); // mark_min_fee
    d.extend_from_slice(&50u64.to_le_bytes()); // force_close_delay_slots

    d
}

/// The integration test environment. Builds a fully-initialised market
/// in percolator-prog with a USDC-equivalent mint, a vault, and a working
/// Pyth oracle. The user has been funded with USDC.
pub struct IntegrationEnv {
    pub svm: LiteSVM,
    pub portfolio_program_id: Pubkey,
    pub admin: Keypair,
    pub user: Keypair,
    pub user_ata: Pubkey,
    pub mint: Pubkey,
    pub slab: Pubkey,
    pub market_vault: Pubkey,
    pub market_vault_authority: Pubkey,
    pub oracle: Pubkey,
}

impl IntegrationEnv {
    /// Build a fresh integration env with both .so files loaded, an
    /// initialised market, and a user funded with `user_balance` USDC base
    /// units.
    pub fn new(user_balance: u64) -> Self {
        let mut svm = LiteSVM::new();
        // Bump CU because debug-built BPF burns more than mainnet release.
        svm.set_compute_budget(ComputeBudget {
            compute_unit_limit: 50_000_000,
            heap_size: 256 * 1024,
            ..ComputeBudget::default()
        });

        let portfolio_id = portfolio_program_id();
        svm.add_program_from_file(portfolio_id, PORTFOLIO_SO).unwrap();
        svm.add_program_from_file(PERCOLATOR_PROG, PERCOLATOR_SO)
            .expect("percolator-prog .so missing — `cd ~/percolator-prog && cargo build-sbf`");

        let admin = Keypair::new();
        let user = Keypair::new();
        svm.airdrop(&admin.pubkey(), 100_000_000_000).unwrap();
        svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

        // Slab address — random; the engine's PDA is the vault auth, not
        // the slab itself.
        let slab = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let oracle = Pubkey::new_unique();
        let (vault_auth, _) =
            Pubkey::find_program_address(&[b"vault", slab.as_ref()], &PERCOLATOR_PROG);
        let market_vault = Pubkey::new_unique();
        let user_ata = Pubkey::new_unique();

        // Allocate slab as a program-owned, zero-data account of size SLAB_LEN.
        svm.set_account(
            slab,
            Account {
                lamports: 1_000_000_000,
                data: vec![0u8; SLAB_LEN],
                owner: PERCOLATOR_PROG,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

        // Mint.
        svm.set_account(
            mint,
            Account {
                lamports: 1_000_000,
                data: make_mint_data(),
                owner: SPL_TOKEN,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

        // Market vault — owned by SPL Token, authority = vault_auth PDA, balance 0.
        svm.set_account(
            market_vault,
            Account {
                lamports: 1_000_000,
                data: make_token_account_data(&mint, &vault_auth, 0),
                owner: SPL_TOKEN,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

        // User's USDC ATA — owned by user, holds `user_balance` tokens.
        svm.set_account(
            user_ata,
            Account {
                lamports: 1_000_000,
                data: make_token_account_data(&mint, &user.pubkey(), user_balance),
                owner: SPL_TOKEN,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

        // Pyth oracle — Full verification, $138 price.
        let pyth_data = make_pyth_data(&TEST_FEED_ID, 138_000_000, -6, 1, 100);
        svm.set_account(
            oracle,
            Account {
                lamports: 1_000_000,
                data: pyth_data,
                owner: PYTH_RECEIVER_PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

        // Set the clock so oracle isn't stale.
        svm.set_sysvar(&Clock {
            slot: 100,
            unix_timestamp: 100,
            ..Clock::default()
        });

        // Now actually init the market via percolator-prog::InitMarket.
        let init_ix = Instruction {
            program_id: PERCOLATOR_PROG,
            accounts: vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new(slab, false),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new(market_vault, false),
                AccountMeta::new_readonly(SPL_TOKEN, false),
                AccountMeta::new_readonly(sysvar::clock::ID, false),
                AccountMeta::new_readonly(sysvar::rent::ID, false),
                AccountMeta::new_readonly(oracle, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data: encode_init_market_basic(&admin.pubkey(), &mint),
        };

        let tx = Transaction::new_signed_with_payer(
            &[
                solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(
                    1_400_000,
                ),
                init_ix,
            ],
            Some(&admin.pubkey()),
            &[&admin],
            svm.latest_blockhash(),
        );
        svm.send_transaction(tx).expect("InitMarket failed");

        Self {
            svm,
            portfolio_program_id: portfolio_id,
            admin,
            user,
            user_ata,
            mint,
            slab,
            market_vault,
            market_vault_authority: vault_auth,
            oracle,
        }
    }

    /// Run InitPortfolio + InitVault for the env's user. Returns the
    /// PDAs the harness's user will use for portfolio operations.
    pub fn init_portfolio_and_vault(&mut self) -> (Pubkey, Pubkey, Pubkey) {
        let user = self.user.insecure_clone();
        let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &self.portfolio_program_id);
        let (vault, _) = Pubkey::find_program_address(
            &[PORTFOLIO_VAULT_SEED, user.pubkey().as_ref()],
            &self.portfolio_program_id,
        );

        // InitPortfolio (tag 0).
        let mut data = vec![0u8];
        data.extend_from_slice(&200u16.to_le_bytes());
        data.extend_from_slice(&50_000u32.to_le_bytes());
        data.extend_from_slice(Pubkey::new_unique().as_ref()); // keeper
        let init_ix = Instruction {
            program_id: self.portfolio_program_id,
            accounts: vec![
                AccountMeta::new(user.pubkey(), true),
                AccountMeta::new(data_pda, false),
                AccountMeta::new_readonly(auth_pda, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data,
        };
        let tx = Transaction::new_signed_with_payer(
            &[init_ix],
            Some(&user.pubkey()),
            &[&user],
            self.svm.latest_blockhash(),
        );
        self.svm.send_transaction(tx).expect("InitPortfolio");
        self.svm.expire_blockhash();

        // InitVault (tag 10).
        let init_vault_ix = Instruction {
            program_id: self.portfolio_program_id,
            accounts: vec![
                AccountMeta::new(user.pubkey(), true),
                AccountMeta::new(data_pda, false),
                AccountMeta::new_readonly(auth_pda, false),
                AccountMeta::new(vault, false),
                AccountMeta::new_readonly(self.mint, false),
                AccountMeta::new_readonly(system_program::ID, false),
                AccountMeta::new_readonly(SPL_TOKEN, false),
            ],
            data: vec![10u8],
        };
        let tx = Transaction::new_signed_with_payer(
            &[init_vault_ix],
            Some(&user.pubkey()),
            &[&user],
            self.svm.latest_blockhash(),
        );
        self.svm.send_transaction(tx).expect("InitVault");

        (data_pda, auth_pda, vault)
    }

    /// Read the user's USDC balance.
    pub fn user_ata_balance(&self) -> u64 {
        let acct = self.svm.get_account(&self.user_ata).unwrap();
        spl_token::state::Account::unpack(&acct.data).unwrap().amount
    }

    /// Read the market vault's USDC balance (in base units).
    pub fn market_vault_balance(&self) -> u64 {
        let acct = self.svm.get_account(&self.market_vault).unwrap();
        spl_token::state::Account::unpack(&acct.data).unwrap().amount
    }

    /// Read a portfolio_vault's USDC balance.
    pub fn portfolio_vault_balance(&self, vault: &Pubkey) -> u64 {
        let acct = self.svm.get_account(vault).unwrap();
        spl_token::state::Account::unpack(&acct.data).unwrap().amount
    }

    /// Read the portfolio data PDA fully decoded.
    pub fn read_portfolio(&self, data_pda: &Pubkey) -> PortfolioAccount {
        let acct = self.svm.get_account(data_pda).unwrap();
        *from_bytes::<PortfolioAccount>(
            &acct.data[..core::mem::size_of::<PortfolioAccount>()],
        )
    }

    /// Bump the slot+timestamp so freshness checks don't trip.
    pub fn warp(&mut self, delta_slots: u64) {
        let mut c = self.svm.get_sysvar::<Clock>();
        c.slot += delta_slots;
        c.unix_timestamp += delta_slots as i64 / 2;
        self.svm.set_sysvar(&c);
    }
}

/// Local copy of pdas_for to avoid coupling to common::mod.rs.
fn pdas_for(user: &Pubkey, program_id: &Pubkey) -> (Pubkey, u8, Pubkey, u8) {
    let (data, db) = Pubkey::find_program_address(&[PORTFOLIO_SEED, user.as_ref()], program_id);
    let (auth, ab) = Pubkey::find_program_address(&[PORTFOLIO_AUTH_SEED, user.as_ref()], program_id);
    (data, db, auth, ab)
}
