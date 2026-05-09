//! End-to-end integration tests against a real percolator-prog slab.
//!
//! These tests load BOTH .so files into LiteSVM and exercise actual
//! token movement and slab state. They are the load-bearing proof that
//! our wrapper's CPIs into percolator-prog work end-to-end.
//!
//! ## What's currently covered
//!
//! - `IntegrationEnv::new`: percolator-prog .so loaded at canonical ID,
//!   `InitMarket` succeeds, mint + vault + Pyth oracle wired correctly.
//! - `init_portfolio_and_vault`: verified end-to-end against a real mint.
//! - User can transfer USDC into the portfolio_vault directly.
//!
//! ## What's NOT covered yet
//!
//! - Portfolio Deposit / Withdraw / Rebalance / EmergencyClose round-
//!   trips. These all require the per-market account in percolator-prog
//!   to have `engine.account.owner == portfolio_auth`, which only the
//!   portfolio program can establish via `invoke_signed` on
//!   `percolator-prog::InitUser`. The current `EnrollMarket` instruction
//!   is state-only — no CPI. A new `EnrollMarketAndInit` instruction
//!   that bundles InitUser-via-CPI is the unblocker; it's the next item
//!   in the roadmap.

mod common;

use common::integration_env::{IntegrationEnv, PERCOLATOR_PROG, SLAB_LEN, SPL_TOKEN, TOKEN_ACCOUNT_LEN};
use solana_sdk::{
    program_pack::Pack,
    signature::Signer,
};

#[test]
fn integration_env_loads_both_programs() {
    let env = IntegrationEnv::new(1_000_000);

    assert_eq!(env.user_ata_balance(), 1_000_000, "user funded");
    assert_eq!(env.market_vault_balance(), 0, "vault empty post-InitMarket");

    let slab_acct = env.svm.get_account(&env.slab).unwrap();
    assert_eq!(slab_acct.owner, PERCOLATOR_PROG);
    assert_eq!(slab_acct.data.len(), SLAB_LEN);
    let magic = u64::from_le_bytes([
        slab_acct.data[0], slab_acct.data[1], slab_acct.data[2], slab_acct.data[3],
        slab_acct.data[4], slab_acct.data[5], slab_acct.data[6], slab_acct.data[7],
    ]);
    assert_eq!(magic, 0x504552434f4c4154, "slab has 'PERCOLAT' magic");
}

#[test]
fn e2e_init_portfolio_and_vault_succeeds() {
    let mut env = IntegrationEnv::new(1_000_000);
    let (data_pda, auth_pda, vault) = env.init_portfolio_and_vault();

    let pa = env.read_portfolio(&data_pda);
    assert_ne!(pa.bump, 0);
    assert_ne!(pa.auth_bump, 0);
    assert_ne!(pa.vault_bump, 0, "vault was actually created");

    let vault_acct = env.svm.get_account(&vault).unwrap();
    assert_eq!(vault_acct.owner, SPL_TOKEN);
    assert_eq!(vault_acct.data.len(), TOKEN_ACCOUNT_LEN);
    assert_eq!(env.portfolio_vault_balance(&vault), 0);

    let acc = spl_token::state::Account::unpack(&vault_acct.data).unwrap();
    assert_eq!(acc.mint, env.mint);
    assert_eq!(acc.owner, auth_pda);
}

#[test]
fn e2e_user_can_transfer_into_portfolio_vault() {
    let mut env = IntegrationEnv::new(1_000_000);
    let (_data_pda, _auth_pda, vault) = env.init_portfolio_and_vault();

    let user = env.user.insecure_clone();
    let xfer = spl_token::instruction::transfer(
        &SPL_TOKEN,
        &env.user_ata,
        &vault,
        &user.pubkey(),
        &[],
        500,
    )
    .unwrap();
    let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
        &[xfer],
        Some(&user.pubkey()),
        &[&user],
        env.svm.latest_blockhash(),
    );
    env.svm.send_transaction(tx).expect("user transfer to vault");

    assert_eq!(env.user_ata_balance(), 1_000_000 - 500);
    assert_eq!(env.portfolio_vault_balance(&vault), 500);
}
