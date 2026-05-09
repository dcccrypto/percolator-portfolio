//! Adversarial integration tests — every rejection must land on a SPECIFIC
//! `PortfolioError`, never on a vacuous "any error" catchall.

mod common;

use bytemuck::from_bytes;
use common::{assert_custom_error, fresh_env, pdas_for, send_init, send_signed};
use percolator_portfolio::{errors::PortfolioError, state::PortfolioAccount};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_program,
};

fn read_portfolio(svm: &litesvm::LiteSVM, data_pda: &Pubkey) -> PortfolioAccount {
    let acct = svm.get_account(data_pda).unwrap();
    *from_bytes::<PortfolioAccount>(
        &acct.data[..core::mem::size_of::<PortfolioAccount>()],
    )
}

/// Setup with a victim who has an initialised portfolio (buffer=200, max_lev=50_000).
struct VictimEnv {
    svm: litesvm::LiteSVM,
    program_id: Pubkey,
    victim: Keypair,
    victim_data_pda: Pubkey,
    victim_auth_pda: Pubkey,
}

fn victim_env() -> VictimEnv {
    let (mut svm, program_id, victim) = fresh_env();
    send_init(&mut svm, program_id, &victim, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&victim.pubkey(), &program_id);
    VictimEnv {
        svm,
        program_id,
        victim,
        victim_data_pda: data_pda,
        victim_auth_pda: auth_pda,
    }
}

#[test]
fn attacker_cannot_update_victims_portfolio() {
    let mut e = victim_env();
    let attacker = Keypair::new();
    e.svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();

    let mut data = vec![8u8];
    data.extend_from_slice(&999u16.to_le_bytes());
    data.extend_from_slice(&30_000u32.to_le_bytes());
    data.extend_from_slice(Pubkey::new_unique().as_ref());
    let ix = Instruction {
        program_id: e.program_id,
        accounts: vec![
            AccountMeta::new_readonly(attacker.pubkey(), true),
            AccountMeta::new(e.victim_data_pda, false),
        ],
        data,
    };
    let res = send_signed(&mut e.svm, ix, &attacker);
    assert_custom_error(res, PortfolioError::BadOwner as u32);
    assert_eq!(read_portfolio(&e.svm, &e.victim_data_pda).buffer_bps, 200);
}

#[test]
fn signer_does_not_grant_access_without_matching_owner() {
    // Both users have legitimate portfolios. user2 tries to mutate
    // victim's. Must fail with BadOwner.
    let mut e = victim_env();
    let user2 = Keypair::new();
    e.svm.airdrop(&user2.pubkey(), 1_000_000_000).unwrap();
    send_init(&mut e.svm, e.program_id, &user2, 200, 50_000, Pubkey::new_unique()).unwrap();

    let mut data = vec![8u8];
    data.extend_from_slice(&999u16.to_le_bytes());
    data.extend_from_slice(&30_000u32.to_le_bytes());
    data.extend_from_slice(Pubkey::new_unique().as_ref());
    let ix = Instruction {
        program_id: e.program_id,
        accounts: vec![
            AccountMeta::new_readonly(user2.pubkey(), true),
            AccountMeta::new(e.victim_data_pda, false),
        ],
        data,
    };
    let res = send_signed(&mut e.svm, ix, &user2);
    assert_custom_error(res, PortfolioError::BadOwner as u32);
}

#[test]
fn non_program_owned_data_account_rejected() {
    // Pass victim's wallet (system-program-owned) as a_data.
    // check_portfolio_account fails on `a_data.owner != program_id` →
    // AccountNotInitialized (= 12).
    let mut e = victim_env();

    let mut data = vec![8u8];
    data.extend_from_slice(&999u16.to_le_bytes());
    data.extend_from_slice(&30_000u32.to_le_bytes());
    data.extend_from_slice(Pubkey::new_unique().as_ref());

    let ix = Instruction {
        program_id: e.program_id,
        accounts: vec![
            AccountMeta::new_readonly(e.victim.pubkey(), true),
            AccountMeta::new(e.victim.pubkey(), false), // wallet, not PDA
        ],
        data,
    };
    let res = send_signed(&mut e.svm, ix, &e.victim);
    assert_custom_error(res, PortfolioError::AccountNotInitialized as u32);
}

#[test]
fn set_paused_owner_check_applies() {
    let mut e = victim_env();
    let attacker = Keypair::new();
    e.svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();

    let ix = Instruction {
        program_id: e.program_id,
        accounts: vec![
            AccountMeta::new_readonly(attacker.pubkey(), true),
            AccountMeta::new(e.victim_data_pda, false),
        ],
        data: vec![9u8, 1u8],
    };
    let res = send_signed(&mut e.svm, ix, &attacker);
    assert_custom_error(res, PortfolioError::BadOwner as u32);
    assert_eq!(read_portfolio(&e.svm, &e.victim_data_pda).paused, 0);
}

#[test]
fn init_rejects_non_pda_data_address() {
    let mut e = victim_env();
    let user2 = Keypair::new();
    e.svm.airdrop(&user2.pubkey(), 1_000_000_000).unwrap();

    let bogus_data = Pubkey::new_unique();
    let (_, _, real_auth, _) = pdas_for(&user2.pubkey(), &e.program_id);

    let mut data = vec![0u8];
    data.extend_from_slice(&200u16.to_le_bytes());
    data.extend_from_slice(&50_000u32.to_le_bytes());
    data.extend_from_slice(Pubkey::new_unique().as_ref());

    let ix = Instruction {
        program_id: e.program_id,
        accounts: vec![
            AccountMeta::new(user2.pubkey(), true),
            AccountMeta::new(bogus_data, false),
            AccountMeta::new_readonly(real_auth, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    };
    let res = send_signed(&mut e.svm, ix, &user2);
    assert_custom_error(res, PortfolioError::BadPda as u32);
}

#[test]
fn init_rejects_wrong_auth_pda_address() {
    let mut e = victim_env();
    let user2 = Keypair::new();
    e.svm.airdrop(&user2.pubkey(), 1_000_000_000).unwrap();

    let (real_data, _, _, _) = pdas_for(&user2.pubkey(), &e.program_id);

    let mut data = vec![0u8];
    data.extend_from_slice(&200u16.to_le_bytes());
    data.extend_from_slice(&50_000u32.to_le_bytes());
    data.extend_from_slice(Pubkey::new_unique().as_ref());

    let ix = Instruction {
        program_id: e.program_id,
        accounts: vec![
            AccountMeta::new(user2.pubkey(), true),
            AccountMeta::new(real_data, false),
            // Pass real_data as a_auth — wrong PDA seed.
            AccountMeta::new_readonly(real_data, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    };
    let res = send_signed(&mut e.svm, ix, &user2);
    assert_custom_error(res, PortfolioError::BadPda as u32);
}

#[test]
fn extra_accounts_rejected() {
    let mut e = victim_env();
    let mut data = vec![8u8];
    data.extend_from_slice(&999u16.to_le_bytes());
    data.extend_from_slice(&30_000u32.to_le_bytes());
    data.extend_from_slice(Pubkey::new_unique().as_ref());
    let ix = Instruction {
        program_id: e.program_id,
        accounts: vec![
            AccountMeta::new_readonly(e.victim.pubkey(), true),
            AccountMeta::new(e.victim_data_pda, false),
            AccountMeta::new_readonly(e.victim_auth_pda, false), // extra
        ],
        data,
    };
    let res = send_signed(&mut e.svm, ix, &e.victim);
    assert_custom_error(res, PortfolioError::BadAccountCount as u32);
    assert_eq!(read_portfolio(&e.svm, &e.victim_data_pda).buffer_bps, 200);
}

#[test]
fn set_paused_rejects_non_bool_byte() {
    let mut e = victim_env();
    for byte in [2u8, 0xff, 100u8] {
        e.svm.expire_blockhash();
        let ix = Instruction {
            program_id: e.program_id,
            accounts: vec![
                AccountMeta::new_readonly(e.victim.pubkey(), true),
                AccountMeta::new(e.victim_data_pda, false),
            ],
            data: vec![9u8, byte],
        };
        let res = send_signed(&mut e.svm, ix, &e.victim);
        assert_custom_error(res, PortfolioError::BadInstruction as u32);
    }
    assert_eq!(read_portfolio(&e.svm, &e.victim_data_pda).paused, 0);
}

#[test]
fn init_rejects_oversized_instruction_data() {
    let mut e = victim_env();
    let user2 = Keypair::new();
    e.svm.airdrop(&user2.pubkey(), 1_000_000_000).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user2.pubkey(), &e.program_id);

    let mut data = vec![0u8];
    data.extend_from_slice(&200u16.to_le_bytes());
    data.extend_from_slice(&50_000u32.to_le_bytes());
    data.extend_from_slice(Pubkey::new_unique().as_ref());
    data.push(0xff);

    let ix = Instruction {
        program_id: e.program_id,
        accounts: vec![
            AccountMeta::new(user2.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    };
    let res = send_signed(&mut e.svm, ix, &user2);
    assert_custom_error(res, PortfolioError::BadInstruction as u32);
}

#[test]
fn corrupted_magic_rejects_with_bad_magic() {
    // Manually corrupt the magic field of an initialised portfolio account
    // (via litesvm::set_account) and verify our check fires with BadMagic
    // — not e.g. BadOwner or AccountNotInitialized.
    let mut e = victim_env();
    let mut acct = e.svm.get_account(&e.victim_data_pda).unwrap();
    // First 8 bytes are the magic field. Zero them.
    for b in &mut acct.data[..8] {
        *b = 0;
    }
    e.svm.set_account(e.victim_data_pda, acct).unwrap();

    let ix = Instruction {
        program_id: e.program_id,
        accounts: vec![
            AccountMeta::new_readonly(e.victim.pubkey(), true),
            AccountMeta::new(e.victim_data_pda, false),
        ],
        data: vec![9u8, 1u8], // SetPaused(true)
    };
    let res = send_signed(&mut e.svm, ix, &e.victim);
    assert_custom_error(res, PortfolioError::BadMagic as u32);
}

#[test]
fn corrupted_version_rejects_with_bad_version() {
    let mut e = victim_env();
    let mut acct = e.svm.get_account(&e.victim_data_pda).unwrap();
    // version field offset = 8 (magic) + 8 (last_rebal) + 8 (cached_at) +
    // 8 (cached_eq) + 8 (cached_mmr) + 32 (owner) + 32 (keeper) + 4
    // (max_lev) + 2 (buffer) + 1 (bump) + 1 (auth_bump) + 1 (vault_bump) = 113
    acct.data[113] = 99; // bogus version
    e.svm.set_account(e.victim_data_pda, acct).unwrap();

    let ix = Instruction {
        program_id: e.program_id,
        accounts: vec![
            AccountMeta::new_readonly(e.victim.pubkey(), true),
            AccountMeta::new(e.victim_data_pda, false),
        ],
        data: vec![9u8, 1u8],
    };
    let res = send_signed(&mut e.svm, ix, &e.victim);
    assert_custom_error(res, PortfolioError::BadVersion as u32);
}

#[test]
fn corrupted_bump_rejects_with_bad_pda() {
    // Corrupt the stored data-PDA bump. create_program_address with the
    // wrong bump produces an address that doesn't match a_data.key.
    let mut e = victim_env();
    let mut acct = e.svm.get_account(&e.victim_data_pda).unwrap();
    // bump field offset = 110.
    let original = acct.data[110];
    acct.data[110] = original.wrapping_add(1); // shift the bump
    e.svm.set_account(e.victim_data_pda, acct).unwrap();

    let ix = Instruction {
        program_id: e.program_id,
        accounts: vec![
            AccountMeta::new_readonly(e.victim.pubkey(), true),
            AccountMeta::new(e.victim_data_pda, false),
        ],
        data: vec![9u8, 1u8],
    };
    let res = send_signed(&mut e.svm, ix, &e.victim);
    assert_custom_error(res, PortfolioError::BadPda as u32);
}
