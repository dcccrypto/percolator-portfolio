//! Shared test infrastructure.
//!
//! The most important thing this module provides is `assert_custom_error` —
//! the path test code uses to verify that a rejection landed on the SPECIFIC
//! `PortfolioError` we expected, not just any error.
//!
//! Vacuous test pattern (banned):
//!
//! ```rust,ignore
//! let err = env.send(ix).expect_err("must fail");
//! assert!(!err.is_empty()); // <- always true, says nothing
//! ```
//!
//! Required pattern:
//!
//! ```rust,ignore
//! let res = env.send_raw(ix);
//! assert_custom_error(res, PortfolioError::BadOwner as u32);
//! ```

#![allow(dead_code)]

pub mod integration_env;

use litesvm::{types::FailedTransactionMetadata, LiteSVM};
use percolator_portfolio::cpi as cpi_helpers;
use solana_sdk::{
    account::Account,
    instruction::{AccountMeta, Instruction, InstructionError},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_program,
    transaction::{Transaction, TransactionError},
};

/// Canonical percolator program ID. Used by `percolator_owned_slab` to
/// allocate a placeholder slab account whose `owner` matches the wrapper's
/// `verify_percolator_slab` guard. Tests that exercise wrapper-only paths
/// (enroll, trade, rebalance_crank, etc.) without a real engine slab must
/// allocate the slab via this helper so the owner check fires correctly
/// before any soft-decode branch.
pub const PERCOLATOR_PROG: Pubkey = Pubkey::new_from_array(cpi_helpers::PERCOLATOR_PROGRAM_ID);

/// Allocate a placeholder slab account at `slab` whose `owner` is the
/// canonical percolator program. Used by wrapper-only tests that just need
/// the slab address to satisfy `verify_percolator_slab` without setting up
/// a fully-encoded engine slab. The data is zero-length so any subsequent
/// `engine_ref` / `read_config` decode in the wrapper will short-circuit
/// through its soft-decode branch (matching the pre-fix test behaviour for
/// "slab is not yet a real market").
pub fn percolator_owned_slab(svm: &mut LiteSVM, slab: Pubkey) {
    svm.set_account(
        slab,
        Account {
            lamports: 1_000_000,
            data: vec![],
            owner: PERCOLATOR_PROG,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

pub const PROGRAM_SO: &str = "target/deploy/percolator_portfolio.so";

/// Set up a fresh LiteSVM with the program loaded and a funded user.
pub fn fresh_env() -> (LiteSVM, Pubkey, Keypair) {
    let program_id = percolator_portfolio::id();
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(program_id, PROGRAM_SO).unwrap();
    let user = Keypair::new();
    svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    (svm, program_id, user)
}

/// Derive both PDAs for a user.
pub fn pdas_for(user_pubkey: &Pubkey, program_id: &Pubkey) -> (Pubkey, u8, Pubkey, u8) {
    let (data, dbump) = Pubkey::find_program_address(
        &[
            percolator_portfolio::constants::PORTFOLIO_SEED,
            user_pubkey.as_ref(),
        ],
        program_id,
    );
    let (auth, abump) = Pubkey::find_program_address(
        &[
            percolator_portfolio::constants::PORTFOLIO_AUTH_SEED,
            user_pubkey.as_ref(),
        ],
        program_id,
    );
    (data, dbump, auth, abump)
}

/// Encode + send InitPortfolio. Returns the raw `TransactionResult` so the
/// caller can pin the exact rejection path.
pub fn send_init(
    svm: &mut LiteSVM,
    program_id: Pubkey,
    user: &Keypair,
    buffer_bps: u16,
    max_lev_bps: u32,
    keeper: Pubkey,
) -> Result<(), FailedTransactionMetadata> {
    let (data_pda, _, auth_pda, _) = pdas_for(&user.pubkey(), &program_id);

    let mut data = vec![0u8];
    data.extend_from_slice(&buffer_bps.to_le_bytes());
    data.extend_from_slice(&max_lev_bps.to_le_bytes());
    data.extend_from_slice(keeper.as_ref());

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
    send_signed(svm, ix, user)
}

/// Send a single ix signed by `signer`. Returns the raw result.
pub fn send_signed(
    svm: &mut LiteSVM,
    ix: Instruction,
    signer: &Keypair,
) -> Result<(), FailedTransactionMetadata> {
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&signer.pubkey()),
        &[signer],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).map(|_| ())
}

/// Assert that the result is a custom program error with the given code.
///
/// This is THE rigorous rejection check. It fails if:
///   - the call succeeded (caller expected a rejection),
///   - the error wasn't a custom program error (e.g., it was a runtime
///     error from the transaction layer or a different InstructionError
///     variant),
///   - the custom code didn't match.
///
/// `expected_code` is the `u32` discriminant from `PortfolioError`.
#[track_caller]
pub fn assert_custom_error(
    result: Result<(), FailedTransactionMetadata>,
    expected_code: u32,
) {
    match result {
        Ok(()) => panic!("expected program error {expected_code}, got Ok"),
        Err(meta) => match meta.err {
            TransactionError::InstructionError(_, InstructionError::Custom(code)) => {
                assert_eq!(
                    code, expected_code,
                    "expected program error {expected_code}, got Custom({code}); \
                     full err: {:?}",
                    meta.err
                );
            }
            other => panic!(
                "expected program error Custom({expected_code}), got {other:?}"
            ),
        },
    }
}

/// Looser assertion: error is from the transaction layer (e.g., system program
/// rejected create_account because account is already initialised). Use ONLY
/// when the failure path goes through a CPI into a non-percolator program.
#[track_caller]
pub fn assert_any_error(result: Result<(), FailedTransactionMetadata>) {
    if result.is_ok() {
        panic!("expected error, got Ok");
    }
}
