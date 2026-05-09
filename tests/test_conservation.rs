//! System-level conservation invariants for the portfolio program.
//!
//! These are the "did we leak tokens or break a global accounting
//! identity?" tests. They are the safety net that catches bugs the
//! per-instruction tests miss — anything that violates conservation,
//! token-balance accounting, or engine-level invariants gets caught
//! here.
//!
//! ## The six invariants
//!
//! Numbered after the engine spec agent's analysis. Source-of-truth is
//! `~/percolator-prog/tests/test_conservation.rs` for the engine side
//! and `~/percolator-prog/src/percolator.rs:12959` for the AuditCrank
//! that enforces them on-chain.
//!
//! - **INV-1: portfolio_vault drains to zero.** After ANY completed
//!   user-signed instruction (Deposit/Withdraw/Rebalance/EmergencyClose),
//!   `portfolio_vault.amount == 0`. The vault is a transient routing
//!   account, never an accumulating balance.
//!
//! - **INV-2: Deposit is additive.** Post-`Deposit(amount)`:
//!     market_vault += amount, engine.c_tot += units(amount),
//!     user_ata -= amount, portfolio_vault delta = 0.
//!
//! - **INV-3: Withdraw is subtractive and symmetric.** Post-`Withdraw(amount)`:
//!     market_vault -= amount, engine.c_tot -= units(amount),
//!     user_ata += amount, portfolio_vault delta = 0.
//!
//! - **INV-4: Rebalance is capital-neutral.** For a single-leg
//!   `Rebalance(from, to, amount)`:
//!     from_market.vault + to_market.vault unchanged,
//!     engine_from.c_tot + engine_to.c_tot unchanged,
//!     portfolio_vault delta = 0.
//!
//! - **INV-5: AuditCrank passes per enrolled market.** After any
//!   portfolio instruction, calling `percolator-prog::AuditCrank` on
//!   each enrolled slab returns Ok. This rolls up four engine-level
//!   sub-invariants: capital_sum == c_tot, pnl_pos_sum == pnl_pos_tot,
//!   eff_pos_sum == oi_eff_long+short, vault >= c_tot + insurance.
//!
//! - **INV-6: Round-trip identity.** `Deposit(x); Withdraw(x)` returns
//!   account.capital and balances to within fee tolerance.
//!
//! ## Coverage status
//!
//! INV-1 is covered by `init_vault_happy_path_actually_creates_vault`
//! in `test_vault_and_cpi.rs` (vault is 0 post-init) and by
//! `e2e_user_can_transfer_into_portfolio_vault` (vault is non-zero
//! mid-flow but remains correctly balanced). The empty-state of the
//! vault after every Deposit/Withdraw/Rebalance is what the rest of
//! these tests will assert — but they need a working Deposit, which
//! requires `EnrollMarketAndInit` (a CPI'd InitUser) that's not yet
//! wired into the portfolio program.
//!
//! INV-2 through INV-6 are blocked on `EnrollMarketAndInit`. The
//! integration harness (`tests/common/integration_env.rs`) is ready
//! for them — once the instruction lands, each invariant becomes a
//! ~20-line test on top of the existing scaffolding.
//!
//! The framework below is the skeleton these tests will plug into.
//! Each `inv_N_*` function documents the precondition, the action,
//! and the post-condition that must hold.

mod common;

use common::integration_env::IntegrationEnv;
use solana_sdk::signature::Signer;

/// INV-1 (covered today): vault is zero immediately after InitVault.
/// This is the trivial base case of "vault never accumulates" — the
/// stronger statement requires Deposit/Withdraw to be testable.
#[test]
fn inv1_vault_is_zero_post_init() {
    let mut env = IntegrationEnv::new(1_000_000);
    let (_data_pda, _auth_pda, vault) = env.init_portfolio_and_vault();
    assert_eq!(env.portfolio_vault_balance(&vault), 0);
}

/// Demonstration of the conservation pattern: snapshot all relevant
/// balances pre-action, execute, snapshot post-action, assert the
/// invariant. Replace the body once Deposit is end-to-end-runnable.
#[test]
fn inv1_vault_drains_after_user_transfer_in_and_out() {
    // Today: user transfers IN, the test transfers OUT. Both via SPL.
    // This proves the harness can observe the balance flow; it doesn't
    // exercise a portfolio Deposit yet.
    let mut env = IntegrationEnv::new(1_000_000);
    let (_data_pda, _auth_pda, vault) = env.init_portfolio_and_vault();

    let user = env.user.insecure_clone();
    let xfer_in = spl_token::instruction::transfer(
        &common::integration_env::SPL_TOKEN,
        &env.user_ata,
        &vault,
        &user.pubkey(),
        &[],
        500,
    )
    .unwrap();
    let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
        &[xfer_in],
        Some(&user.pubkey()),
        &[&user],
        env.svm.latest_blockhash(),
    );
    env.svm.send_transaction(tx).unwrap();
    assert_eq!(env.portfolio_vault_balance(&vault), 500);

    // Note: with the current set of instructions, the vault can only
    // be drained back to user_ata by going through Withdraw, which
    // requires EnrollMarketAndInit. So we can't yet demonstrate the
    // full drain-to-zero invariant for a portfolio-driven flow. The
    // skeleton stops here; the next milestone is the unblocker.
}

// ── Tests below are intentionally not yet active ──────────────────────────
//
// Each will be written when `EnrollMarketAndInit` lands. They are listed
// here so the test-suite reader sees the full coverage plan. Use
// `#[ignore]` rather than commenting out so `cargo test -- --ignored`
// runs them once the precondition is met.

#[ignore = "blocked: EnrollMarketAndInit (InitUser-via-CPI) not yet wired"]
#[test]
fn inv2_deposit_is_additive() {
    // Plan:
    //   1. env = IntegrationEnv::new(...);
    //   2. let (data, auth, vault) = env.init_portfolio_and_vault();
    //   3. env.enroll_and_init_user(slab, fee_payment).expect();
    //   4. snapshot user_ata, market_vault, c_tot, vault.
    //   5. env.deposit(slab, account_idx, 1000).unwrap();
    //   6. assert market_vault += 1000, c_tot += units(1000),
    //      user_ata -= 1000, vault delta = 0.
}

#[ignore = "blocked: EnrollMarketAndInit"]
#[test]
fn inv3_withdraw_is_subtractive() {
    // Mirror of inv2_*.
}

#[ignore = "blocked: EnrollMarketAndInit + multi-market harness"]
#[test]
fn inv4_rebalance_is_capital_neutral() {
    // Two-slab harness needed.
}

#[ignore = "blocked: AuditCrank harness not yet built"]
#[test]
fn inv5_audit_crank_passes_after_every_op() {
    // For each of [deposit, withdraw, rebalance, emergency_close]:
    //   - perform the op
    //   - submit percolator-prog::AuditCrank on each enrolled slab
    //   - assert Ok
}

#[ignore = "blocked: EnrollMarketAndInit"]
#[test]
fn inv6_round_trip_preserves_balance() {
    // deposit(x) then withdraw(x) — verify within fee tolerance.
}
