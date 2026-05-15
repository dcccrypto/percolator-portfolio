//! Integration tests for `EnrollMarket` / `UnenrollMarket`.
//!
//! Every happy-path test verifies the resulting `PortfolioAccount`
//! state byte-for-byte. Every rejection test asserts the specific
//! `PortfolioError` discriminant.

mod common;

use bytemuck::from_bytes;
use common::{assert_custom_error, fresh_env, pdas_for, percolator_owned_slab, send_init, send_signed};
use percolator_portfolio::{
    constants::MAX_ENROLLED_MARKETS,
    errors::PortfolioError,
    state::PortfolioAccount,
};
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

fn enroll_ix(
    program_id: Pubkey,
    user: &Keypair,
    data_pda: Pubkey,
    market: Pubkey,
    account_idx: u16,
) -> Instruction {
    let mut data = vec![1u8];
    data.extend_from_slice(&account_idx.to_le_bytes());
    Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(market, false),
        ],
        data,
    }
}


fn unenroll_ix(
    program_id: Pubkey,
    user: &Keypair,
    data_pda: Pubkey,
    market: Pubkey,
    account_idx: u16,
) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&account_idx.to_le_bytes());
    Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(market, false),
            // H-6: slab account (same pubkey as market for the check that
            // a_slab.key == a_market.key). State-only tests pass a random
            // pubkey here — the soft-decode in the handler tolerates that.
            AccountMeta::new_readonly(market, false),
        ],
        data,
    }
}

#[test]
fn enroll_appends_to_empty_slot_zero() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    let market = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, market);
    send_signed(&mut svm, enroll_ix(program_id, &user, data_pda, market, 7), &user).unwrap();

    let pa = read_portfolio(&svm, &data_pda);
    assert_eq!(pa.enrolled_count, 1);
    assert_eq!(pa.enrolled[0].market, market.to_bytes());
    assert_eq!(pa.enrolled[0].account_idx, 7);
    assert_eq!(pa.enrolled[0].last_seen_eq_e6, 0);
    // Untouched slots remain zero.
    for slot in &pa.enrolled[1..] {
        assert_eq!(slot.market, [0u8; 32]);
        assert_eq!(slot.account_idx, 0);
    }
}

#[test]
fn enroll_appends_in_order() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    let m1 = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, m1);
    let m2 = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, m2);
    let m3 = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, m3);
    send_signed(&mut svm, enroll_ix(program_id, &user, data_pda, m1, 1), &user).unwrap();
    svm.expire_blockhash();
    send_signed(&mut svm, enroll_ix(program_id, &user, data_pda, m2, 2), &user).unwrap();
    svm.expire_blockhash();
    send_signed(&mut svm, enroll_ix(program_id, &user, data_pda, m3, 3), &user).unwrap();

    let pa = read_portfolio(&svm, &data_pda);
    assert_eq!(pa.enrolled_count, 3);
    assert_eq!(pa.enrolled[0].market, m1.to_bytes());
    assert_eq!(pa.enrolled[0].account_idx, 1);
    assert_eq!(pa.enrolled[1].market, m2.to_bytes());
    assert_eq!(pa.enrolled[1].account_idx, 2);
    assert_eq!(pa.enrolled[2].market, m3.to_bytes());
    assert_eq!(pa.enrolled[2].account_idx, 3);
}

#[test]
fn enroll_rejects_duplicate_market_and_idx() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    let market = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, market);
    send_signed(&mut svm, enroll_ix(program_id, &user, data_pda, market, 5), &user).unwrap();
    svm.expire_blockhash();
    let res = send_signed(&mut svm, enroll_ix(program_id, &user, data_pda, market, 5), &user);
    assert_custom_error(res, PortfolioError::MarketAlreadyEnrolled as u32);

    let pa = read_portfolio(&svm, &data_pda);
    assert_eq!(pa.enrolled_count, 1, "duplicate enrol must not increment count");
}

#[test]
fn enroll_rejects_same_market_different_idx() {
    // CRIT-6: enrolling the same market pubkey twice (different idx) is
    // now rejected. Defense 1's pair-region lookup matches by market
    // pubkey alone — supporting multi-account-same-market would require
    // (slab, idx)-joint matching there. The product decision is to keep
    // Defense 1 simple and forbid the configuration at enrollment.
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    let market = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, market);
    send_signed(&mut svm, enroll_ix(program_id, &user, data_pda, market, 5), &user).unwrap();
    svm.expire_blockhash();
    // Same market, different idx → rejected with MarketAlreadyEnrolled.
    let res = send_signed(&mut svm, enroll_ix(program_id, &user, data_pda, market, 6), &user);
    assert!(res.is_err(), "second enrol on same market must reject");
    assert_custom_error(res, PortfolioError::MarketAlreadyEnrolled as u32);

    let pa = read_portfolio(&svm, &data_pda);
    assert_eq!(pa.enrolled_count, 1, "second enrol must not increment count");
}

#[test]
fn enroll_rejects_when_full() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    // Fill all MAX_ENROLLED_MARKETS slots.
    for i in 0..MAX_ENROLLED_MARKETS as u16 {
        svm.expire_blockhash();
        let market = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, market);
        send_signed(&mut svm, enroll_ix(program_id, &user, data_pda, market, i), &user)
            .unwrap_or_else(|e| panic!("slot {i} failed: {e:?}"));
    }
    let pa = read_portfolio(&svm, &data_pda);
    assert_eq!(pa.enrolled_count as usize, MAX_ENROLLED_MARKETS);

    // The next enrol must fail.
    svm.expire_blockhash();
    let extra = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, extra);
    let res = send_signed(&mut svm, enroll_ix(program_id, &user, data_pda, extra, 99), &user);
    assert_custom_error(res, PortfolioError::TooManyEnrolled as u32);
}

#[test]
fn enroll_rejects_when_paused() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    // Pause.
    let pause_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
        ],
        data: vec![9u8, 1u8],
    };
    send_signed(&mut svm, pause_ix, &user).unwrap();
    svm.expire_blockhash();

    let market = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, market);
    let res = send_signed(&mut svm, enroll_ix(program_id, &user, data_pda, market, 1), &user);
    assert_custom_error(res, PortfolioError::Paused as u32);
}

#[test]
fn enroll_rejects_wrong_signer() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    let attacker = Keypair::new();
    svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();

    let market = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, market);
    let res = send_signed(
        &mut svm,
        enroll_ix(program_id, &attacker, data_pda, market, 1),
        &attacker,
    );
    assert_custom_error(res, PortfolioError::BadOwner as u32);
}

#[test]
fn unenroll_finds_and_swap_removes() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    let m1 = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, m1);
    let m2 = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, m2);
    let m3 = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, m3);
    for (i, m) in [m1, m2, m3].iter().enumerate() {
        svm.expire_blockhash();
        send_signed(&mut svm, enroll_ix(program_id, &user, data_pda, *m, i as u16), &user)
            .unwrap();
    }
    assert_eq!(read_portfolio(&svm, &data_pda).enrolled_count, 3);

    // Unenroll the middle one (m2, idx=1). Swap-remove should put m3 in
    // slot 1, leave m1 in slot 0, and zero slot 2.
    svm.expire_blockhash();
    send_signed(&mut svm, unenroll_ix(program_id, &user, data_pda, m2, 1), &user).unwrap();

    let pa = read_portfolio(&svm, &data_pda);
    assert_eq!(pa.enrolled_count, 2);
    assert_eq!(pa.enrolled[0].market, m1.to_bytes(), "m1 stays at 0");
    assert_eq!(pa.enrolled[0].account_idx, 0);
    assert_eq!(pa.enrolled[1].market, m3.to_bytes(), "m3 swap-moved to 1");
    assert_eq!(pa.enrolled[1].account_idx, 2);
    // Slot 2 must be zero (swap-remove invariant: vacated tail is wiped).
    assert_eq!(pa.enrolled[2].market, [0u8; 32]);
    assert_eq!(pa.enrolled[2].account_idx, 0);
}

#[test]
fn unenroll_last_just_decrements_count() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    let m1 = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, m1);
    let m2 = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, m2);
    send_signed(&mut svm, enroll_ix(program_id, &user, data_pda, m1, 0), &user).unwrap();
    svm.expire_blockhash();
    send_signed(&mut svm, enroll_ix(program_id, &user, data_pda, m2, 1), &user).unwrap();

    svm.expire_blockhash();
    send_signed(&mut svm, unenroll_ix(program_id, &user, data_pda, m2, 1), &user).unwrap();

    let pa = read_portfolio(&svm, &data_pda);
    assert_eq!(pa.enrolled_count, 1);
    assert_eq!(pa.enrolled[0].market, m1.to_bytes());
    assert_eq!(pa.enrolled[1].market, [0u8; 32]);
}

#[test]
fn unenroll_rejects_unknown_market() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    let m1 = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, m1);
    send_signed(&mut svm, enroll_ix(program_id, &user, data_pda, m1, 0), &user).unwrap();

    svm.expire_blockhash();
    let unknown = Pubkey::new_unique();
    let res = send_signed(
        &mut svm,
        unenroll_ix(program_id, &user, data_pda, unknown, 0),
        &user,
    );
    assert_custom_error(res, PortfolioError::MarketNotEnrolled as u32);
}

#[test]
fn unenroll_rejects_wrong_idx_for_known_market() {
    // Enrolled (m1, 5). Unenroll (m1, 6) → MarketNotEnrolled.
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    let market = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, market);
    send_signed(&mut svm, enroll_ix(program_id, &user, data_pda, market, 5), &user).unwrap();

    svm.expire_blockhash();
    let res = send_signed(
        &mut svm,
        unenroll_ix(program_id, &user, data_pda, market, 6),
        &user,
    );
    assert_custom_error(res, PortfolioError::MarketNotEnrolled as u32);
}

#[test]
fn unenroll_from_empty_portfolio_rejects() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    let market = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, market);
    let res = send_signed(
        &mut svm,
        unenroll_ix(program_id, &user, data_pda, market, 0),
        &user,
    );
    assert_custom_error(res, PortfolioError::MarketNotEnrolled as u32);
}

#[test]
fn enroll_rejects_truncated_data() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    let market = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, market);
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(market, false),
        ],
        // Tag 1 with only one body byte (need 2).
        data: vec![1u8, 0u8],
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadInstruction as u32);
}

#[test]
fn enroll_rejects_extra_data_byte() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    let market = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, market);
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(market, false),
        ],
        data: vec![1u8, 5u8, 0u8, 0xff], // tag + 2-byte body + trailing junk
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadInstruction as u32);
}

#[test]
fn enroll_rejects_wrong_account_count() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    // Pass only 2 accounts (need 3).
    let mut data = vec![1u8];
    data.extend_from_slice(&5u16.to_le_bytes());
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
        ],
        data,
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadAccountCount as u32);
}
