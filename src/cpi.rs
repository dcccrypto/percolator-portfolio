//! CPI helpers — builders for `percolator-prog` and `spl-token` instructions
//! that this program issues during Deposit / Withdraw / Trade / Rebalance /
//! EmergencyClose.
//!
//! Each builder returns a `solana_program::instruction::Instruction` so the
//! caller can pass it to `invoke` or `invoke_signed`. Instruction tags and
//! account orders mirror `~/percolator-prog/src/percolator.rs`.
//!
//! NOTE: these builders DO NOT verify accounts or perform CPIs themselves —
//! they're pure encoders. The processor module is responsible for
//! constructing the AccountInfo slice and choosing the right invoke variant.

#![allow(dead_code)]

use alloc::vec;
use alloc::vec::Vec;
use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};

/// `percolator-prog::DepositCollateral` (tag 3).
///
/// Body: `u16 user_idx || u64 amount` (10 bytes).
///
/// Accounts (per `~/percolator-prog/src/percolator.rs:5954`):
///   0. `[signer]`   user (= engine.account.owner)
///   1. `[writable]` slab
///   2. `[]`         user_ata (token account holding the source funds)
///   3. `[writable]` market vault
///   4. `[]`         token program
///   5. `[]`         clock sysvar
pub fn percolator_deposit_collateral(
    program_id: Pubkey,
    user_signer: Pubkey,
    slab: Pubkey,
    source_ata: Pubkey,
    market_vault: Pubkey,
    token_program: Pubkey,
    clock_sysvar: Pubkey,
    user_idx: u16,
    amount: u64,
) -> Instruction {
    let mut data = vec![3u8];
    data.extend_from_slice(&user_idx.to_le_bytes());
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user_signer, true),
            AccountMeta::new(slab, false),
            AccountMeta::new(source_ata, false),
            AccountMeta::new(market_vault, false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(clock_sysvar, false),
        ],
        data,
    }
}

/// `percolator-prog::WithdrawCollateral` (tag 4).
///
/// Body: `u16 user_idx || u64 amount` (10 bytes).
///
/// Accounts (per `~/percolator-prog/src/percolator.rs:8139`):
///   0. `[signer]`   user (= engine.account.owner)
///   1. `[writable]` slab
///   2. `[writable]` market vault
///   3. `[writable]` dest_ata (where withdrawn funds land)
///   4. `[]`         vault authority PDA (engine-owned)
///   5. `[]`         token program
///   6. `[]`         clock sysvar
///   7. `[]`         oracle (price feed for the market)
pub fn percolator_withdraw_collateral(
    program_id: Pubkey,
    user_signer: Pubkey,
    slab: Pubkey,
    market_vault: Pubkey,
    dest_ata: Pubkey,
    market_vault_authority: Pubkey,
    token_program: Pubkey,
    clock_sysvar: Pubkey,
    oracle: Pubkey,
    user_idx: u16,
    amount: u64,
) -> Instruction {
    let mut data = vec![4u8];
    data.extend_from_slice(&user_idx.to_le_bytes());
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user_signer, true),
            AccountMeta::new(slab, false),
            AccountMeta::new(market_vault, false),
            AccountMeta::new(dest_ata, false),
            AccountMeta::new_readonly(market_vault_authority, false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(clock_sysvar, false),
            AccountMeta::new_readonly(oracle, false),
        ],
        data,
    }
}

/// `percolator-prog::CloseAccount` (tag 8).
///
/// Body: `u16 user_idx` (2 bytes).
///
/// Accounts (verified against `handle_close_account` in
/// `~/percolator-prog/src/percolator.rs` — exactly 8 accounts):
///   0. `[signer]`   user (= engine.account.owner)
///   1. `[writable]` slab
///   2. `[writable]` market vault (returns residual collateral)
///   3. `[writable]` dest_ata (where the closed position's collateral lands)
///   4. `[]`         vault authority PDA (= derive_vault_authority(slab))
///   5. `[]`         token program
///   6. `[]`         clock sysvar
///   7. `[]`         oracle (price feed)
///
/// Used by EmergencyClose to flatten + close a per-market position. Note:
/// the v0.1.0 builder shipped 2 accounts; that was a P-CRITICAL bug
/// because percolator-prog's handler unconditionally `accounts::expect_len(_, 8)`.
pub fn percolator_close_account(
    program_id: Pubkey,
    user_signer: Pubkey,
    slab: Pubkey,
    market_vault: Pubkey,
    dest_ata: Pubkey,
    market_vault_authority: Pubkey,
    token_program: Pubkey,
    clock_sysvar: Pubkey,
    oracle: Pubkey,
    user_idx: u16,
) -> Instruction {
    let mut data = vec![8u8];
    data.extend_from_slice(&user_idx.to_le_bytes());
    Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user_signer, true),
            AccountMeta::new(slab, false),
            AccountMeta::new(market_vault, false),
            AccountMeta::new(dest_ata, false),
            AccountMeta::new_readonly(market_vault_authority, false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(clock_sysvar, false),
            AccountMeta::new_readonly(oracle, false),
        ],
        data,
    }
}

/// `percolator-prog::InitUser` (tag 1).
///
/// Body: `u64 fee_payment` (8 bytes).
///
/// Accounts (per `~/percolator-prog/src/percolator.rs:7873`):
///   0. `[signer]`   user (becomes engine.account.owner)
///   1. `[writable]` slab
///   2. `[writable]` user_ata (source for fee_payment)
///   3. `[writable]` market vault
///   4. `[]`         token program
///   5. `[]`         clock sysvar
pub fn percolator_init_user(
    program_id: Pubkey,
    user_signer: Pubkey,
    slab: Pubkey,
    source_ata: Pubkey,
    market_vault: Pubkey,
    token_program: Pubkey,
    clock_sysvar: Pubkey,
    fee_payment: u64,
) -> Instruction {
    let mut data = vec![1u8];
    data.extend_from_slice(&fee_payment.to_le_bytes());
    Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user_signer, true),
            AccountMeta::new(slab, false),
            AccountMeta::new(source_ata, false),
            AccountMeta::new(market_vault, false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(clock_sysvar, false),
        ],
        data,
    }
}

/// `percolator-prog::TradeCpi` (tag 10).
///
/// Body: `u16 lp_idx || u16 user_idx || i128 size || u64 limit_price_e6` (28 bytes).
///
/// Accounts (per `~/percolator-prog/src/percolator.rs:7092`):
///   0. `[signer]`   user (= engine.account.owner) — wrapper signs as portfolio_auth PDA
///   1. `[]`         lp_owner (non-signer; matcher delegates auth)
///   2. `[writable]` slab
///   3. `[]`         clock sysvar
///   4. `[]`         oracle
///   5. `[]`         matcher_program
///   6. `[writable]` matcher_context
///   7. `[]`         lp_pda
///   8..N            VARIADIC tail forwarded to matcher CPI verbatim
pub fn percolator_trade_cpi(
    program_id: Pubkey,
    user_signer: Pubkey,
    lp_owner: Pubkey,
    slab: Pubkey,
    clock_sysvar: Pubkey,
    oracle: Pubkey,
    matcher_program: Pubkey,
    matcher_context: Pubkey,
    lp_pda: Pubkey,
    tail: &[AccountMeta],
    lp_idx: u16,
    user_idx: u16,
    size: i128,
    limit_price_e6: u64,
) -> Instruction {
    let mut data = vec![10u8];
    data.extend_from_slice(&lp_idx.to_le_bytes());
    data.extend_from_slice(&user_idx.to_le_bytes());
    data.extend_from_slice(&size.to_le_bytes());
    data.extend_from_slice(&limit_price_e6.to_le_bytes());
    let mut accounts = vec![
        AccountMeta::new_readonly(user_signer, true),
        AccountMeta::new_readonly(lp_owner, false),
        AccountMeta::new(slab, false),
        AccountMeta::new_readonly(clock_sysvar, false),
        AccountMeta::new_readonly(oracle, false),
        AccountMeta::new_readonly(matcher_program, false),
        AccountMeta::new(matcher_context, false),
        AccountMeta::new_readonly(lp_pda, false),
    ];
    accounts.extend_from_slice(tail);
    Instruction {
        program_id,
        accounts,
        data,
    }
}

/// SPL Token Program ID (`TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`).
/// Hardcoded here so we don't need an spl-token import in the processor's
/// hot paths; the wrapper only needs to verify byte equality.
pub const SPL_TOKEN_ID: [u8; 32] = [
    6, 221, 246, 225, 215, 101, 161, 147, 217, 203, 225, 70, 206, 235, 121, 172, 28, 180, 133,
    237, 95, 91, 55, 145, 58, 140, 245, 133, 126, 255, 0, 169,
];

/// `percolator-prog` program ID (`ESa89R5Es3rJ5mnwGybVRG1GrNt9etP11Z5V2QWD4edv`).
/// Hardcoded so the processor can verify the caller-supplied executable
/// account is actually percolator-prog, not an attacker-substituted
/// program. This was a P-CRITICAL gap in v0.1.0 — without this check,
/// passing a fake `a_percolator_prog` would route every Deposit/Withdraw/
/// Rebalance/EmergencyClose CPI to the attacker's program.
///
/// Source: `~/percolator-prog/src/percolator.rs:40`.
/// Verified: bytes match `Pubkey::from_str("ESa89R5...").to_bytes()`.
pub const PERCOLATOR_PROGRAM_ID: [u8; 32] = [
    199, 180, 231, 28, 169, 127, 223, 6, 13, 64, 223, 31, 229, 237, 183, 2, 182, 201, 78, 87,
    235, 180, 40, 205, 87, 58, 38, 116, 241, 27, 43, 41,
];

/// SPL Token `Transfer` (tag 3 in spl-token v3 / v4).
///
/// Body: `u8 tag || u64 amount` (9 bytes).
///
/// Accounts:
///   0. `[writable]` source
///   1. `[writable]` destination
///   2. `[signer]`   authority
pub fn spl_token_transfer(
    source: Pubkey,
    destination: Pubkey,
    authority: Pubkey,
    amount: u64,
) -> Instruction {
    // SPL Token v3 Transfer instruction: tag=3, then u64 amount.
    let mut data = vec![3u8];
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: Pubkey::new_from_array(SPL_TOKEN_ID),
        accounts: vec![
            AccountMeta::new(source, false),
            AccountMeta::new(destination, false),
            AccountMeta::new_readonly(authority, true),
        ],
        data,
    }
}

/// SPL Token `InitializeAccount3` (tag 18). Initialises a token account
/// in-place — does NOT require a rent sysvar account.
///
/// Body: `u8 tag || [u8; 32] owner` (33 bytes).
///
/// Accounts:
///   0. `[writable]` account to initialise
///   1. `[]`         mint
pub fn spl_token_initialize_account3(account: Pubkey, mint: Pubkey, owner: Pubkey) -> Instruction {
    let mut data = vec![18u8];
    data.extend_from_slice(owner.as_ref());
    Instruction {
        program_id: Pubkey::new_from_array(SPL_TOKEN_ID),
        accounts: vec![
            AccountMeta::new(account, false),
            AccountMeta::new_readonly(mint, false),
        ],
        data,
    }
}

/// SPL Token `CloseAccount` (tag 9). Used by EmergencyClose's vault
/// drain path if a market account no longer needs its split.
///
/// Body: `u8 tag` (1 byte).
///
/// Accounts:
///   0. `[writable]` account to close
///   1. `[writable]` destination (lamports)
///   2. `[signer]`   authority
pub fn spl_token_close_account(
    account: Pubkey,
    destination: Pubkey,
    authority: Pubkey,
) -> Instruction {
    Instruction {
        program_id: Pubkey::new_from_array(SPL_TOKEN_ID),
        accounts: vec![
            AccountMeta::new(account, false),
            AccountMeta::new(destination, false),
            AccountMeta::new_readonly(authority, true),
        ],
        data: vec![9u8],
    }
}

/// Re-encode a u16 leg list for `Rebalance`. The on-chain decoder consumes
/// `body[0] = leg_count`, then `leg_count × (u16 from_idx, u16 to_idx, u64
/// amount)`. This helper packs the payload for use by tests / off-chain
/// keeper clients.
pub fn encode_rebalance_legs(legs: &[(u16, u16, u64)]) -> Vec<u8> {
    assert!(legs.len() <= u8::MAX as usize, "leg_count overflows u8");
    let mut out = vec![6u8, legs.len() as u8];
    for (from, to, amount) in legs {
        out.extend_from_slice(&from.to_le_bytes());
        out.extend_from_slice(&to.to_le_bytes());
        out.extend_from_slice(&amount.to_le_bytes());
    }
    out
}
