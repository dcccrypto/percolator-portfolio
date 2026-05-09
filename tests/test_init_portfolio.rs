//! Integration tests for `InitPortfolio`.
//!
//! Every rejection test asserts the EXACT `PortfolioError` discriminant —
//! `assert!(!err.is_empty())` is banned per the test-rigor convention in
//! `tests/common/mod.rs`.

mod common;

use bytemuck::from_bytes;
use common::{assert_any_error, assert_custom_error, fresh_env, pdas_for, send_init, send_signed};
use percolator_portfolio::{
    constants::{MAGIC, VERSION},
    errors::PortfolioError,
    state::PortfolioAccount,
};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_program,
};

#[test]
fn init_portfolio_happy_path() {
    let (mut svm, program_id, user) = fresh_env();
    let keeper = Keypair::new().pubkey();

    send_init(&mut svm, program_id, &user, 200, 50_000, keeper).expect("init succeeds");

    let (data_pda, data_bump, _, auth_bump) = pdas_for(&user.pubkey(), &program_id);
    let acct = svm.get_account(&data_pda).expect("data PDA must exist");
    assert_eq!(acct.owner, program_id);

    let pa: &PortfolioAccount =
        from_bytes(&acct.data[..core::mem::size_of::<PortfolioAccount>()]);
    assert_eq!(pa.magic, MAGIC);
    assert_eq!(pa.owner, user.pubkey().to_bytes());
    assert_eq!(pa.bump, data_bump);
    assert_eq!(pa.auth_bump, auth_bump);
    assert_eq!(pa.version, VERSION);
    assert_eq!(pa.paused, 0);
    assert_eq!(pa.buffer_bps, 200);
    assert_eq!(pa.max_leverage_bps, 50_000);
    assert_eq!(pa.keeper, keeper.to_bytes());
    assert_eq!(pa.enrolled_count, 0);
    assert_eq!(pa.last_rebalance_slot, 0);
    assert_eq!(pa.cached_at_slot, 0);
    assert_eq!(pa.cached_total_eq_e6, 0);
    assert_eq!(pa.cached_total_mmr_e6, 0);
    for slot in &pa.enrolled {
        assert_eq!(slot.market, [0u8; 32]);
        assert_eq!(slot.account_idx, 0);
        assert_eq!(slot.last_seen_eq_e6, 0);
    }
}

#[test]
fn init_rejects_double_init() {
    // The second init must use a *different* tx (different keeper bytes
    // are enough) — otherwise Solana's tx-level dedup returns
    // `AlreadyProcessed` before our handler runs, masking the real check.
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique())
        .expect("first init");
    // Advance the blockhash so the second tx isn't a hash collision.
    svm.expire_blockhash();
    let res = send_init(&mut svm, program_id, &user, 300, 60_000, Pubkey::new_unique());
    assert_custom_error(res, PortfolioError::AccountAlreadyInitialized as u32);
}

#[test]
fn init_rejects_buffer_too_low() {
    let (mut svm, program_id, user) = fresh_env();
    let res = send_init(&mut svm, program_id, &user, 50, 50_000, Pubkey::new_unique());
    assert_custom_error(res, PortfolioError::BufferOutOfRange as u32);
}

#[test]
fn init_rejects_buffer_at_min_minus_one() {
    // Boundary: MIN_BUFFER_BPS - 1 = 99 must be rejected.
    let (mut svm, program_id, user) = fresh_env();
    let res = send_init(&mut svm, program_id, &user, 99, 50_000, Pubkey::new_unique());
    assert_custom_error(res, PortfolioError::BufferOutOfRange as u32);
}

#[test]
fn init_accepts_buffer_at_min() {
    // Boundary: MIN_BUFFER_BPS = 100 must succeed AND the value must be
    // stored. (Earlier version of this test only checked that init
    // didn't error — that wouldn't catch a bug where init silently
    // clamped the value.)
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 100, 50_000, Pubkey::new_unique())
        .expect("buffer=MIN must succeed");

    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);
    let acct = svm.get_account(&data_pda).unwrap();
    let pa: &PortfolioAccount =
        from_bytes(&acct.data[..core::mem::size_of::<PortfolioAccount>()]);
    assert_eq!(pa.buffer_bps, 100, "boundary value actually stored");
}

#[test]
fn init_accepts_buffer_at_max() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 5_000, 50_000, Pubkey::new_unique())
        .expect("buffer=MAX must succeed");

    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);
    let acct = svm.get_account(&data_pda).unwrap();
    let pa: &PortfolioAccount =
        from_bytes(&acct.data[..core::mem::size_of::<PortfolioAccount>()]);
    assert_eq!(pa.buffer_bps, 5_000, "boundary value actually stored");
}

#[test]
fn init_rejects_buffer_too_high() {
    let (mut svm, program_id, user) = fresh_env();
    let res = send_init(&mut svm, program_id, &user, 6_000, 50_000, Pubkey::new_unique());
    assert_custom_error(res, PortfolioError::BufferOutOfRange as u32);
}

#[test]
fn init_rejects_zero_leverage() {
    let (mut svm, program_id, user) = fresh_env();
    let res = send_init(&mut svm, program_id, &user, 200, 0, Pubkey::new_unique());
    assert_custom_error(res, PortfolioError::LeverageOutOfRange as u32);
}

#[test]
fn init_rejects_leverage_above_ceiling() {
    let (mut svm, program_id, user) = fresh_env();
    let res = send_init(&mut svm, program_id, &user, 200, 200_000, Pubkey::new_unique());
    assert_custom_error(res, PortfolioError::LeverageOutOfRange as u32);
}

#[test]
fn init_accepts_leverage_at_ceiling() {
    // Boundary: MAX_PORTFOLIO_LEV_BPS = 100_000 must succeed AND the
    // value must be stored.
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 100_000, Pubkey::new_unique())
        .expect("max_lev=ceiling must succeed");

    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);
    let acct = svm.get_account(&data_pda).unwrap();
    let pa: &PortfolioAccount =
        from_bytes(&acct.data[..core::mem::size_of::<PortfolioAccount>()]);
    assert_eq!(pa.max_leverage_bps, 100_000, "boundary value actually stored");
}

#[test]
fn init_rejects_wrong_system_program() {
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);

    let mut data = vec![0u8];
    data.extend_from_slice(&200u16.to_le_bytes());
    data.extend_from_slice(&50_000u32.to_le_bytes());
    data.extend_from_slice(Pubkey::new_unique().as_ref());

    let bogus_program = Pubkey::new_unique();
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new_readonly(bogus_program, false),
        ],
        data,
    };
    let res = send_signed(&mut svm, ix, &user);
    // v0.2: dedicated `WrongSystemProgram` variant; previously this was
    // overloaded onto `BadAccountCount`.
    assert_custom_error(res, PortfolioError::WrongSystemProgram as u32);
}

#[test]
fn init_rejects_truncated_instruction_data() {
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: vec![0u8, 1, 2, 3, 4, 5], // tag 0 + 5 bytes < 38
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadInstruction as u32);
}

#[test]
fn init_rejects_empty_instruction_data() {
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: vec![],
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadInstruction as u32);
}

#[test]
fn init_rejects_unknown_tag() {
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: vec![42u8],
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadInstruction as u32);
}

#[test]
fn init_rejects_oversized_instruction_data() {
    // Tag 0 with valid 38-byte body + extra trailing byte → strict-length
    // check rejects.
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);

    let mut data = vec![0u8];
    data.extend_from_slice(&200u16.to_le_bytes());
    data.extend_from_slice(&50_000u32.to_le_bytes());
    data.extend_from_slice(Pubkey::new_unique().as_ref());
    data.push(0xff);

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
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadInstruction as u32);
}

#[test]
fn init_rejects_too_few_accounts() {
    // accounts.len() != 4 — pass only 3.
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
            // missing system program
        ],
        data,
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadAccountCount as u32);
}

// Note: a "user passed as is_signer=false" test was intentionally removed.
// Solana's runtime normalises is_signer across all references to the same
// pubkey: when the fee payer signs the transaction, every AccountMeta
// referencing that pubkey gets is_signer=true regardless of how it was
// declared. So the `WrongSigner` gate inside our handler is unreachable
// from a fee-paying caller — and a test that doesn't reach the code path
// it claims to test is exactly the kind of vacuous assertion this commit
// is meant to remove. The check stays in the program as defence-in-depth
// for future call paths (e.g., a multi-instruction tx where slot-0 is a
// secondary account, not the fee payer); when such a path exists, a
// targeted test can be written.
