//! Adversarial integration tests — every rejection must land on a SPECIFIC
//! `PortfolioError`, never on a vacuous "any error" catchall.
//!
//! Defense 1 / Defense 3 attack vectors are appended at the end of this file.
//! They test: oracle substitution attacks (Trade), stale oracle, low-confidence
//! oracle, bounty drainage grief (RebalanceCrank), and corrupted enrolled_count
//! (CRIT-5 regression).

mod common;

use bytemuck::from_bytes;
use common::{assert_custom_error, fresh_env, pdas_for, send_init, send_signed, percolator_owned_slab};
use percolator_portfolio::{
    constants::PORTFOLIO_VAULT_SEED,
    cpi as cpi_helpers,
    errors::PortfolioError,
    state::PortfolioAccount,
};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    sysvar,
    system_program,
};

const PERCOLATOR_PROG: Pubkey = Pubkey::new_from_array(cpi_helpers::PERCOLATOR_PROGRAM_ID);
const SPL_TOKEN_PROG: Pubkey = Pubkey::new_from_array(cpi_helpers::SPL_TOKEN_ID);

// Layout offsets (new main-repo layout).
const OFF_HAS_VAULT: usize = 116;
const OFF_PAUSED: usize = 114;
const OFF_ENROLLED_COUNT: usize = 115;
const OFF_VAULT_BUMP: usize = 112;
const OFF_ENROLLED_BASE: usize = 184;
const SLOT_SIZE: usize = 48;
const SLOT_MARKET_OFF: usize = 8;
const SLOT_ACCT_IDX_OFF: usize = 40;

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

// ── Defense 1 attack vectors ─────────────────────────────────────────────────
//
// These tests cover oracle-substitution and staleness attacks on the pre-trade
// aggregate IM check (Defense 1). All require the Trade ix's pair-region
// validation to fire, which means the portfolio must have at least 2 enrolled
// markets so the (slab, oracle) pair loop runs.
//
// Tests that need engine slab data to decode cleanly (read_oracle_price_e6
// calls into percolator_prog::zc) are blocked on the integration_env
// InitMarket encoding mismatch and are marked #[ignore].

#[test]
fn defense1_attack_fake_percolator_rejected_before_oracle() {
    // Substituting a_percolator_prog with a fake pubkey is caught at the
    // verify_percolator_program call, BEFORE any oracle borrow. This is a
    // Defense 1 / BadProgram gate test using the Trade ix path.
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);

    // Set has_vault=1 and enroll one market.
    let slab = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, slab);
    let mut acct = svm.get_account(&data_pda).unwrap();
    acct.data[OFF_HAS_VAULT] = 1;
    acct.data[OFF_ENROLLED_COUNT] = 1;
    let s0 = OFF_ENROLLED_BASE + SLOT_MARKET_OFF;
    acct.data[s0..s0 + 32].copy_from_slice(&slab.to_bytes());
    svm.set_account(data_pda, acct).unwrap();

    let fake_prog = Pubkey::new_unique();
    let mut d = vec![5u8];
    d.extend_from_slice(&0u16.to_le_bytes()); // account_idx
    d.extend_from_slice(&1u16.to_le_bytes()); // lp_idx
    d.push(0u8);                               // side = buy
    d.extend_from_slice(&1_000u64.to_le_bytes()); // size_q
    d.extend_from_slice(&1_000_000u64.to_le_bytes()); // limit_price_e6

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(slab, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false), // oracle
            AccountMeta::new_readonly(Pubkey::new_unique(), false), // matcher_prog
            AccountMeta::new(Pubkey::new_unique(), false),          // matcher_ctx
            AccountMeta::new_readonly(Pubkey::new_unique(), false), // lp_pda
            AccountMeta::new_readonly(Pubkey::new_unique(), false), // lp_owner
            AccountMeta::new_readonly(fake_prog, false),            // <-- fake percolator-prog
        ],
        data: d,
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadProgram as u32);
}

#[test]
#[ignore = "requires engine slab decode (read_oracle_price_e6) — harness-issue: InitMarket encoding mismatch"]
fn defense1_attack_substituted_oracle() {
    // An attacker passes an oracle for a DIFFERENT feed_id than the slab's
    // MarketConfig.index_feed_id. The engine's read_pyth_price_e6 call inside
    // the wrapper's Defense 1 loop validates feed_id and rejects it with
    // MarginNotionalRejected or an engine-level error.
    //
    // Unblocked when integration_env::IntegrationEnv::new() succeeds.
    todo!("harness-issue: InitMarket encoding mismatch")
}

#[test]
#[ignore = "requires engine slab decode (read_oracle_price_e6) — harness-issue: InitMarket encoding mismatch"]
fn defense1_attack_stale_oracle() {
    // Pass an oracle whose publish_time is older than max_staleness_secs.
    // read_oracle_price_e6 returns an error → propagated as custom error.
    //
    // Unblocked when integration_env::IntegrationEnv::new() succeeds.
    todo!("harness-issue: InitMarket encoding mismatch")
}

#[test]
#[ignore = "requires engine slab decode (read_oracle_price_e6) — harness-issue: InitMarket encoding mismatch"]
fn defense1_attack_low_confidence_oracle() {
    // Pass an oracle where conf/price > conf_filter_bps.
    // read_oracle_price_e6 returns an error → propagated as custom error.
    //
    // Unblocked when integration_env::IntegrationEnv::new() succeeds.
    todo!("harness-issue: InitMarket encoding mismatch")
}

// ── Defense 3 attack vectors ─────────────────────────────────────────────────

#[test]
fn defense3_bounty_drainage_grief_amount_too_small_for_bounty() {
    // A griever calls RebalanceCrank with amount = 99 (far below the
    // A-4 minimum: MIN_REBALANCE_AMOUNT_E6 = 10_000_000 i.e. 10 USDC).
    // The minimum-amount gate fires ZeroAmount before any CPI or token
    // movement. Confirms: no vault drainage on dust-amount crank grief
    // attempts.
    let (mut svm, program_id, user) = fresh_env();
    let from_slab = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, from_slab);
    let to_slab = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, to_slab);
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);

    // Derive the canonical vault PDA; patch vault_bump so the handler's
    // PDA check passes and we reach the slab-decode gate.
    let vault_seed = percolator_portfolio::constants::PORTFOLIO_VAULT_SEED;
    let (vault_pda, vault_bump_val) =
        Pubkey::find_program_address(&[vault_seed, user.pubkey().as_ref()], &program_id);

    // Patch state.
    let mut acct = svm.get_account(&data_pda).unwrap();
    acct.data[OFF_HAS_VAULT] = 1;
    acct.data[OFF_VAULT_BUMP] = vault_bump_val;
    acct.data[OFF_ENROLLED_COUNT] = 2;
    let s0 = OFF_ENROLLED_BASE + SLOT_MARKET_OFF;
    acct.data[s0..s0 + 32].copy_from_slice(&from_slab.to_bytes());
    acct.data[OFF_ENROLLED_BASE + SLOT_ACCT_IDX_OFF..OFF_ENROLLED_BASE + SLOT_ACCT_IDX_OFF + 2]
        .copy_from_slice(&0u16.to_le_bytes());
    let s1 = OFF_ENROLLED_BASE + SLOT_SIZE + SLOT_MARKET_OFF;
    acct.data[s1..s1 + 32].copy_from_slice(&to_slab.to_bytes());
    let i1 = OFF_ENROLLED_BASE + SLOT_SIZE + SLOT_ACCT_IDX_OFF;
    acct.data[i1..i1 + 2].copy_from_slice(&1u16.to_le_bytes());
    svm.set_account(data_pda, acct).unwrap();

    // amount = 99 (below MIN_REBALANCE_AMOUNT_E6 = 10_000_000).
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault_pda, false), // correct vault PDA
            AccountMeta::new(Pubkey::new_unique(), false), // payout
            AccountMeta::new_readonly(SPL_TOKEN_PROG, false),
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
        data: {
            let mut d = vec![13u8];
            d.extend_from_slice(&0u16.to_le_bytes()); // from_idx
            d.extend_from_slice(&1u16.to_le_bytes()); // to_idx
            d.extend_from_slice(&99u64.to_le_bytes()); // amount = 99 < MIN_REBALANCE_AMOUNT_E6
            d
        },
    };
    let res = send_signed(&mut svm, ix, &user);
    // A-4 (MIN_REBALANCE_AMOUNT_E6 = 10_000_000) fires BEFORE the slab-decode
    // gate: amount=99 < 10_000_000 returns ZeroAmount. This is an earlier,
    // cheaper guard than MarginSlabDecodeFailed. The security property holds:
    // the call fails before any token transfer.
    assert_custom_error(res, PortfolioError::ZeroAmount as u32);
}

#[test]
fn defense3_crit5_corrupted_enrolled_count_is_clamped() {
    // CRIT-5 regression: patch enrolled_count to 200 (> MAX_ENROLLED_MARKETS=16).
    // A handler that reads enrolled[] must NOT OOB-index the 16-slot array.
    //
    // We exercise via RebalanceCrank (tag 13): find_enrolled() is called at
    // the enrollment-check gate before any slab decode. It clamps the loop to
    // min(200, 16) = 16, scans all 16 zeroed slots, finds no match, and
    // returns MarketNotEnrolled — the program must NOT panic.
    // enrolled_count must be unchanged (rejected ix writes nothing).
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);

    // Patch enrolled_count = 200 (corrupted / adversarially written) and
    // has_vault = 1 so the handler doesn't bail at AccountNotInitialized
    // before reaching find_enrolled.
    let mut acct = svm.get_account(&data_pda).unwrap();
    acct.data[OFF_ENROLLED_COUNT] = 200;
    acct.data[OFF_HAS_VAULT] = 1;
    svm.set_account(data_pda, acct).unwrap();

    // RebalanceCrank (tag 13) — 15 accounts with a random slab pair.
    // find_enrolled clamps loop to 16; all slots zeroed → MarketNotEnrolled.
    // The program must NOT panic from OOB access.
    let from_slab = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, from_slab);
    let to_slab = Pubkey::new_unique();
    percolator_owned_slab(&mut svm, to_slab);
    let mut d = vec![13u8];
    d.extend_from_slice(&0u16.to_le_bytes()); // from_idx
    d.extend_from_slice(&1u16.to_le_bytes()); // to_idx
    d.extend_from_slice(&1_000_000u64.to_le_bytes()); // amount
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),  // caller (signer)
            AccountMeta::new(data_pda, false),               // portfolio_data (NOT signer)
            AccountMeta::new_readonly(auth_pda, false),      // auth
            AccountMeta::new(Pubkey::new_unique(), false),   // vault (random — MarketNotEnrolled fires first)
            AccountMeta::new(Pubkey::new_unique(), false),   // payout
            AccountMeta::new_readonly(SPL_TOKEN_PROG, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
            AccountMeta::new(from_slab, false),
            AccountMeta::new(Pubkey::new_unique(), false),   // from_market_vault
            AccountMeta::new_readonly(Pubkey::new_unique(), false), // from_vault_auth
            AccountMeta::new_readonly(Pubkey::new_unique(), false), // from_oracle
            AccountMeta::new(to_slab, false),
            AccountMeta::new(Pubkey::new_unique(), false),   // to_market_vault
            AccountMeta::new_readonly(Pubkey::new_unique(), false), // to_oracle
        ],
        data: d,
    };
    let res = send_signed(&mut svm, ix, &user);
    // find_enrolled scans i=0..16 (clamped), finds nothing, returns
    // MarketNotEnrolled — NOT a panic or OOB abort.
    assert_custom_error(res, PortfolioError::MarketNotEnrolled as u32);
    // Verify enrolled_count was not mutated by the rejected call.
    let acct_after = svm.get_account(&data_pda).unwrap();
    assert_eq!(
        acct_after.data[OFF_ENROLLED_COUNT], 200,
        "enrolled_count must not be modified by a rejected instruction"
    );
}

// ── Slab-owner validation — Defense 1 / aggregate-IM bypass closure ─────────
//
// These tests pin the wrapper-side invariant that every slab `AccountInfo`
// the wrapper decodes via `zc::engine_ref` / `state::read_config` is owned by
// `PERCOLATOR_PROGRAM`. Without that check, an attacker could substitute a
// self-owned account whose raw bytes happen to decode as a valid engine
// layout — fabricating phantom per-account equity in the aggregate-IM gate.

use solana_sdk::account::Account;

/// EnrollMarket rejects a market `AccountInfo` owned by `system_program`
/// (the default for any address with no prior `set_account`). This is the
/// canonical adversarial scenario: an attacker would otherwise be able to
/// enrol a placeholder address they later populate via their own program.
#[test]
fn enroll_rejects_slab_not_owned_by_percolator() {
    let (mut svm, program_id, user) = fresh_env();
    common::send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    // System-owned dummy slab account — passes existing account-count and
    // signer checks but must fail the new owner gate.
    let spoof = Pubkey::new_unique();
    svm.set_account(
        spoof,
        Account {
            lamports: 1_000_000,
            data: vec![0u8; 8],
            owner: system_program::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();

    let mut d = vec![1u8];
    d.extend_from_slice(&0u16.to_le_bytes());
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(spoof, false),
        ],
        data: d,
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadSlabOwner as u32);

    // Defensive: rejected ix must not have mutated enrolled_count.
    let pa = svm.get_account(&data_pda).unwrap();
    assert_eq!(
        pa.data[OFF_ENROLLED_COUNT], 0,
        "enrolled_count must remain 0 after a rejected enrol"
    );
}

/// EnrollMarket rejects a market `AccountInfo` owned by the SPL Token
/// program. This covers the realistic confusion attack where a caller
/// supplies a token account in the slab slot. Distinct error code (not
/// `BadMint`) lets triage disambiguate slab vs. token-account misuse.
#[test]
fn enroll_rejects_slab_owned_by_token_program() {
    let (mut svm, program_id, user) = fresh_env();
    common::send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);

    let spoof = Pubkey::new_unique();
    svm.set_account(
        spoof,
        Account {
            lamports: 1_000_000,
            data: vec![0u8; 165],
            owner: SPL_TOKEN_PROG,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();

    let mut d = vec![1u8];
    d.extend_from_slice(&0u16.to_le_bytes());
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(spoof, false),
        ],
        data: d,
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadSlabOwner as u32);
}

/// Trade rejects a TARGET slab `AccountInfo` not owned by the percolator
/// program. The owner gate fires before the Defense-1 pair-region walk, so
/// the attack chain (spoof target slab → fabricate equity) terminates here.
#[test]
fn trade_rejects_target_slab_not_owned_by_percolator() {
    let (mut svm, program_id, user) = fresh_env();
    common::send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);

    // Spoof slab: system-owned, but pubkey written into pa.enrolled[0] so
    // the wrapper's find_enrolled membership check passes. The owner gate
    // must reject before the borrow on slab bytes.
    let spoof = Pubkey::new_unique();
    svm.set_account(
        spoof,
        Account {
            lamports: 1_000_000,
            data: vec![0u8; 8],
            owner: system_program::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();

    // Inject enrolled[0] = (spoof, 0) and has_vault = 1.
    let mut acct = svm.get_account(&data_pda).unwrap();
    acct.data[OFF_HAS_VAULT] = 1;
    acct.data[OFF_ENROLLED_COUNT] = 1;
    let s0 = OFF_ENROLLED_BASE + SLOT_MARKET_OFF;
    acct.data[s0..s0 + 32].copy_from_slice(&spoof.to_bytes());
    svm.set_account(data_pda, acct).unwrap();

    let mut d = vec![5u8];
    d.extend_from_slice(&0u16.to_le_bytes()); // account_idx
    d.extend_from_slice(&1u16.to_le_bytes()); // lp_idx
    d.push(0u8); // side
    d.extend_from_slice(&1_000u64.to_le_bytes()); // size_q
    d.extend_from_slice(&1_000_000u64.to_le_bytes()); // limit_price_e6

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(spoof, false), // <- spoofed target slab
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
        ],
        data: d,
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadSlabOwner as u32);
}

/// RebalanceCrank rejects a `from_slab` that's not owned by the percolator
/// program. The owner gate fires before the slab is borrowed for the H-8
/// post-withdraw IM projection.
#[test]
fn crank_rejects_from_slab_not_owned_by_percolator() {
    let (mut svm, program_id, user) = fresh_env();
    common::send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);

    // Spoofed from_slab (system-owned), legitimate to_slab placeholder
    // (percolator-owned but no real data; we won't reach the to-side decode).
    let spoof_from = Pubkey::new_unique();
    svm.set_account(
        spoof_from,
        Account {
            lamports: 1_000_000,
            data: vec![0u8; 8],
            owner: system_program::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    let to_slab = Pubkey::new_unique();
    svm.set_account(
        to_slab,
        Account {
            lamports: 1_000_000,
            data: vec![],
            owner: PERCOLATOR_PROG,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();

    // Derive canonical vault PDA so the handler reaches the slab gate.
    let (vault_pda, vault_bump_val) = Pubkey::find_program_address(
        &[PORTFOLIO_VAULT_SEED, user.pubkey().as_ref()],
        &program_id,
    );

    // Inject enrolled[0..2] and has_vault.
    let mut acct = svm.get_account(&data_pda).unwrap();
    acct.data[OFF_HAS_VAULT] = 1;
    acct.data[OFF_VAULT_BUMP] = vault_bump_val;
    acct.data[OFF_ENROLLED_COUNT] = 2;
    let s0 = OFF_ENROLLED_BASE + SLOT_MARKET_OFF;
    acct.data[s0..s0 + 32].copy_from_slice(&spoof_from.to_bytes());
    let s1 = OFF_ENROLLED_BASE + SLOT_SIZE + SLOT_MARKET_OFF;
    acct.data[s1..s1 + 32].copy_from_slice(&to_slab.to_bytes());
    let i1 = OFF_ENROLLED_BASE + SLOT_SIZE + SLOT_ACCT_IDX_OFF;
    acct.data[i1..i1 + 2].copy_from_slice(&1u16.to_le_bytes());
    svm.set_account(data_pda, acct).unwrap();

    let mut d = vec![13u8];
    d.extend_from_slice(&0u16.to_le_bytes()); // from_idx
    d.extend_from_slice(&1u16.to_le_bytes()); // to_idx
    d.extend_from_slice(&10_000_000u64.to_le_bytes()); // amount (≥ MIN_REBALANCE_AMOUNT_E6)

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault_pda, false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(SPL_TOKEN_PROG, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
            AccountMeta::new(spoof_from, false), // <- spoofed from_slab
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new(to_slab, false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
        ],
        data: d,
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadSlabOwner as u32);
}
