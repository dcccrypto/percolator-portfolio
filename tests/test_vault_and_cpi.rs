//! Wrapper-validation tests for `InitVault`, `Deposit`, `Withdraw`,
//! `Rebalance`, `EmergencyClose`.
//!
//! IMPORTANT — what these tests cover and what they do NOT:
//!
//!   COVERED: every handler's argument-validation path. Each test below
//!   constructs an instruction with a deliberate validation failure
//!   (wrong account count, wrong PDA, wrong signer, paused, vault not
//!   initialised, market not enrolled, etc.) and asserts the SPECIFIC
//!   `PortfolioError` discriminant returned. None of these tests ever
//!   reach the CPI into `percolator-prog` — the wrapper rejects first.
//!
//!   NOT COVERED: the happy paths. Deposit / Withdraw / Rebalance /
//!   EmergencyClose all CPI into `percolator-prog`, and `percolator-prog`
//!   itself is not loaded into the LiteSVM in this file. Building real
//!   end-to-end tests requires (1) compiling percolator-prog with its
//!   `test` feature, (2) loading both .so files, (3) initialising a
//!   market with admin keypair + mint + vault + oracle, (4) calling
//!   InitUser via the portfolio program so the per-market account is
//!   owned by portfolio_auth, (5) funding the user's USDC ATA, then
//!   exercising the round-trip. That harness is intentionally deferred
//!   until the engine asks (`GetAccountHealth`, `UpdateAccountOwner`)
//!   land — both will materially change the CPI shape, so writing the
//!   happy-path tests now would be premature.
//!
//!   Until then: the tests below verify the WRAPPER side of the contract
//!   (fail-fast on every bogus input shape). The percolator-prog side
//!   is verified by percolator-prog's own test suite.

mod common;

use bytemuck::from_bytes;
use common::{assert_custom_error, fresh_env, pdas_for, send_init, send_signed};
use percolator_portfolio::{
    constants::{PORTFOLIO_AUTH_SEED, PORTFOLIO_VAULT_SEED},
    cpi as cpi_helpers,
    errors::PortfolioError,
    state::PortfolioAccount,
};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_program,
};

const SPL_TOKEN: Pubkey = Pubkey::new_from_array(cpi_helpers::SPL_TOKEN_ID);
const PERCOLATOR_PROG: Pubkey = Pubkey::new_from_array(cpi_helpers::PERCOLATOR_PROGRAM_ID);

fn vault_pda(user: &Pubkey, program_id: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[PORTFOLIO_VAULT_SEED, user.as_ref()], program_id)
}

fn read_portfolio(svm: &litesvm::LiteSVM, data_pda: &Pubkey) -> PortfolioAccount {
    let acct = svm.get_account(data_pda).unwrap();
    *from_bytes::<PortfolioAccount>(
        &acct.data[..core::mem::size_of::<PortfolioAccount>()],
    )
}

fn init_vault_ix(
    program_id: Pubkey,
    user: Pubkey,
    data_pda: Pubkey,
    auth_pda: Pubkey,
    vault: Pubkey,
    mint: Pubkey,
) -> Instruction {
    Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user, true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
        ],
        data: vec![10u8],
    }
}

#[test]
fn init_vault_rejects_uninitialised_portfolio() {
    // No InitPortfolio first — InitVault should fail at the
    // check_portfolio_account chain.
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _) = vault_pda(&user.pubkey(), &program_id);
    let mint = Pubkey::new_unique();

    let ix = init_vault_ix(program_id, user.pubkey(), data_pda, auth_pda, vault, mint);
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::AccountNotInitialized as u32);
}

#[test]
fn init_vault_rejects_wrong_system_program() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _) = vault_pda(&user.pubkey(), &program_id);
    let mint = Pubkey::new_unique();

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false), // bogus system program
            AccountMeta::new_readonly(SPL_TOKEN, false),
        ],
        data: vec![10u8],
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::WrongSystemProgram as u32);
}

#[test]
fn init_vault_rejects_wrong_token_program() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _) = vault_pda(&user.pubkey(), &program_id);
    let mint = Pubkey::new_unique();

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false), // bogus token program
        ],
        data: vec![10u8],
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::WrongTokenProgram as u32);
}

#[test]
fn init_vault_rejects_wrong_auth_pda() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _) = vault_pda(&user.pubkey(), &program_id);
    let mint = Pubkey::new_unique();
    let bogus_auth = Pubkey::new_unique();

    let ix = init_vault_ix(program_id, user.pubkey(), data_pda, bogus_auth, vault, mint);
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadPda as u32);
}

#[test]
fn init_vault_rejects_wrong_vault_pda() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let mint = Pubkey::new_unique();
    let bogus_vault = Pubkey::new_unique();

    let ix = init_vault_ix(program_id, user.pubkey(), data_pda, auth_pda, bogus_vault, mint);
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadPda as u32);
}

#[test]
fn init_vault_rejects_wrong_account_count() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _) = vault_pda(&user.pubkey(), &program_id);

    // Drop the mint account (only 6 accounts instead of 7).
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
        ],
        data: vec![10u8],
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadAccountCount as u32);
}

#[test]
fn init_vault_rejects_extra_data_byte() {
    // InitVault body must be empty.
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _) = vault_pda(&user.pubkey(), &program_id);
    let mint = Pubkey::new_unique();

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
        ],
        data: vec![10u8, 0xff], // extra trailing byte
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadInstruction as u32);
}

// ── Deposit / Withdraw / Rebalance / EmergencyClose validation tests ──
//
// These confirm the wrapper rejects bogus inputs WITHOUT reaching the CPI.
// Account-count / signer / paused / vault-not-initialised / market-not-
// enrolled paths.

#[test]
fn deposit_rejects_when_vault_not_initialised() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();

    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _) = vault_pda(&user.pubkey(), &program_id);
    let dummies = [
        Pubkey::new_unique(); // user_ata
        7
    ];
    let user_ata = dummies[0];
    let slab = dummies[1];
    let mvault = dummies[2];
    let mvault_auth = dummies[3];
    let clock = dummies[4];
    // Use the canonical percolator-prog ID so we get past
    // verify_percolator_program and reach the vault-not-initialised check.
    let percolator = PERCOLATOR_PROG;

    let mut data = vec![3u8]; // tag Deposit
    data.extend_from_slice(&0u16.to_le_bytes()); // account_idx
    data.extend_from_slice(&100u64.to_le_bytes()); // amount

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new_readonly(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new(user_ata, false),
            AccountMeta::new(slab, false),
            AccountMeta::new(mvault, false),
            AccountMeta::new_readonly(mvault_auth, false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(clock, false),
            AccountMeta::new_readonly(percolator, false),
        ],
        data,
    };
    let res = send_signed(&mut svm, ix, &user);
    // Vault uninitialised → vault_bump == 0 → AccountNotInitialized.
    assert_custom_error(res, PortfolioError::AccountNotInitialized as u32);
}

#[test]
fn deposit_rejects_wrong_account_count() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();

    // Pass only 5 accounts (need 11).
    let mut data = vec![3u8];
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&100u64.to_le_bytes());
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
        ],
        data,
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadAccountCount as u32);
}

#[test]
fn withdraw_rejects_when_paused() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();

    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);
    // Pause first.
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

    let (auth_pda, _) = (
        pdas_for(&user.pubkey(), &program_id).2,
        pdas_for(&user.pubkey(), &program_id).3,
    );
    let (vault, _) = vault_pda(&user.pubkey(), &program_id);
    let dummies = [Pubkey::new_unique(); 8];

    let mut data = vec![4u8];
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&100u64.to_le_bytes());

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new_readonly(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new(dummies[0], false), // user_ata
            AccountMeta::new(dummies[1], false), // slab
            AccountMeta::new(dummies[2], false), // mvault
            AccountMeta::new_readonly(dummies[3], false), // mvault_auth
            AccountMeta::new_readonly(dummies[4], false), // oracle
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(dummies[5], false), // clock
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
        ],
        data,
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::Paused as u32);
}

#[test]
fn rebalance_rejects_wrong_keeper() {
    let (mut svm, program_id, user) = fresh_env();
    let real_keeper = Pubkey::new_unique();
    send_init(&mut svm, program_id, &user, 200, 50_000, real_keeper).unwrap();

    let attacker = Keypair::new();
    svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();

    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _) = vault_pda(&user.pubkey(), &program_id);
    let dummies = [Pubkey::new_unique(); 5];

    // 0-leg rebalance: tag 6 + leg_count 0 + no leg bytes.
    let data = vec![6u8, 0u8];

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(attacker.pubkey(), true), // wrong keeper
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(dummies[0], false), // clock
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
        ],
        data,
    };
    let res = send_signed(&mut svm, ix, &attacker);
    assert_custom_error(res, PortfolioError::WrongKeeper as u32);
}

#[test]
fn rebalance_rejects_when_paused() {
    let (mut svm, program_id, user) = fresh_env();
    let keeper_kp = Keypair::new();
    svm.airdrop(&keeper_kp.pubkey(), 1_000_000_000).unwrap();
    send_init(&mut svm, program_id, &user, 200, 50_000, keeper_kp.pubkey()).unwrap();

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

    let (_, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _) = vault_pda(&user.pubkey(), &program_id);
    let dummies = [Pubkey::new_unique(); 5];

    let data = vec![6u8, 0u8];
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(keeper_kp.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(dummies[0], false),
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
        ],
        data,
    };
    let res = send_signed(&mut svm, ix, &keeper_kp);
    assert_custom_error(res, PortfolioError::Paused as u32);
}

#[test]
fn rebalance_account_count_must_match_leg_count() {
    // leg_count = 1 → expected accounts = BASE(7) + PER_LEG(6) = 13
    // We pass 7 (no leg accounts at all). Must reject.
    let (mut svm, program_id, user) = fresh_env();
    let keeper_kp = Keypair::new();
    svm.airdrop(&keeper_kp.pubkey(), 1_000_000_000).unwrap();
    send_init(&mut svm, program_id, &user, 200, 50_000, keeper_kp.pubkey()).unwrap();

    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _) = vault_pda(&user.pubkey(), &program_id);
    let dummies = [Pubkey::new_unique(); 5];

    // 1 leg encoded but only 7 accounts passed.
    let mut data = vec![6u8, 1u8];
    data.extend_from_slice(&0u16.to_le_bytes()); // from_idx
    data.extend_from_slice(&1u16.to_le_bytes()); // to_idx
    data.extend_from_slice(&100u64.to_le_bytes()); // amount

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(keeper_kp.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(dummies[0], false),
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
        ],
        data,
    };
    let res = send_signed(&mut svm, ix, &keeper_kp);
    assert_custom_error(res, PortfolioError::BadAccountCount as u32);
}

#[test]
fn emergency_close_rejects_unenrolled_market() {
    // EmergencyClose now requires a fully-initialised vault (because it
    // sends released collateral through portfolio_vault → user_ata) AND
    // the 12-account layout. Without a real vault we can't reach the
    // enrolment check unless we patch vault_bump directly. Patch it,
    // pass the canonical percolator program, and assert MarketNotEnrolled
    // fires before the CPI.
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();

    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, vbump) = vault_pda(&user.pubkey(), &program_id);

    // Patch vault_bump (offset 112) to satisfy the vault_bump != 0 guard.
    let mut acct = svm.get_account(&data_pda).unwrap();
    acct.data[112] = vbump;
    acct.data[116] = 1; // has_vault (CRIT-3 fix)
    svm.set_account(data_pda, acct).unwrap();

    let dummies = [Pubkey::new_unique(); 8];
    let mut data = vec![7u8];
    data.extend_from_slice(&0u16.to_le_bytes());

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new(dummies[0], false), // user_ata
            AccountMeta::new(dummies[1], false), // unknown_slab
            AccountMeta::new(dummies[2], false), // mvault
            AccountMeta::new_readonly(dummies[3], false), // mvault_auth
            AccountMeta::new_readonly(dummies[4], false), // oracle
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(dummies[5], false), // clock
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
        ],
        data,
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::MarketNotEnrolled as u32);
}

#[test]
fn emergency_close_rejects_wrong_account_count() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();

    let mut data = vec![7u8];
    data.extend_from_slice(&0u16.to_le_bytes());

    // Pass 5 accounts (need 12).
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
        ],
        data,
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadAccountCount as u32);
}

#[test]
fn deposit_rejects_unenrolled_market_after_vault_init() {
    // To reach the "market not enrolled" check we need a working vault.
    // We can't easily init the vault here without a real mint, so we
    // manually patch the vault_bump in the data PDA and verify that the
    // wrapper STILL rejects on the enrolment check.
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();

    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, vbump) = vault_pda(&user.pubkey(), &program_id);

    // Patch vault_bump to bypass the "vault not initialised" guard.
    let mut acct = svm.get_account(&data_pda).unwrap();
    // vault_bump field offset = 112 (after bump=110, auth_bump=111).
    acct.data[112] = vbump;
    acct.data[116] = 1; // has_vault (CRIT-3 fix)
    svm.set_account(data_pda, acct).unwrap();

    let dummies = [Pubkey::new_unique(); 7];
    let unknown_slab = dummies[1];

    let mut data = vec![3u8];
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&100u64.to_le_bytes());

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new_readonly(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new(dummies[0], false),
            AccountMeta::new(unknown_slab, false),
            AccountMeta::new(dummies[2], false),
            AccountMeta::new_readonly(dummies[3], false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(dummies[4], false),
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
        ],
        data,
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::MarketNotEnrolled as u32);
}

#[test]
fn init_vault_happy_path_actually_creates_vault() {
    // The single happy-path test in this file. Builds a real SPL mint in
    // LiteSVM, calls InitVault, and verifies the resulting token account
    // is correctly owned by SPL Token, has the right mint, has zero
    // balance, has its `account.owner` set to the portfolio_auth PDA,
    // and that the portfolio's `vault_bump` field is populated.
    use solana_sdk::{
        program_pack::Pack, system_instruction,
        sysvar::rent::Rent,
    };

    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();

    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _vbump) = vault_pda(&user.pubkey(), &program_id);

    // Create a brand-new mint owned by SPL Token. Two-step:
    //   (1) system_instruction::create_account at a fresh keypair address
    //   (2) spl_token::instruction::initialize_mint2 to set decimals/auth
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
        &user.pubkey(), // mint authority
        None,           // freeze authority
        6,              // decimals (USDC convention)
    )
    .unwrap();
    let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
        &[create_mint, init_mint],
        Some(&user.pubkey()),
        &[&user, &mint_kp],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).expect("mint creation");

    // Now call InitVault.
    svm.expire_blockhash();
    let ix = init_vault_ix(program_id, user.pubkey(), data_pda, auth_pda, vault, mint_kp.pubkey());
    send_signed(&mut svm, ix, &user).expect("init_vault must succeed");

    // Verify the vault token account is real.
    let vault_acct = svm.get_account(&vault).expect("vault must exist");
    assert_eq!(
        vault_acct.owner, SPL_TOKEN,
        "vault is owned by the SPL Token program"
    );
    assert_eq!(
        vault_acct.data.len(),
        spl_token::state::Account::LEN,
        "vault has SPL Token Account size (165)"
    );

    // Decode the SPL Token account state.
    let token_state = spl_token::state::Account::unpack(&vault_acct.data)
        .expect("vault data is a valid SPL Token Account");
    assert_eq!(token_state.mint, mint_kp.pubkey(), "vault mint set");
    assert_eq!(token_state.owner, auth_pda, "vault owner = portfolio_auth PDA");
    assert_eq!(token_state.amount, 0, "vault balance starts at 0");

    // Verify the portfolio's vault_bump was populated.
    let pa = read_portfolio(&svm, &data_pda);
    assert_ne!(pa.vault_bump, 0, "vault_bump must be set after InitVault");
}

// ── Phase-1 critical-bug regression tests ─────────────────────────────────
//
// Each of these locks in a guard added in v0.2 against a real bug in v0.1.
// See README "Phase 1 — Critical bugs" section for the full audit context.

#[test]
fn deposit_rejects_fake_percolator_program() {
    // P-CRITICAL: passing a substituted executable account as
    // a_percolator_prog must be rejected with `BadProgram`. v0.1 would
    // have routed Deposit's CPI to the attacker's program.
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();

    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, vbump) = vault_pda(&user.pubkey(), &program_id);
    // Patch vault_bump to bypass the not-init guard.
    let mut acct = svm.get_account(&data_pda).unwrap();
    acct.data[112] = vbump;
    acct.data[116] = 1; // has_vault (CRIT-3 fix)
    svm.set_account(data_pda, acct).unwrap();

    let fake_program = Pubkey::new_unique(); // NOT the canonical percolator
    let dummies = [Pubkey::new_unique(); 6];

    let mut data = vec![3u8];
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&100u64.to_le_bytes());

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new_readonly(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new(dummies[0], false), // user_ata
            AccountMeta::new(dummies[1], false), // slab
            AccountMeta::new(dummies[2], false), // mvault
            AccountMeta::new_readonly(dummies[3], false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(dummies[4], false),
            AccountMeta::new_readonly(fake_program, false), // <-- attack
        ],
        data,
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadProgram as u32);
}

#[test]
fn deposit_rejects_zero_amount() {
    // P-MEDIUM: amount==0 was previously allowed through to percolator-prog
    // which rejects with a generic InvalidArgument. Now caught earlier
    // with `ZeroAmount`.
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();

    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _) = vault_pda(&user.pubkey(), &program_id);
    let dummies = [Pubkey::new_unique(); 6];

    let mut data = vec![3u8];
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes()); // amount = 0

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new_readonly(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new(dummies[0], false),
            AccountMeta::new(dummies[1], false),
            AccountMeta::new(dummies[2], false),
            AccountMeta::new_readonly(dummies[3], false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(dummies[4], false),
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
        ],
        data,
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::ZeroAmount as u32);
}

#[test]
fn withdraw_rejects_zero_amount() {
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();

    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _) = vault_pda(&user.pubkey(), &program_id);
    let dummies = [Pubkey::new_unique(); 7];

    let mut data = vec![4u8];
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes()); // amount = 0

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new_readonly(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new(dummies[0], false),
            AccountMeta::new(dummies[1], false),
            AccountMeta::new(dummies[2], false),
            AccountMeta::new_readonly(dummies[3], false),
            AccountMeta::new_readonly(dummies[4], false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(dummies[5], false),
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
        ],
        data,
    };
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::ZeroAmount as u32);
}

#[test]
fn rebalance_rejects_too_many_legs_at_decode() {
    // P-CRITICAL: the decoder now rejects leg_count > MAX_REBALANCE_LEGS.
    // This is the first line of defence — even malformed ix data with
    // mismatched body length doesn't reach the processor.
    let (mut svm, program_id, user) = fresh_env();
    let keeper = Keypair::new();
    svm.airdrop(&keeper.pubkey(), 1_000_000_000).unwrap();
    send_init(&mut svm, program_id, &user, 200, 50_000, keeper.pubkey()).unwrap();

    // 5 legs > MAX_REBALANCE_LEGS = 4. Body length = 1 + 5*12 = 61 bytes.
    let mut data = vec![6u8, 5u8];
    for _ in 0..5 {
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&100u64.to_le_bytes());
    }

    let ix = Instruction {
        program_id,
        // Don't bother with full account list — decoder rejects first.
        accounts: vec![AccountMeta::new(keeper.pubkey(), true)],
        data,
    };
    let res = send_signed(&mut svm, ix, &keeper);
    assert_custom_error(res, PortfolioError::TooManyLegs as u32);
}

#[test]
fn rebalance_rejects_zero_amount_leg() {
    // Per-leg amount==0 is rejected. Stops a misbehaving keeper from
    // wasting CU and stamping last_rebalance_slot without doing real work.
    let (mut svm, program_id, user) = fresh_env();
    let keeper = Keypair::new();
    svm.airdrop(&keeper.pubkey(), 1_000_000_000).unwrap();
    send_init(&mut svm, program_id, &user, 200, 50_000, keeper.pubkey()).unwrap();

    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, vbump) = vault_pda(&user.pubkey(), &program_id);
    // Patch vault_bump and pre-enrol two markets so we reach the leg loop.
    let mut acct = svm.get_account(&data_pda).unwrap();
    acct.data[112] = vbump;
    acct.data[116] = 1; // has_vault (CRIT-3 fix)
    svm.set_account(data_pda, acct).unwrap();

    let m1 = Pubkey::new_unique();
    let m2 = Pubkey::new_unique();
    for (i, m) in [m1, m2].iter().enumerate() {
        svm.expire_blockhash();
        let mut d = vec![1u8];
        d.extend_from_slice(&(i as u16).to_le_bytes());
        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(user.pubkey(), true),
                AccountMeta::new(data_pda, false),
                AccountMeta::new_readonly(*m, false),
            ],
            data: d,
        };
        send_signed(&mut svm, ix, &user).unwrap();
    }

    let dummies = [Pubkey::new_unique(); 7];

    // 1-leg rebalance with amount=0.
    let mut data = vec![6u8, 1u8];
    data.extend_from_slice(&0u16.to_le_bytes()); // from_idx
    data.extend_from_slice(&1u16.to_le_bytes()); // to_idx
    data.extend_from_slice(&0u64.to_le_bytes()); // amount = 0

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(keeper.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
            AccountMeta::new_readonly(dummies[0], false),
            AccountMeta::new_readonly(PERCOLATOR_PROG, false),
            // leg accounts:
            AccountMeta::new(m1, false),
            AccountMeta::new(dummies[1], false),
            AccountMeta::new_readonly(dummies[2], false),
            AccountMeta::new_readonly(dummies[3], false),
            AccountMeta::new(m2, false),
            AccountMeta::new(dummies[4], false),
        ],
        data,
    };
    svm.expire_blockhash();
    let res = send_signed(&mut svm, ix, &keeper);
    assert_custom_error(res, PortfolioError::ZeroAmount as u32);
}

#[test]
fn portfolio_account_layout_has_vault_bump_at_offset_112() {
    // Sanity test that pins the layout — if this ever drifts (e.g., a
    // new field shifts vault_bump), we want to know IMMEDIATELY because
    // every Deposit/Withdraw/Rebalance handler depends on it.
    //
    // CRIT-3 fix: has_vault is the load-bearing "vault initialised"
    // sentinel at offset 116. vault_bump (offset 112) is now just the
    // canonical PDA bump and can legitimately be 0 post-InitVault for
    // ~1/256 users.
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();

    let (data_pda, _, _, _) = pdas_for(&user.pubkey(), &program_id);
    let pa = read_portfolio(&svm, &data_pda);
    assert_eq!(pa.vault_bump, 0, "vault_bump zero post-InitPortfolio");
    assert_eq!(pa.has_vault, 0, "has_vault zero pre-InitVault");

    // Verify byte-level: offset 112 is vault_bump, offset 116 is has_vault.
    let acct = svm.get_account(&data_pda).unwrap();
    assert_eq!(acct.data[112], 0, "byte at offset 112 is vault_bump (zero)");
    assert_eq!(acct.data[116], 0, "byte at offset 116 is has_vault (zero)");
    // And bump (offset 110) and auth_bump (offset 111) are non-zero
    // (they're real find_program_address bumps which start from 255).
    assert!(acct.data[110] > 0, "bump non-zero");
    assert!(acct.data[111] > 0, "auth_bump non-zero");
}
