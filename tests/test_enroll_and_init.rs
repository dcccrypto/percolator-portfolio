//! Integration tests for Tag 12 `EnrollMarketAndInit` instruction.
//!
//! Coverage strategy:
//!   All validations that fire BEFORE the InitUser CPI are exercised via
//!   direct account-data injection. The happy-path requires a real
//!   percolator-prog::InitUser round-trip (small-tier slab + funded vault)
//!   and is blocked on the integration_env InitMarket encoding mismatch;
//!   it is deferred with `#[ignore]`.
//!
//! EnrollMarketAndInit body: `tag(1) | expected_idx(u16) | fee_payment(u64)`
//!
//! Account layout (10):
//!   0. [signer]      user
//!   1. [writable]    portfolio_data PDA
//!   2. []            portfolio_auth PDA
//!   3. [writable]    portfolio_vault token account
//!   4. [writable]    user_ata (source of fee_payment)
//!   5. [writable]    market slab
//!   6. [writable]    market_vault (engine's destination)
//!   7. []            spl_token_program
//!   8. []            clock sysvar
//!   9. []            percolator-prog (executable)
//!
//! PortfolioAccount byte offsets (new layout):
//!   bump=110  auth_bump=111  vault_bump=112  version=113
//!   paused=114  enrolled_count=115  has_vault=116
//!   enrolled[i] at 120 + 48*i: last_seen_eq_e6(8) | market(32) | account_idx(u16) | _pad0(6)

mod common;

use common::{assert_custom_error, fresh_env, pdas_for, send_init, send_signed, percolator_owned_slab};
use percolator_portfolio::{
    constants::{MAX_ENROLLED_MARKETS, PORTFOLIO_AUTH_SEED, PORTFOLIO_VAULT_SEED},
    cpi as cpi_helpers,
    errors::PortfolioError,
};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    sysvar,
};

const PERCOLATOR_PROG: Pubkey = Pubkey::new_from_array(cpi_helpers::PERCOLATOR_PROGRAM_ID);
const SPL_TOKEN: Pubkey = Pubkey::new_from_array(cpi_helpers::SPL_TOKEN_ID);

// Layout offsets.
const OFF_PAUSED: usize = 114;
const OFF_ENROLLED_COUNT: usize = 115;
const OFF_HAS_VAULT: usize = 116;
const OFF_ENROLLED_BASE: usize = 184;
const SLOT_SIZE: usize = 48;
const SLOT_MARKET_OFF: usize = 8;
const SLOT_ACCT_IDX_OFF: usize = 40;

/// Encode tag-12 EnrollMarketAndInit body.
fn enroll_init_data(expected_idx: u16, fee_payment: u64) -> Vec<u8> {
    let mut d = vec![12u8];
    d.extend_from_slice(&expected_idx.to_le_bytes());
    d.extend_from_slice(&fee_payment.to_le_bytes());
    d
}

/// Build a 10-account EnrollMarketAndInit ix.
fn enroll_init_ix(
    program_id: Pubkey,
    user: Pubkey,
    data_pda: Pubkey,
    auth_pda: Pubkey,
    vault: Pubkey,
    user_ata: Pubkey,
    market_slab: Pubkey,
    data: Vec<u8>,
) -> Instruction {
    Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user, true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new(user_ata, false),
            AccountMeta::new(market_slab, false),
            AccountMeta::new(Pubkey::new_unique(), false), // market_vault
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
        ],
        data,
    }
}

/// Initialise a portfolio and patch has_vault=1 so vault-check passes.
fn setup_portfolio_with_vault(
    svm: &mut litesvm::LiteSVM,
    program_id: Pubkey,
    user: &Keypair,
) -> (Pubkey, Pubkey, Pubkey) {
    send_init(svm, program_id, user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, vault_bump) = Pubkey::find_program_address(
        &[PORTFOLIO_VAULT_SEED, user.pubkey().as_ref()],
        &program_id,
    );

    let mut acct = svm.get_account(&data_pda).unwrap();
    acct.data[OFF_HAS_VAULT] = 1;
    // Also store vault_bump at offset 112 so create_program_address succeeds.
    acct.data[112] = vault_bump;
    svm.set_account(data_pda, acct).unwrap();

    (data_pda, auth_pda, vault)
}

// ── Rejection tests ─────────────────────────────────────────────────────────

#[test]
fn enroll_init_rejects_zero_fee_payment() {
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, auth_pda, vault) = setup_portfolio_with_vault(&mut svm, program_id, &user);

    let ix = enroll_init_ix(
        program_id,
        user.pubkey(),
        data_pda,
        auth_pda,
        vault,
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        enroll_init_data(0, 0), // fee_payment = 0
    );
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::ZeroAmount as u32);
}

#[test]
fn enroll_init_rejects_wrong_signer() {
    // attacker attempts to enroll into victim's portfolio.
    let (mut svm, program_id, victim) = fresh_env();
    let (data_pda, auth_pda, vault) = setup_portfolio_with_vault(&mut svm, program_id, &victim);

    let attacker = Keypair::new();
    svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();

    let ix = enroll_init_ix(
        program_id,
        attacker.pubkey(), // wrong signer
        data_pda,
        auth_pda,
        vault,
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        enroll_init_data(0, 1_000),
    );
    let res = send_signed(&mut svm, ix, &attacker);
    assert_custom_error(res, PortfolioError::BadOwner as u32);
}

#[test]
fn enroll_init_rejects_paused() {
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, auth_pda, vault) = setup_portfolio_with_vault(&mut svm, program_id, &user);

    // Patch paused = 1.
    let mut acct = svm.get_account(&data_pda).unwrap();
    acct.data[OFF_PAUSED] = 1;
    svm.set_account(data_pda, acct).unwrap();

    let ix = enroll_init_ix(
        program_id,
        user.pubkey(),
        data_pda,
        auth_pda,
        vault,
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        enroll_init_data(0, 1_000),
    );
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::Paused as u32);
}

#[test]
fn enroll_init_rejects_when_full() {
    // Patch enrolled_count = MAX_ENROLLED_MARKETS → TooManyEnrolled.
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, auth_pda, vault) = setup_portfolio_with_vault(&mut svm, program_id, &user);

    let mut acct = svm.get_account(&data_pda).unwrap();
    acct.data[OFF_ENROLLED_COUNT] = MAX_ENROLLED_MARKETS as u8;
    svm.set_account(data_pda, acct).unwrap();

    let ix = enroll_init_ix(
        program_id,
        user.pubkey(),
        data_pda,
        auth_pda,
        vault,
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        enroll_init_data(0, 1_000),
    );
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::TooManyEnrolled as u32);
}

#[test]
fn enroll_init_rejects_duplicate_market() {
    // CRIT-6: enroll same market slab twice → MarketAlreadyEnrolled.
    // The check in EnrollMarketAndInit rejects ANY same-market duplicate
    // (even with different account_idx), unlike the old EnrollMarket which
    // only rejected same (market, idx) pairs.
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, auth_pda, vault) = setup_portfolio_with_vault(&mut svm, program_id, &user);
    let slab = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, slab);

    // Patch enrolled_count=1, enrolled[0].market = slab.
    let mut acct = svm.get_account(&data_pda).unwrap();
    acct.data[OFF_ENROLLED_COUNT] = 1;
    let s0 = OFF_ENROLLED_BASE + SLOT_MARKET_OFF;
    acct.data[s0..s0 + 32].copy_from_slice(&slab.to_bytes());
    svm.set_account(data_pda, acct).unwrap();

    // Try to enroll the same slab again → MarketAlreadyEnrolled.
    let ix = enroll_init_ix(
        program_id,
        user.pubkey(),
        data_pda,
        auth_pda,
        vault,
        Pubkey::new_unique(),
        slab, // same market slab
        enroll_init_data(1, 1_000), // different idx — still rejected
    );
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::MarketAlreadyEnrolled as u32);
}

#[test]
fn enroll_init_rejects_wrong_account_count() {
    // Only 9 accounts (need 10) → BadAccountCount.
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            // Missing 10th account (percolator-prog).
        ],
        data: enroll_init_data(0, 1_000),
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadAccountCount as u32);
}

#[test]
fn enroll_init_rejects_invalid_data_pda() {
    // Passing a wrong data PDA (bogus address) → BadPda.
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (_, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _) = Pubkey::find_program_address(
        &[PORTFOLIO_VAULT_SEED, user.pubkey().as_ref()],
        &program_id,
    );
    // Get real data_pda but pass a wrong one.
    let bogus_data = Pubkey::new_unique();

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(bogus_data, false), // wrong data PDA
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
        ],
        data: enroll_init_data(0, 1_000),
    };
    let res = send_signed(&mut svm, ix, &user);
    // bogus_data is not program-owned → AccountNotInitialized before BadPda.
    assert_custom_error(res, PortfolioError::AccountNotInitialized as u32);
}

#[test]
fn enroll_init_rejects_invalid_auth_pda() {
    // auth_pda doesn't match expected derivation → BadPda.
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, auth_pda, vault) = setup_portfolio_with_vault(&mut svm, program_id, &user);
    let bogus_auth = Pubkey::new_unique();

    let ix = enroll_init_ix(
        program_id,
        user.pubkey(),
        data_pda,
        bogus_auth, // wrong auth PDA
        vault,
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        enroll_init_data(0, 1_000),
    );
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadPda as u32);
}

#[test]
fn enroll_init_rejects_invalid_vault_pda() {
    // vault address doesn't match the stored vault_bump derivation → BadPda.
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, auth_pda, _real_vault) = setup_portfolio_with_vault(&mut svm, program_id, &user);
    let bogus_vault = Pubkey::new_unique();

    let ix = enroll_init_ix(
        program_id,
        user.pubkey(),
        data_pda,
        auth_pda,
        bogus_vault, // wrong vault PDA
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        enroll_init_data(0, 1_000),
    );
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadPda as u32);
}

#[test]
fn enroll_init_rejects_uninitialised_vault() {
    // has_vault = 0 → AccountNotInitialized.
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _) = Pubkey::find_program_address(
        &[PORTFOLIO_VAULT_SEED, user.pubkey().as_ref()],
        &program_id,
    );
    // has_vault is still 0 (default after InitPortfolio, vault not yet created).

    let ix = enroll_init_ix(
        program_id,
        user.pubkey(),
        data_pda,
        auth_pda,
        vault,
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        enroll_init_data(0, 1_000),
    );
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::AccountNotInitialized as u32);
}

// ── Happy-path placeholder ───────────────────────────────────────────────────
//
// EnrollMarketAndInit happy-path requires:
//   1. A fully-initialised percolator-prog market (via IntegrationEnv::new).
//   2. A funded portfolio_vault token account (via InitVault CPI).
//   3. encode_init_market_basic to succeed against the small-tier .so.
//
// Currently IntegrationEnv::new panics at InitMarket with Custom(4)
// (LeverageOutOfRange in the percolator-prog encoding). This is a
// harness-side encoding issue, not a bug in the portfolio program.
// Once resolved, the happy-path tests below can be un-ignored.

#[test]
#[ignore = "requires integration_env InitMarket encoding fix — harness-issue"]
fn enroll_init_happy_path_creates_engine_account_owned_by_auth() {
    // When the encoding mismatch is resolved:
    //   1. IntegrationEnv::new() to get a real slab + oracle.
    //   2. env.init_portfolio_and_vault() to create portfolio + vault.
    //   3. Call EnrollMarketAndInit with fee_payment >= new_account_fee.
    //   4. Verify enrolled_count == 1 in portfolio_data.
    //   5. Verify the engine account's owner == portfolio_auth PDA.
    todo!("harness-issue: InitMarket encoding mismatch")
}
