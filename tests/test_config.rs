//! Integration tests for `UpdateConfig` and `SetPaused`.

mod common;

use bytemuck::from_bytes;
use common::{assert_custom_error, fresh_env, pdas_for, send_init, send_signed};
use percolator_portfolio::{errors::PortfolioError, state::PortfolioAccount};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};

fn read_portfolio(svm: &litesvm::LiteSVM, data_pda: &Pubkey) -> PortfolioAccount {
    let acct = svm.get_account(data_pda).unwrap();
    *from_bytes::<PortfolioAccount>(
        &acct.data[..core::mem::size_of::<PortfolioAccount>()],
    )
}

fn update_config_ix(
    program_id: Pubkey,
    user: &Keypair,
    data_pda: Pubkey,
    buffer: u16,
    max_lev: u32,
    keeper: Pubkey,
) -> Instruction {
    let mut data = vec![8u8];
    data.extend_from_slice(&buffer.to_le_bytes());
    data.extend_from_slice(&max_lev.to_le_bytes());
    data.extend_from_slice(keeper.as_ref());
    Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
        ],
        data,
    }
}

fn set_paused_ix(
    program_id: Pubkey,
    user: &Keypair,
    data_pda: Pubkey,
    paused: bool,
) -> Instruction {
    Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
        ],
        data: vec![9u8, u8::from(paused)],
    }
}

#[test]
fn update_config_happy_path() {
    let (mut svm, program_id, user) = fresh_env();
    let init_keeper = Pubkey::new_unique();
    send_init(&mut svm, program_id, &user, 200, 50_000, init_keeper).unwrap();

    let new_keeper = Pubkey::new_unique();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);
    let ix = update_config_ix(program_id, &user, data_pda, 500, 30_000, new_keeper);
    send_signed(&mut svm, ix, &user).expect("update_config");

    let pa = read_portfolio(&svm, &data_pda);
    assert_eq!(pa.buffer_bps, 500, "buffer mutated");
    assert_eq!(pa.max_leverage_bps, 30_000, "leverage mutated");
    assert_eq!(pa.keeper, new_keeper.to_bytes(), "keeper mutated");
    // Verify other fields untouched.
    assert_eq!(pa.owner, user.pubkey().to_bytes(), "owner unchanged");
    assert_eq!(pa.paused, 0, "paused unchanged");
    assert_eq!(pa.enrolled_count, 0, "enrolled unchanged");
}

#[test]
fn update_config_rejects_invalid_buffer() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();

    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);
    let ix = update_config_ix(program_id, &user, data_pda, 0, 30_000, Pubkey::new_unique());
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BufferOutOfRange as u32);

    // State unchanged.
    let pa = read_portfolio(&svm, &data_pda);
    assert_eq!(pa.buffer_bps, 200);
}

#[test]
fn update_config_rejects_invalid_leverage() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();

    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);
    let ix = update_config_ix(program_id, &user, data_pda, 200, 200_000, Pubkey::new_unique());
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::LeverageOutOfRange as u32);

    // State preserved.
    let pa = read_portfolio(&svm, &data_pda);
    assert_eq!(pa.max_leverage_bps, 50_000, "leverage unchanged after rejection");
    assert_eq!(pa.buffer_bps, 200, "buffer unchanged after rejection");
}

#[test]
fn update_config_rejects_wrong_signer() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();

    let attacker = Keypair::new();
    svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();

    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);
    let ix = update_config_ix(program_id, &attacker, data_pda, 500, 30_000, Pubkey::new_unique());
    let res = send_signed(&mut svm, ix, &attacker);
    // BadOwner: a_user.is_signer ✓, a_data.owner == program_id ✓, magic ✓,
    // version ✓, then pa.owner != a_user.key (attacker) → BadOwner.
    assert_custom_error(res, PortfolioError::BadOwner as u32);

    // State preserved.
    let pa = read_portfolio(&svm, &data_pda);
    assert_eq!(pa.buffer_bps, 200, "buffer not modified by attacker");
}

#[test]
fn set_paused_toggles() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    assert_eq!(read_portfolio(&svm, &data_pda).paused, 0);

    send_signed(&mut svm, set_paused_ix(program_id, &user, data_pda, true), &user).unwrap();
    assert_eq!(read_portfolio(&svm, &data_pda).paused, 1, "paused after toggle");

    svm.expire_blockhash();
    send_signed(&mut svm, set_paused_ix(program_id, &user, data_pda, false), &user).unwrap();
    assert_eq!(read_portfolio(&svm, &data_pda).paused, 0, "active after toggle");
}

#[test]
fn set_paused_rejects_invalid_byte() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    for byte in [2u8, 0xff, 100u8] {
        svm.expire_blockhash();
        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(user.pubkey(), true),
                AccountMeta::new(data_pda, false),
            ],
            data: vec![9u8, byte],
        };
        let res = send_signed(&mut svm, ix, &user);
        assert_custom_error(res, PortfolioError::BadInstruction as u32);
    }
    // State preserved.
    assert_eq!(read_portfolio(&svm, &data_pda).paused, 0);
}

#[test]
fn update_config_preserves_enrolled_state() {
    // After UpdateConfig, the enrolled[] array and enrolled_count must be
    // bit-identical to pre-state.
    //
    // This test explicitly enrols THREE markets first so we're verifying
    // a non-trivial preservation property — an earlier version of this
    // test compared all-zero enrolled[] before vs after, which would pass
    // even if UpdateConfig zeroed the array.
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    // Enrol 3 markets so enrolled[] has real bytes to preserve.
    let m1 = Pubkey::new_unique();
    let m2 = Pubkey::new_unique();
    let m3 = Pubkey::new_unique();
    for (i, m) in [m1, m2, m3].iter().enumerate() {
        svm.expire_blockhash();
        let mut data = vec![1u8]; // EnrollMarket
        data.extend_from_slice(&(i as u16 + 7).to_le_bytes()); // distinctive idx
        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(user.pubkey(), true),
                AccountMeta::new(data_pda, false),
                AccountMeta::new_readonly(*m, false),
            ],
            data,
        };
        send_signed(&mut svm, ix, &user).unwrap();
    }

    let pre = read_portfolio(&svm, &data_pda);
    assert_eq!(pre.enrolled_count, 3, "precondition: 3 markets enrolled");
    assert_eq!(pre.enrolled[0].market, m1.to_bytes());
    assert_eq!(pre.enrolled[1].market, m2.to_bytes());
    assert_eq!(pre.enrolled[2].market, m3.to_bytes());
    assert_eq!(pre.enrolled[0].account_idx, 7);
    assert_eq!(pre.enrolled[1].account_idx, 8);
    assert_eq!(pre.enrolled[2].account_idx, 9);

    svm.expire_blockhash();
    let ix = update_config_ix(program_id, &user, data_pda, 500, 30_000, Pubkey::new_unique());
    send_signed(&mut svm, ix, &user).unwrap();

    let post = read_portfolio(&svm, &data_pda);
    // Config fields actually changed.
    assert_eq!(post.buffer_bps, 500, "buffer DID change");
    assert_eq!(post.max_leverage_bps, 30_000, "leverage DID change");
    // Enrolled array did NOT change.
    assert_eq!(post.enrolled_count, 3, "count preserved");
    assert_eq!(post.enrolled[0].market, m1.to_bytes(), "m1 preserved");
    assert_eq!(post.enrolled[1].market, m2.to_bytes(), "m2 preserved");
    assert_eq!(post.enrolled[2].market, m3.to_bytes(), "m3 preserved");
    assert_eq!(post.enrolled[0].account_idx, 7);
    assert_eq!(post.enrolled[1].account_idx, 8);
    assert_eq!(post.enrolled[2].account_idx, 9);
}
