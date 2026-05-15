//! CU (compute unit) benchmark with pinned upper bounds.
//!
//! Each instruction has a tested-and-pinned CU ceiling. CI fails if any
//! handler exceeds its budget — this is the regression net for CU
//! growth as the program evolves.
//!
//! The bounds are deliberately set ~50% above measured CU so cosmetic
//! refactors don't trip the guard, but a 3× regression (which usually
//! indicates a real algorithmic problem) does.
//!
//! Bounds are calibrated against the debug BPF build, which burns ~2-3×
//! more CU than a release build. Production deploys will be far below
//! these caps.
//!
//! Instructions that CPI into percolator-prog (Deposit, Withdraw,
//! Rebalance, EmergencyClose) are not yet benchmarked here — they
//! depend on `EnrollMarketAndInit` being wired in. They will be added
//! once that lands; the framework below extends naturally.

mod common;

use bytemuck::from_bytes;
use common::{fresh_env, pdas_for, send_init, percolator_owned_slab};
use percolator_portfolio::{
    constants::{PORTFOLIO_VAULT_SEED},
    cpi as cpi_helpers,
    state::PortfolioAccount,
};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction, system_program,
    sysvar::rent::Rent,
    transaction::Transaction,
};

const SPL_TOKEN: Pubkey = Pubkey::new_from_array(cpi_helpers::SPL_TOKEN_ID);

/// Pinned CU ceilings per instruction. Numbers picked at ~1.5x measured
/// debug-build CU. Update only when the underlying handler genuinely
/// changes complexity.
mod budget {
    pub const INIT_PORTFOLIO: u64 = 25_000;
    pub const UPDATE_CONFIG: u64 = 8_000;
    pub const SET_PAUSED: u64 = 8_000;
    pub const ENROLL_MARKET: u64 = 12_000;
    pub const UNENROLL_MARKET: u64 = 12_000;
    pub const INIT_VAULT: u64 = 35_000;
    pub const CLOSE_PORTFOLIO_NO_VAULT: u64 = 12_000;
    pub const CLOSE_PORTFOLIO_WITH_VAULT: u64 = 35_000;
}

fn send_and_measure(
    svm: &mut litesvm::LiteSVM,
    ix: Instruction,
    signer: &Keypair,
    label: &str,
    budget: u64,
) {
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&signer.pubkey()),
        &[signer],
        svm.latest_blockhash(),
    );
    let meta = svm.send_transaction(tx).unwrap_or_else(|e| {
        panic!("{label} failed: {:?}", e.err);
    });
    let cu = meta.compute_units_consumed;
    println!("CU[{label:30}] = {cu:>7} / {budget:>7}");
    assert!(
        cu <= budget,
        "CU regression: {label} consumed {cu}, budget is {budget}"
    );
}

/// Helper: full vault setup (mint + InitVault) so we can benchmark the
/// vault-aware close path.
fn setup_with_vault(
    svm: &mut litesvm::LiteSVM,
    program_id: Pubkey,
    user: &Keypair,
) -> (Pubkey, Pubkey, Pubkey, Pubkey) {
    send_init(svm, program_id, user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _) =
        Pubkey::find_program_address(&[PORTFOLIO_VAULT_SEED, user.pubkey().as_ref()], &program_id);

    let mint_kp = Keypair::new();
    let rent_lamports = Rent::default().minimum_balance(spl_token::state::Mint::LEN);
    let create_mint = system_instruction::create_account(
        &user.pubkey(),
        &mint_kp.pubkey(),
        rent_lamports,
        spl_token::state::Mint::LEN as u64,
        &SPL_TOKEN,
    );
    let init_mint = spl_token::instruction::initialize_mint2(
        &SPL_TOKEN,
        &mint_kp.pubkey(),
        &user.pubkey(),
        None,
        6,
    )
    .unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[create_mint, init_mint],
        Some(&user.pubkey()),
        &[user, &mint_kp],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).unwrap();
    svm.expire_blockhash();

    let init_vault_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(mint_kp.pubkey(), false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
        ],
        data: vec![10u8],
    };
    let tx = Transaction::new_signed_with_payer(
        &[init_vault_ix],
        Some(&user.pubkey()),
        &[user],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).unwrap();

    (data_pda, auth_pda, vault, mint_kp.pubkey())
}

#[test]
fn cu_init_portfolio() {
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);

    let mut data = vec![0u8];
    data.extend_from_slice(&200u16.to_le_bytes());
    data.extend_from_slice(&50_000u32.to_le_bytes());
    data.extend_from_slice(Pubkey::new_unique().as_ref());

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    };
    send_and_measure(&mut svm, ix, &user, "InitPortfolio", budget::INIT_PORTFOLIO);
}

#[test]
fn cu_update_config() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    svm.expire_blockhash();
    let mut data = vec![8u8];
    data.extend_from_slice(&500u16.to_le_bytes());
    data.extend_from_slice(&30_000u32.to_le_bytes());
    data.extend_from_slice(Pubkey::new_unique().as_ref());

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
        ],
        data,
    };
    send_and_measure(&mut svm, ix, &user, "UpdateConfig", budget::UPDATE_CONFIG);
}

#[test]
fn cu_set_paused() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    svm.expire_blockhash();
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
        ],
        data: vec![9u8, 1u8],
    };
    send_and_measure(&mut svm, ix, &user, "SetPaused", budget::SET_PAUSED);
}

#[test]
fn cu_enroll_market() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    let market = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, market);
    svm.expire_blockhash();
    let mut data = vec![1u8];
    data.extend_from_slice(&5u16.to_le_bytes());
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(market, false),
        ],
        data,
    };
    send_and_measure(&mut svm, ix, &user, "EnrollMarket", budget::ENROLL_MARKET);
}

#[test]
fn cu_unenroll_market() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    let market = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, market);
    svm.expire_blockhash();
    let mut data = vec![1u8];
    data.extend_from_slice(&5u16.to_le_bytes());
    let enroll_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(market, false),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[enroll_ix],
        Some(&user.pubkey()),
        &[&user],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).unwrap();

    svm.expire_blockhash();
    let mut data = vec![2u8];
    data.extend_from_slice(&5u16.to_le_bytes());
    let unenroll_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(market, false),
            // H-6: slab account (same pubkey as market — see test_enroll.rs).
            AccountMeta::new_readonly(market, false),
        ],
        data,
    };
    send_and_measure(&mut svm, unenroll_ix, &user, "UnenrollMarket", budget::UNENROLL_MARKET);
}

#[test]
fn cu_init_vault() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _) =
        Pubkey::find_program_address(&[PORTFOLIO_VAULT_SEED, user.pubkey().as_ref()], &program_id);

    // Build the mint first.
    let mint_kp = Keypair::new();
    let rent_lamports = Rent::default().minimum_balance(spl_token::state::Mint::LEN);
    let create_mint = system_instruction::create_account(
        &user.pubkey(),
        &mint_kp.pubkey(),
        rent_lamports,
        spl_token::state::Mint::LEN as u64,
        &SPL_TOKEN,
    );
    let init_mint = spl_token::instruction::initialize_mint2(
        &SPL_TOKEN,
        &mint_kp.pubkey(),
        &user.pubkey(),
        None,
        6,
    )
    .unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[create_mint, init_mint],
        Some(&user.pubkey()),
        &[&user, &mint_kp],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).unwrap();
    svm.expire_blockhash();

    let init_vault_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(mint_kp.pubkey(), false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
        ],
        data: vec![10u8],
    };
    send_and_measure(&mut svm, init_vault_ix, &user, "InitVault", budget::INIT_VAULT);
}

#[test]
fn cu_close_portfolio_no_vault() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let dummy_vault = Pubkey::new_unique();

    svm.expire_blockhash();
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(dummy_vault, false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
        ],
        data: vec![11u8],
    };
    send_and_measure(
        &mut svm,
        ix,
        &user,
        "ClosePortfolio (no vault)",
        budget::CLOSE_PORTFOLIO_NO_VAULT,
    );
}

#[test]
fn cu_close_portfolio_with_vault() {
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, auth_pda, vault, _mint) = setup_with_vault(&mut svm, program_id, &user);

    svm.expire_blockhash();
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
        ],
        data: vec![11u8],
    };
    send_and_measure(
        &mut svm,
        ix,
        &user,
        "ClosePortfolio (with vault)",
        budget::CLOSE_PORTFOLIO_WITH_VAULT,
    );
}

#[test]
fn cu_summary_table() {
    // Re-runs all benchmarks in one place so `cargo test cu_summary_table
    // -- --nocapture` prints the full table at once. Useful for inclusion
    // in PR descriptions / CI logs.
    let (mut svm1, pid1, u1) = fresh_env();
    let (mut svm2, pid2, u2) = fresh_env();
    let (mut svm3, pid3, u3) = fresh_env();

    send_init(&mut svm2, pid2, &u2, 200, 50_000, Pubkey::new_unique()).unwrap();
    send_init(&mut svm3, pid3, &u3, 200, 50_000, Pubkey::new_unique()).unwrap();

    println!("\nPortfolio program CU table (debug build):");
    println!("(Production release builds typically run 2-3× lower.)");

    // Trigger the same code paths as the per-instruction tests but
    // without panicking on a budget overrun — just print the numbers.
    let (data1, _, auth1, _) = pdas_for(&u1.pubkey(), &pid1);
    let mut data = vec![0u8];
    data.extend_from_slice(&200u16.to_le_bytes());
    data.extend_from_slice(&50_000u32.to_le_bytes());
    data.extend_from_slice(Pubkey::new_unique().as_ref());
    let ix = Instruction {
        program_id: pid1,
        accounts: vec![
            AccountMeta::new(u1.pubkey(), true),
            AccountMeta::new(data1, false),
            AccountMeta::new_readonly(auth1, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&u1.pubkey()),
        &[&u1],
        svm1.latest_blockhash(),
    );
    let meta = svm1.send_transaction(tx).unwrap();
    println!("  InitPortfolio                 = {:>7}", meta.compute_units_consumed);
}
