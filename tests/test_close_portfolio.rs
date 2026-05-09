//! Integration tests for `ClosePortfolio` (tag 11).
//!
//! Each happy-path test verifies actual rent reclamation by reading
//! lamport balances. Rejection tests assert the specific PortfolioError.

mod common;

use bytemuck::from_bytes;
use common::{assert_custom_error, fresh_env, pdas_for, send_init, send_signed};
use percolator_portfolio::{
    constants::PORTFOLIO_VAULT_SEED, cpi as cpi_helpers, errors::PortfolioError,
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

fn vault_pda(user: &Pubkey, program_id: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[PORTFOLIO_VAULT_SEED, user.as_ref()], program_id)
}

fn close_ix(
    program_id: Pubkey,
    user: Pubkey,
    data_pda: Pubkey,
    auth_pda: Pubkey,
    vault: Pubkey,
) -> Instruction {
    Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user, true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
        ],
        data: vec![11u8],
    }
}

/// Helper: create a real SPL mint and call InitVault so the user has a
/// fully-initialised, empty vault.
fn init_full_portfolio(
    svm: &mut litesvm::LiteSVM,
    program_id: Pubkey,
    user: &Keypair,
) -> (Pubkey, Pubkey, Pubkey, Pubkey) {
    send_init(svm, program_id, user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    let (vault, _) = vault_pda(&user.pubkey(), &program_id);

    // Create the mint.
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

    // Init the vault.
    svm.expire_blockhash();
    let init_vault = Instruction {
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
    send_signed(svm, init_vault, user).unwrap();

    (data_pda, auth_pda, vault, mint_kp.pubkey())
}

#[test]
fn close_portfolio_happy_path_reclaims_rent() {
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, auth_pda, vault, _mint) = init_full_portfolio(&mut svm, program_id, &user);

    // Snapshot lamports.
    let user_before = svm.get_account(&user.pubkey()).unwrap().lamports;
    let data_lamports = svm.get_account(&data_pda).unwrap().lamports;
    let vault_lamports = svm.get_account(&vault).unwrap().lamports;
    assert!(data_lamports > 0, "data PDA must have rent");
    assert!(vault_lamports > 0, "vault must have rent");

    // Close.
    svm.expire_blockhash();
    send_signed(&mut svm, close_ix(program_id, user.pubkey(), data_pda, auth_pda, vault), &user)
        .expect("close_portfolio");

    // Both accounts should be lamport-zero (will be GC'd next epoch).
    let data_after = svm.get_account(&data_pda).unwrap_or_default();
    let vault_after = svm.get_account(&vault).unwrap_or_default();
    assert_eq!(data_after.lamports, 0, "data PDA lamports drained");
    assert_eq!(vault_after.lamports, 0, "vault lamports drained");

    // User received both rent refunds (minus tx fee).
    let user_after = svm.get_account(&user.pubkey()).unwrap().lamports;
    let recovered = user_after as i64 - user_before as i64;
    let expected_min = (data_lamports + vault_lamports) as i64 - 10_000; // tx fee budget
    assert!(
        recovered >= expected_min,
        "user should recover ~{} lamports, got delta {}",
        data_lamports + vault_lamports,
        recovered
    );

    // Data account should now be zeroed (defensive zero before reassign).
    if !data_after.data.is_empty() {
        for b in &data_after.data {
            assert_eq!(*b, 0, "data account zeroed post-close");
        }
    }
}

#[test]
fn close_portfolio_rejects_when_enrolled() {
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, auth_pda, vault, _mint) = init_full_portfolio(&mut svm, program_id, &user);

    // Enrol a market so enrolled_count == 1.
    let mut data = vec![1u8];
    data.extend_from_slice(&0u16.to_le_bytes());
    svm.expire_blockhash();
    send_signed(
        &mut svm,
        Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(user.pubkey(), true),
                AccountMeta::new(data_pda, false),
                AccountMeta::new_readonly(Pubkey::new_unique(), false),
            ],
            data,
        },
        &user,
    )
    .unwrap();

    // Now ClosePortfolio must reject.
    svm.expire_blockhash();
    let res = send_signed(
        &mut svm,
        close_ix(program_id, user.pubkey(), data_pda, auth_pda, vault),
        &user,
    );
    assert_custom_error(res, PortfolioError::TooManyEnrolled as u32);
}

#[test]
fn close_portfolio_rejects_when_paused() {
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, auth_pda, vault, _mint) = init_full_portfolio(&mut svm, program_id, &user);

    // Pause.
    svm.expire_blockhash();
    let pause_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
        ],
        data: vec![9u8, 1u8],
    };
    send_signed(&mut svm, pause_ix, &user).unwrap();

    // ClosePortfolio must refuse on paused.
    svm.expire_blockhash();
    let res = send_signed(
        &mut svm,
        close_ix(program_id, user.pubkey(), data_pda, auth_pda, vault),
        &user,
    );
    assert_custom_error(res, PortfolioError::Paused as u32);
}

#[test]
fn close_portfolio_rejects_when_vault_has_balance() {
    // Mint a few tokens directly to the vault and verify ClosePortfolio
    // refuses to close (would forfeit user's funds).
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, auth_pda, vault, mint) = init_full_portfolio(&mut svm, program_id, &user);

    // Mint 100 tokens to the vault.
    let mint_to_ix = spl_token::instruction::mint_to(
        &SPL_TOKEN,
        &mint,
        &vault,
        &user.pubkey(),
        &[],
        100,
    )
    .unwrap();
    svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[mint_to_ix],
        Some(&user.pubkey()),
        &[&user],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).expect("mint_to vault");

    // Now ClosePortfolio must refuse.
    svm.expire_blockhash();
    let res = send_signed(
        &mut svm,
        close_ix(program_id, user.pubkey(), data_pda, auth_pda, vault),
        &user,
    );
    assert_custom_error(res, PortfolioError::ZeroAmount as u32);
}

#[test]
fn close_portfolio_rejects_wrong_signer() {
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, auth_pda, vault, _mint) = init_full_portfolio(&mut svm, program_id, &user);

    let attacker = Keypair::new();
    svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(attacker.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
        ],
        data: vec![11u8],
    };
    let res = send_signed(&mut svm, ix, &attacker);
    assert_custom_error(res, PortfolioError::BadOwner as u32);
}

#[test]
fn close_portfolio_rejects_extra_data_byte() {
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, auth_pda, vault, _mint) = init_full_portfolio(&mut svm, program_id, &user);

    let mut bad_data = vec![11u8];
    bad_data.push(0xff); // non-empty body
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(auth_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(SPL_TOKEN, false),
        ],
        data: bad_data,
    };
    svm.expire_blockhash();
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadInstruction as u32);
}

#[test]
fn close_portfolio_rejects_wrong_account_count() {
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, _, _, _) = init_full_portfolio(&mut svm, program_id, &user);

    // Only 4 accounts (need 5).
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(data_pda, false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
        ],
        data: vec![11u8],
    };
    svm.expire_blockhash();
    let res = send_signed(&mut svm, ix, &user);
    assert_custom_error(res, PortfolioError::BadAccountCount as u32);
}

#[test]
fn close_portfolio_works_when_vault_never_initialised() {
    // If the user did InitPortfolio but never InitVault (vault_bump == 0),
    // ClosePortfolio should still succeed and reclaim the data PDA's rent.
    // The vault skip path is exercised here.
    let (mut svm, program_id, user) = fresh_env();
    send_init(&mut svm, program_id, &user, 200, 50_000, Pubkey::new_unique()).unwrap();
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);
    // We still pass *some* vault account because the ix expects 5 accounts;
    // the handler skips touching it when vault_bump == 0.
    let dummy_vault = Pubkey::new_unique();

    let user_before = svm.get_account(&user.pubkey()).unwrap().lamports;
    let data_lamports = svm.get_account(&data_pda).unwrap().lamports;

    svm.expire_blockhash();
    send_signed(
        &mut svm,
        close_ix(program_id, user.pubkey(), data_pda, auth_pda, dummy_vault),
        &user,
    )
    .expect("close_portfolio without vault should succeed");

    let user_after = svm.get_account(&user.pubkey()).unwrap().lamports;
    let recovered = user_after as i64 - user_before as i64;
    assert!(
        recovered >= data_lamports as i64 - 10_000,
        "user should recover at least the data PDA rent"
    );
}

#[test]
fn portfolio_reinit_after_close_succeeds() {
    // After ClosePortfolio drains the data PDA, a fresh InitPortfolio at
    // the same address should work — the runtime should treat the
    // zero-data, zero-lamport account as available for reuse.
    let (mut svm, program_id, user) = fresh_env();
    let (data_pda, auth_pda, vault, _mint) = init_full_portfolio(&mut svm, program_id, &user);
    svm.expire_blockhash();
    send_signed(&mut svm, close_ix(program_id, user.pubkey(), data_pda, auth_pda, vault), &user)
        .unwrap();

    // Try to InitPortfolio again. This should succeed (account is now
    // empty per `data_is_empty` check).
    svm.expire_blockhash();
    send_init(&mut svm, program_id, &user, 300, 60_000, Pubkey::new_unique())
        .expect("re-init should succeed after close");

    // Verify the new portfolio has the new params.
    let acct = svm.get_account(&data_pda).unwrap();
    let pa: &PortfolioAccount =
        from_bytes(&acct.data[..core::mem::size_of::<PortfolioAccount>()]);
    assert_eq!(pa.buffer_bps, 300);
    assert_eq!(pa.max_leverage_bps, 60_000);
    assert_eq!(pa.enrolled_count, 0);
}
