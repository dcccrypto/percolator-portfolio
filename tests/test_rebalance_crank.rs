//! Integration tests for Tag 13 `RebalanceCrank` instruction — Defense 3.
//!
//! Coverage strategy:
//!   All tests exercise the wrapper's pre-CPI validation paths. The actual
//!   Withdraw+Deposit CPIs and bounty transfer are NOT tested here — they
//!   require a fully-initialised percolator-prog engine account with
//!   below-IM state (which needs the integration_env InitMarket path, which
//!   currently fails against the small-tier .so). Those are merge-pending
//!   once the percolator-prog InitMarket encoding is corrected.
//!
//! RebalanceCrank body: `tag(1) | from_idx(u16) | to_idx(u16) | amount(u64)`
//!
//! Account layout (15 fixed):
//!   0. [signer]      caller (any)
//!   1. [writable]    portfolio_data PDA
//!   2. []            portfolio_auth PDA
//!   3. [writable]    portfolio_vault token account
//!   4. [writable]    caller_payout_ata (bounty destination)
//!   5. []            spl_token_program
//!   6. []            clock sysvar
//!   7. []            percolator-prog (executable)
//!   8. [writable]    from_slab
//!   9. [writable]    from_market_vault
//!  10. []            from_market_vault_authority
//!  11. []            from_oracle
//!  12. [writable]    to_slab
//!  13. [writable]    to_market_vault
//!  14. []            to_oracle
//!
//! PortfolioAccount byte offsets (new layout):
//!   bump=110  auth_bump=111  vault_bump=112  version=113
//!   paused=114  enrolled_count=115  has_vault=116
//!   enrolled[i] at 184 + 48*i: last_seen_eq_e6(8) | market(32) | account_idx(u16) | _pad0(6)

mod common;

use common::{assert_custom_error, fresh_env, pdas_for, send_init, send_signed};
use percolator_portfolio::{
    cpi as cpi_helpers,
    errors::PortfolioError,
};
use solana_sdk::{
    account::Account,
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
const OFF_VAULT_BUMP: usize = 112;
const OFF_ENROLLED_BASE: usize = 184;
const SLOT_SIZE: usize = 48;
const SLOT_MARKET_OFF: usize = 8;
const SLOT_ACCT_IDX_OFF: usize = 40;

/// Encode tag-13 RebalanceCrank body.
fn crank_data(from_idx: u16, to_idx: u16, amount: u64) -> Vec<u8> {
    let mut d = vec![13u8];
    d.extend_from_slice(&from_idx.to_le_bytes());
    d.extend_from_slice(&to_idx.to_le_bytes());
    d.extend_from_slice(&amount.to_le_bytes());
    d
}

/// Set up a portfolio with two enrolled slabs, has_vault=1, and vault_bump
/// set to the canonical value. Returns (data_pda, auth_pda, vault_pda).
/// This is test-only state injection: NOT a real InitUser CPI.
fn setup_portfolio_two_markets(
    svm: &mut litesvm::LiteSVM,
    program_id: Pubkey,
    user: &Keypair,
    slab_from: Pubkey,
    from_idx: u16,
    slab_to: Pubkey,
    to_idx: u16,
) -> (Pubkey, Pubkey, Pubkey) {
    send_init(svm, program_id, user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);

    // Derive the canonical vault PDA so the handler's PDA check passes.
    let vault_seed = percolator_portfolio::constants::PORTFOLIO_VAULT_SEED;
    let (vault_pda, vault_bump) =
        Pubkey::find_program_address(&[vault_seed, user.pubkey().as_ref()], &program_id);

    let mut acct = svm.get_account(&data_pda).unwrap();
    acct.data[OFF_HAS_VAULT] = 1;
    acct.data[OFF_VAULT_BUMP] = vault_bump;
    acct.data[OFF_ENROLLED_COUNT] = 2;
    // enrolled[0]
    let s0 = OFF_ENROLLED_BASE + SLOT_MARKET_OFF;
    acct.data[s0..s0 + 32].copy_from_slice(&slab_from.to_bytes());
    let i0 = OFF_ENROLLED_BASE + SLOT_ACCT_IDX_OFF;
    acct.data[i0..i0 + 2].copy_from_slice(&from_idx.to_le_bytes());
    // enrolled[1]
    let s1 = OFF_ENROLLED_BASE + SLOT_SIZE + SLOT_MARKET_OFF;
    acct.data[s1..s1 + 32].copy_from_slice(&slab_to.to_bytes());
    let i1 = OFF_ENROLLED_BASE + SLOT_SIZE + SLOT_ACCT_IDX_OFF;
    acct.data[i1..i1 + 2].copy_from_slice(&to_idx.to_le_bytes());
    svm.set_account(data_pda, acct).unwrap();

    (data_pda, auth_pda, vault_pda)
}

/// Build a 15-account RebalanceCrank ix.
fn crank_ix(
    program_id: Pubkey,
    caller: Pubkey,
    data_pda: Pubkey,
    auth_pda: Pubkey,
    vault: Pubkey,
    payout: Pubkey,
    from_slab: Pubkey,
    to_slab: Pubkey,
    data: Vec<u8>,
) -> Instruction {
    Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(caller, true),          // 0: caller
            AccountMeta::new(data_pda, false),                // 1: portfolio_data
            AccountMeta::new_readonly(auth_pda, false),       // 2: portfolio_auth
            AccountMeta::new(vault, false),                   // 3: portfolio_vault
            AccountMeta::new(payout, false),                  // 4: caller_payout_ata
            AccountMeta::new_readonly(SPL_TOKEN, false),      // 5: token_program
            AccountMeta::new_readonly(sysvar::clock::ID, false), // 6: clock
            AccountMeta::new_readonly(PERCOLATOR_PROG, false), // 7: percolator_prog
            AccountMeta::new(from_slab, false),               // 8: from_slab
            AccountMeta::new(Pubkey::new_unique(), false),    // 9: from_market_vault
            AccountMeta::new_readonly(Pubkey::new_unique(), false), // 10: from_vault_auth
            AccountMeta::new_readonly(Pubkey::new_unique(), false), // 11: from_oracle
            AccountMeta::new(to_slab, false),                 // 12: to_slab
            AccountMeta::new(Pubkey::new_unique(), false),    // 13: to_market_vault
            AccountMeta::new_readonly(Pubkey::new_unique(), false), // 14: to_oracle
        ],
        data,
    }
}

// ── Rejection tests ─────────────────────────────────────────────────────────

#[test]
fn crank_rejects_zero_amount() {
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let from_slab = Pubkey::new_unique();
    let to_slab = Pubkey::new_unique();
    setup_portfolio_two_markets(&mut svm, program_id, &user, from_slab, 0, to_slab, 1);

    let ix = crank_ix(
        program_id,
        user.pubkey(),
        data_pda,
        auth_pda,
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        from_slab,
        to_slab,
        crank_data(0, 1, 0), // amount = 0
    );
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::ZeroAmount as u32);
}

#[test]
fn crank_rejects_self_leg() {
    // from_slab == to_slab AND from_idx == to_idx → CrankSelfLeg.
    let (mut svm, program_id, user) = fresh_env();
    let slab = Pubkey::new_unique();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    setup_portfolio_two_markets(&mut svm, program_id, &user, slab, 0, Pubkey::new_unique(), 1);

    let ix = crank_ix(
        program_id,
        user.pubkey(),
        data_pda,
        auth_pda,
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        slab,
        slab, // same slab
        crank_data(0, 0, 1_000), // same from_idx == to_idx
    );
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::CrankSelfLeg as u32);
}

#[test]
fn crank_rejects_unsigned_caller() {
    // Not signing the transaction → WrongSigner. LiteSVM raises a tx-level
    // error when the instruction's is_signer=true account doesn't match the
    // transaction signers. We test via a_caller.is_signer=false path by
    // using AccountMeta::new_readonly (not signer) and signing with a
    // different key.
    //
    // NOTE: Solana's runtime enforces signer presence at the transaction
    // level — a non-signer account marked `is_signer=true` in AccountMeta
    // causes the tx to fail before our handler runs. So we test the
    // wrapper's own `!a_caller.is_signer` guard by marking it non-signer
    // in the instruction while still having valid accounts otherwise.
    let (mut svm, program_id, user) = fresh_env();
    let from_slab = Pubkey::new_unique();
    let to_slab = Pubkey::new_unique();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    setup_portfolio_two_markets(&mut svm, program_id, &user, from_slab, 0, to_slab, 1);

    // Build the same ix but use a fresh random pubkey as caller so that
    // a_caller.is_signer = false in the program. Using user.pubkey() would
    // not work because the fee-payer is always marked signer by the runtime
    // regardless of AccountMeta flags.
    let attacker = Pubkey::new_unique(); // unsigned, no keypair
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(attacker, false), // NOT is_signer
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
            AccountMeta::new(from_slab, false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new(to_slab, false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
        ],
        data: crank_data(0, 1, 1_000),
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::WrongSigner as u32);
}

#[test]
fn crank_rejects_paused() {
    let (mut svm, program_id, user) = fresh_env();
    let from_slab = Pubkey::new_unique();
    let to_slab = Pubkey::new_unique();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    setup_portfolio_two_markets(&mut svm, program_id, &user, from_slab, 0, to_slab, 1);

    // Patch paused = 1.
    let mut acct = svm.get_account(&data_pda).unwrap();
    acct.data[OFF_PAUSED] = 1;
    svm.set_account(data_pda, acct).unwrap();

    let ix = crank_ix(
        program_id,
        user.pubkey(),
        data_pda,
        auth_pda,
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        from_slab,
        to_slab,
        crank_data(0, 1, 1_000),
    );
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::Paused as u32);
}

#[test]
fn crank_rejects_unenrolled_from() {
    // from_slab not in enrolled[] → MarketNotEnrolled.
    let (mut svm, program_id, user) = fresh_env();
    let to_slab = Pubkey::new_unique();
    let bogus_from = Pubkey::new_unique();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    // Only to_slab is enrolled.
    let real_from = Pubkey::new_unique();
    setup_portfolio_two_markets(&mut svm, program_id, &user, real_from, 0, to_slab, 1);

    let ix = crank_ix(
        program_id,
        user.pubkey(),
        data_pda,
        auth_pda,
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        bogus_from, // not enrolled
        to_slab,
        crank_data(0, 1, 1_000),
    );
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::MarketNotEnrolled as u32);
}

#[test]
fn crank_rejects_unenrolled_to() {
    // to_slab not in enrolled[] → MarketNotEnrolled.
    let (mut svm, program_id, user) = fresh_env();
    let from_slab = Pubkey::new_unique();
    let bogus_to = Pubkey::new_unique();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let real_to = Pubkey::new_unique();
    setup_portfolio_two_markets(&mut svm, program_id, &user, from_slab, 0, real_to, 1);

    let ix = crank_ix(
        program_id,
        user.pubkey(),
        data_pda,
        auth_pda,
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        from_slab,
        bogus_to, // not enrolled
        crank_data(0, 1, 1_000),
    );
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::MarketNotEnrolled as u32);
}

#[test]
fn crank_rejects_spoofed_data() {
    // CRIT-1: a_data.owner != program_id → AccountNotInitialized.
    // Pass user's wallet as a_data (system-program-owned).
    let (mut svm, program_id, user) = fresh_env();
    let from_slab = Pubkey::new_unique();
    let to_slab = Pubkey::new_unique();
    let (_, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    setup_portfolio_two_markets(&mut svm, program_id, &user, from_slab, 0, to_slab, 1);

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(user.pubkey(), false), // wallet, NOT PDA → owner = system_program
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
            AccountMeta::new(from_slab, false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new(to_slab, false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
        ],
        data: crank_data(0, 1, 1_000),
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::AccountNotInitialized as u32);
}

#[test]
fn crank_rejects_unwritable_data() {
    // a_data not writable → DataAccountNotWritable.
    let (mut svm, program_id, user) = fresh_env();
    let from_slab = Pubkey::new_unique();
    let to_slab = Pubkey::new_unique();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    setup_portfolio_two_markets(&mut svm, program_id, &user, from_slab, 0, to_slab, 1);

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new_readonly(data_pda, false), // NOT writable
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
            AccountMeta::new(from_slab, false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new(to_slab, false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
        ],
        data: crank_data(0, 1, 1_000),
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::DataAccountNotWritable as u32);
}

#[test]
fn crank_rejects_fake_percolator_program() {
    // Substituted a_percolator_prog → BadProgram.
    let (mut svm, program_id, user) = fresh_env();
    let from_slab = Pubkey::new_unique();
    let to_slab = Pubkey::new_unique();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    setup_portfolio_two_markets(&mut svm, program_id, &user, from_slab, 0, to_slab, 1);

    let fake_prog = Pubkey::new_unique();
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(fake_prog, false), // <-- attack
            AccountMeta::new(from_slab, false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new(to_slab, false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
        ],
        data: crank_data(0, 1, 1_000),
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadProgram as u32);
}

#[test]
fn crank_rejects_wrong_account_count() {
    // Only 14 accounts (need exactly 15) → BadAccountCount.
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
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            // Missing 15th account
        ],
        data: crank_data(0, 1, 1_000),
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadAccountCount as u32);
}

// ── Merge-pending / requires engine state tests ──────────────────────────────
//
// The following tests depend on engine slab data being readable:
//
//   crank_rejects_when_not_needed — needs to_slab with a HEALTHY account
//     (is_above_initial_margin returns true). This requires a real engine
//     slab with properly formatted binary layout matching the small-tier
//     .so. Currently the integration_env InitMarket fails with
//     Custom(4) (the small-tier slab param validation); unblocked when
//     encode_init_market_basic is corrected.    [harness-issue]
//
//   crank_rejects_bounty_vault_underfunded — needs to_slab with BELOW-IM
//     account (CrankNotNeeded doesn't fire) AND portfolio_vault with
//     insufficient balance for bounty. Same harness-issue blocker.
//     [merge-pending: CRIT-2 fix is shipped in main; only the test setup
//      for "needs help" state is blocked]
//
//   crank_rejects_payout_wrong_owner — needs bounty > 0, which requires
//     surviving the "needs help" gate. H-1 fix IS shipped. Blocked on
//     same harness-issue.    [merge-pending: H-1 fix shipped]
//
//   crank_rejects_oracle_decode_failure — needs a valid engine slab so the
//     "needs help" gate runs. Blocked on harness-issue.    [harness-issue]
//
//   crank_rejects_below_mm_destination (M-11) — needs to_slab with an account
//     that is BELOW maintenance margin (already liquidatable). The new MM
//     floor sits *after* the below-IM "needs help" gate, so it needs the same
//     readable below-IM/below-MM engine slab the gate tests need. Expected:
//     Custom(PortfolioError::CrankDestUnrescuable = 41). Blocked on the same
//     harness-issue as crank_rejects_when_not_needed.    [harness-issue]
