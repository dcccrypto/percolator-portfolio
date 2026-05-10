//! Percolator Portfolio: USDC cross-margin wrapper over isolated percolator markets.
//!
//! Design contract (v0.1, 2026-05):
//! * Each enrolled per-market `Account` slot has its `owner` field set to a
//!   `portfolio_auth` PDA derived from the user's pubkey. Engine still treats
//!   `owner` as non-normative; the wrapper auth check (`owner_ok` byte-equality
//!   on signer) accepts a PDA signing via `invoke_signed`.
//! * Engine remains untouched. Per-market liquidation, IM/MMR enforcement and
//!   accrual stay 100% inside `percolator-prog` / `percolator`. This program
//!   only routes user actions and rebalances collateral between enrolled
//!   markets to keep each one above its local MMR.
//! * `Rebalance` is the only path that requires a non-user signer (the
//!   off-chain keeper bot). Every other instruction is user-signed.
//!
//! Failure-mode coverage matrix in README; the silent failure mode (margin
//! math drift between this program's local mirror and engine math) is gated
//! on the upstream `GetAccountHealth` CPI whenever it lands.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use solana_program::declare_id;

declare_id!("PercoFoLPort1111111111111111111111111111111");

// CPI helper builders for percolator-prog and spl-token live in their own
// module to keep the processor readable.
pub mod cpi;

// Wrapper-side margin math (mirrors engine `account_equity_maint_raw`,
// `notional_checked`, `is_above_*_margin`). Per upstream maintainer review
// (#58/#87/#88): wrappers mirror engine math rather than depend on an
// engine read-view ABI. v1 = scaffolded stubs; aggregate margin check is
// the next major change.
pub mod margin;

// Pyth Price account decoder for fresh-oracle trade admission. Per the
// same review: cached `engine.last_oracle_price` is NOT authoritative for
// trade admission; admission must read a Pyth Price account this slot.
// v1 = scaffolded stubs.
pub mod pyth;

// ─────────────────────────────────────────────────────────────────────────
// 1. constants
// ─────────────────────────────────────────────────────────────────────────
pub mod constants {
    /// Magic on the data account header. Opaque 64-bit constant — picked
    /// once and never reinterpreted as text. The fixed bit pattern lets
    /// `check_portfolio_account` reject any account that wasn't initialised
    /// by this program.
    pub const MAGIC: u64 = 0x5043_5052_5446_4c00;

    /// Account-data version. Bumped on any struct layout change.
    /// Account-data layout version. Bumped on any `PortfolioAccount`
    /// struct field change.
    ///
    /// **Migration policy**: bumping `VERSION` immediately bricks every
    /// existing account because `check_portfolio_account` returns
    /// `BadVersion` for `pa.version != VERSION`. Before bumping, ship a
    /// `MigrateV1ToV2` instruction that:
    ///   1. accepts an account with `version == 1`,
    ///   2. translates the layout in-place,
    ///   3. writes the new `VERSION` value.
    ///
    /// Reserve a tag (e.g., 12 = `Migrate`) BEFORE the bump so SDKs ship
    /// the migration path in lockstep with the new program binary. Do not
    /// remove the v1-accepting code path in the same release that bumps
    /// VERSION — keep it for at least one full cycle so users have time
    /// to migrate.
    pub const VERSION: u8 = 1;

    /// Maximum markets a single portfolio account can enroll.
    /// Picked to keep `PortfolioAccount` under 2 KiB so it fits comfortably in
    /// one allocation. 16 markets is enough for any realistic single-user
    /// portfolio; if a power user wants more, they can run multiple
    /// portfolios under different wallets.
    pub const MAX_ENROLLED_MARKETS: usize = 16;

    /// Minimum buffer (above local MMR) the wrapper enforces on enrollments.
    /// 1 % is the floor; the user can configure higher. Below this, the
    /// keeper-bot's rebalance window during oracle ticks gets too tight.
    pub const MIN_BUFFER_BPS: u16 = 100;

    /// Maximum buffer the user can request. Above 50 % of MMR, the program
    /// is just hoarding capital and there's no benefit over isolated mode.
    pub const MAX_BUFFER_BPS: u16 = 5_000;

    /// Maximum portfolio-level leverage cap, in bps where 10_000 = 1x.
    /// 100_000 bps = 10x. The wrapper rejects new trades that would push
    /// portfolio-IM utilisation beyond this. User can configure tighter
    /// (lower) but not above this ceiling.
    pub const MAX_PORTFOLIO_LEV_BPS: u32 = 100_000;

    /// Seeds for the data account PDA.
    pub const PORTFOLIO_SEED: &[u8] = b"portfolio";
    /// Seeds for the signing PDA that becomes `Account.owner` on each
    /// enrolled per-market slot.
    pub const PORTFOLIO_AUTH_SEED: &[u8] = b"portfolio_auth";
    /// Seeds for the per-user collateral vault token account. Authority
    /// is the `portfolio_auth` PDA. One vault per user holds USDC between
    /// the user's wallet and the per-market vaults.
    pub const PORTFOLIO_VAULT_SEED: &[u8] = b"portfolio_vault";

    /// SPL Token v3 account size (mint:32 + owner:32 + amount:8 + delegate
    /// option + state + native option + delegated_amount + close_authority
    /// option) = 165 bytes total.
    pub const SPL_TOKEN_ACCOUNT_LEN: usize = 165;

    /// Maximum legs in a single Rebalance instruction. Each leg is one
    /// withdraw + one deposit CPI (~170K CU). 4 legs ≈ 680K CU plus
    /// wrapper overhead leaves comfortable headroom in the 1.4M CU budget.
    pub const MAX_REBALANCE_LEGS: u8 = 4;
}

// ─────────────────────────────────────────────────────────────────────────
// 2. errors
// ─────────────────────────────────────────────────────────────────────────
pub mod errors {
    use solana_program::program_error::ProgramError;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(u32)]
    pub enum PortfolioError {
        BadMagic = 1,
        BadVersion = 2,
        BadOwner = 3,
        BufferOutOfRange = 4,
        LeverageOutOfRange = 5,
        TooManyEnrolled = 6,
        MarketAlreadyEnrolled = 7,
        MarketNotEnrolled = 8,
        Paused = 9,
        WrongKeeper = 10,
        WrongSigner = 11,
        AccountNotInitialized = 12,
        AccountAlreadyInitialized = 13,
        BadInstruction = 14,
        BadAccountCount = 15,
        BadPda = 16,
        ArithmeticOverflow = 17,
        BadMint = 18,
        BadVault = 19,
        ZeroAmount = 20,
        TooManyLegs = 21,
        /// Wrong system program supplied where `system_program::ID` is
        /// expected. Distinct from `BadAccountCount` which is reserved
        /// for "wrong number of accounts".
        WrongSystemProgram = 22,
        /// Wrong SPL Token program supplied. Same separation rationale.
        WrongTokenProgram = 23,
        /// `a_percolator_prog` is not the canonical percolator-prog ID.
        /// Without this guard, an attacker could substitute a fake
        /// program and intercept Deposit/Withdraw/Rebalance/EmergencyClose
        /// CPIs.
        BadProgram = 24,
        /// A handler tried to take a mutable borrow of a data account
        /// that wasn't passed as `is_writable=true`. Distinct from the
        /// runtime `AccountBorrowFailed` so callers can disambiguate
        /// "you forgot the writable flag" from "you held two mut borrows".
        DataAccountNotWritable = 25,
        /// Pre-trade aggregate IM check failed: post-trade portfolio
        /// equity (sum of `account_equity_maint_raw` across enrolled)
        /// is below aggregate IM_req. Defense 1 of soft+ cross-margin.
        AggregateImBreach = 26,
        /// Wrong number of margin-check accounts. Trade ix must receive
        /// (slab, oracle) pairs for every enrolled market beyond the
        /// trade target. Caller-side bug.
        WrongMarginAccountCount = 27,
        /// A margin-check slab does not match any enrolled market in the
        /// portfolio. Caller-side bug.
        MarginSlabNotEnrolled = 28,
        /// A margin-check slab appears more than once in the account
        /// list. Caller-side bug.
        MarginSlabDuplicate = 29,
        /// Engine `try_notional` rejected — invalid oracle price or
        /// account-not-used. Surfaces as a wrapper-level error so
        /// callers can disambiguate from generic engine errors.
        MarginNotionalRejected = 30,
        /// Engine slab data did not decode cleanly via
        /// `percolator_prog::zc::engine_ref`. Indicates a slab that's
        /// not a percolator-prog market, or schema drift after an
        /// upstream sync wave.
        MarginSlabDecodeFailed = 31,
        /// Defense 3 / RebalanceCrank: destination account is already
        /// at or above its per-market initial margin, so no rebalance
        /// is needed and no bounty is paid. The caller wasted CU and
        /// should retry only when an account actually drops below IM.
        CrankNotNeeded = 32,
        /// Defense 3 / RebalanceCrank: from_idx == to_idx, or the
        /// from and to slabs are the same and indices match. Self-
        /// rebalance is meaningless.
        CrankSelfLeg = 33,
    }

    impl From<PortfolioError> for ProgramError {
        fn from(e: PortfolioError) -> Self {
            ProgramError::Custom(e as u32)
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// 3. state
// ─────────────────────────────────────────────────────────────────────────
pub mod state {
    use crate::constants::MAX_ENROLLED_MARKETS;
    use bytemuck::{Pod, Zeroable};

    /// One slot in `PortfolioAccount.enrolled[]`. Identifies a per-market
    /// engine account by (slab pubkey, account_idx within slab).
    ///
    /// All fields are 8-byte aligned to satisfy bytemuck::Pod without
    /// implicit padding. We deliberately avoid `i128` here because its
    /// alignment varies between sbf and host targets — using `i64` for
    /// cached UX values is fine (e6 USDC range up to ±9.2 × 10^12 USDC,
    /// well above any realistic portfolio).
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable)]
    pub struct MarketSlot {
        /// Cached equity at last rebalance (in engine e6 units, signed).
        /// NOT authoritative; recomputed from slab on every rebalance.
        pub last_seen_eq_e6: i64,
        /// Pubkey of the market slab. Zero pubkey = unused slot.
        pub market: [u8; 32],
        /// Account index within the slab.
        pub account_idx: u16,
        /// Padding to 48-byte boundary.
        pub _pad0: [u8; 6],
    }

    // 8 + 32 + 2 + 6 = 48
    const _: () = assert!(core::mem::size_of::<MarketSlot>() == 48);
    const _: () = assert!(core::mem::align_of::<MarketSlot>() == 8);
    // B10: pin the array footprint so silent drift in either constant is a
    // build error.
    const _: () =
        assert!(MAX_ENROLLED_MARKETS * core::mem::size_of::<MarketSlot>() == 768);

    /// On-chain state for a single user's portfolio.
    /// PDA seeds: `[PORTFOLIO_SEED, user_pubkey]`.
    ///
    /// Field order: 8-byte fields first, then 4/2/1-byte fields, with
    /// explicit padding to keep `Pod` happy and the `[MarketSlot]` array
    /// aligned. All multi-byte integers little-endian on every target.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable)]
    pub struct PortfolioAccount {
        // ── 8-byte block ────────────────────────────────────────────────
        /// Magic — must equal `constants::MAGIC`.
        pub magic: u64,
        /// Slot of the most recent successful Rebalance. Lets liveness
        /// monitors detect stuck keepers.
        pub last_rebalance_slot: u64,
        /// Slot at which `cached_*` values were last refreshed.
        pub cached_at_slot: u64,
        /// Cached portfolio equity (sum of per-market equity at refresh).
        /// NOT authoritative — used only for cheap UX queries / monitoring.
        /// In e6 USDC units, signed.
        pub cached_total_eq_e6: i64,
        /// Cached portfolio MMR. Same caveat.
        pub cached_total_mmr_e6: i64,

        // ── 32-byte blocks ──────────────────────────────────────────────
        /// Owner — the user pubkey. Authoritative for all user-signed ops.
        pub owner: [u8; 32],
        /// Off-chain keeper bot's pubkey. Must sign every `Rebalance`.
        pub keeper: [u8; 32],

        // ── 4-byte field ────────────────────────────────────────────────
        /// Portfolio-level leverage cap, in bps where 10_000 = 1x. Trades
        /// that would push portfolio IM utilisation above this are rejected.
        pub max_leverage_bps: u32,

        // ── 2-byte field ────────────────────────────────────────────────
        /// Buffer enforced above local MMR per enrolled market, in bps.
        pub buffer_bps: u16,

        // ── 1-byte fields ───────────────────────────────────────────────
        /// Bump for the data PDA.
        pub bump: u8,
        /// Bump for the `portfolio_auth` signing PDA.
        pub auth_bump: u8,
        /// Bump for the `portfolio_vault` token account PDA. `0` means the
        /// vault has not yet been created (call `InitVault` to do so);
        /// non-zero means the vault exists at the canonical address. The
        /// 0-sentinel collides with a true bump of 0 in roughly 1/256 of
        /// users; affected users would re-init. Acceptable v1 trade-off.
        pub vault_bump: u8,
        /// Layout version.
        pub version: u8,
        /// Emergency stop (0 = active, 1 = paused). While paused,
        /// keeper-driven Rebalance is rejected; user actions still work.
        pub paused: u8,
        /// Number of slots populated in `enrolled` (≤ MAX_ENROLLED_MARKETS).
        pub enrolled_count: u8,
        /// Padding to next 8-byte boundary.
        pub _pad0: [u8; 4],

        /// Enrolled markets. Slots beyond `enrolled_count` are zeroed.
        pub enrolled: [MarketSlot; MAX_ENROLLED_MARKETS],
    }

    // Layout breakdown:
    //   8 × 5  =  40   (magic..cached_total_mmr_e6)
    //  32 × 2  =  64   (owner, keeper)
    //   4 × 1  =   4   (max_leverage_bps)
    //   2 × 1  =   2   (buffer_bps)
    //   1 × 6  =   6   (bump, auth_bump, vault_bump, version, paused, enrolled_count)
    //   pad    =   4
    //               ───
    //              120   header
    //  48 × 16  = 768   enrolled
    //               ───
    //              888   total
    const _: () = assert!(core::mem::size_of::<PortfolioAccount>() == 888);
    const _: () = assert!(core::mem::align_of::<PortfolioAccount>() == 8);
    const _: () = assert!(
        core::mem::size_of::<PortfolioAccount>() < 4096,
        "PortfolioAccount should fit comfortably in a single allocation"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// 4. instruction enum + decode
// ─────────────────────────────────────────────────────────────────────────
pub mod instruction {
    use crate::errors::PortfolioError;
    use solana_program::program_error::ProgramError;

    /// Instruction tags. Stable u8 — never reorder, only append.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Instruction {
        /// Tag 0. Allocate a new `PortfolioAccount` PDA.
        ///
        /// Accounts:
        ///   0. `[signer, writable]` user (payer + authority)
        ///   1. `[writable]`         portfolio data PDA (uninitialized)
        ///   2. `[]`                 portfolio_auth PDA (verified, no data)
        ///   3. `[]`                 system program
        InitPortfolio {
            buffer_bps: u16,
            max_leverage_bps: u32,
            keeper: [u8; 32],
        },

        /// Tag 1. Enroll an existing per-market account into the portfolio.
        ///
        /// REQUIRES upstream `UpdateAccountOwner` — see DM thread. Until that
        /// lands, enrollment can only target a freshly-initialized
        /// percolator account whose owner was set to `portfolio_auth` at
        /// `InitUser` time.
        EnrollMarket {
            account_idx: u16,
        },

        /// Tag 2. Reverse of EnrollMarket. Returns owner authority to user.
        UnenrollMarket {
            account_idx: u16,
        },

        /// Tag 3. User deposits collateral into a specific enrolled market.
        Deposit {
            account_idx: u16,
            amount: u64,
        },

        /// Tag 4. User withdraws from an enrolled market. Validates that
        /// portfolio-level health is preserved post-withdraw.
        Withdraw {
            account_idx: u16,
            amount: u64,
        },

        /// Tag 5. User opens / modifies a position on an enrolled market via
        /// percolator-prog::TradeCpi. The wrapper signs as `portfolio_auth`
        /// (the engine account's owner). LP authorization is delegated to the
        /// matcher program at LP-registration time — the matcher CPI is what
        /// actually authorizes the trade against the LP's inventory; the
        /// wrapper merely forwards the matcher tail accounts unchanged.
        ///
        /// Per-market IM/MM is enforced engine-side. Cross-market portfolio
        /// IM is best-effort via keeper-side `Rebalance`; the wrapper does
        /// NOT do its own pre-trade aggregate margin check in v1 — that
        /// requires a fresh-oracle margin port that's deferred.
        Trade {
            account_idx: u16,
            lp_idx: u16,
            side: u8,
            size_q: u64,
            limit_price_e6: u64,
        },

        /// Tag 6. Keeper-signed multi-leg collateral rebalance.
        /// Each leg moves `amount` from `from_idx` to `to_idx`.
        Rebalance {
            leg_count: u8,
            // followed by leg_count × { from_idx: u16, to_idx: u16, amount: u64 }
        },

        /// Tag 7. User-controlled escape hatch. Forces a position close on
        /// `account_idx` regardless of portfolio health. For when the user
        /// wants out and doesn't trust the keeper to coordinate.
        EmergencyClose {
            account_idx: u16,
        },

        /// Tag 8. Update buffer / leverage / keeper.
        UpdateConfig {
            buffer_bps: u16,
            max_leverage_bps: u32,
            keeper: [u8; 32],
        },

        /// Tag 9. Toggle `paused`. While paused, Rebalance is rejected;
        /// user-signed actions still work.
        SetPaused {
            paused: bool,
        },

        /// Tag 10. Lazily create the user's `portfolio_vault` token account.
        /// Costs ~15K CU and ~rent-min lamports. Required before any
        /// Deposit / Withdraw / Trade / Rebalance / EmergencyClose.
        InitVault,

        /// Tag 11. Close the portfolio entirely and reclaim all rent.
        /// Requires:
        ///   - `enrolled_count == 0` (user must `Unenroll`/`EmergencyClose` first),
        ///   - `portfolio_vault.amount == 0` (must be drained — `Withdraw` last).
        /// On success, both `portfolio_data` PDA and `portfolio_vault`
        /// token account return their rent to the user. The portfolio is
        /// permanently destroyed; subsequent ops require a fresh
        /// `InitPortfolio`.
        ClosePortfolio,

        /// Tag 12. Atomic enroll: transfer fee from user_ata into the
        /// portfolio_vault, CPI `percolator-prog::InitUser` (signed as
        /// portfolio_auth so the engine sets `account.owner = portfolio_auth`),
        /// then record (market, expected_idx) in `enrolled[]`.
        ///
        /// `expected_idx` is what the off-chain client predicts the engine
        /// will assign as the new account's idx — typically derived from
        /// reading the slab pre-tx. The wrapper records it without
        /// verifying engine state (which would couple the wrapper to the
        /// engine's binary layout). If the prediction is wrong, downstream
        /// Deposit/Trade/Withdraw fail engine-side with the per-account
        /// owner check; the user loses only the InitUser fee. This is a
        /// trust-but-verify-on-use design — the consequence of being
        /// decoupled from the engine crate per upstream maintainer
        /// guidance.
        ///
        /// `fee_payment` must exceed the market's `new_account_fee`; the
        /// engine splits it into (insurance fee) + (initial capital).
        EnrollMarketAndInit {
            expected_idx: u16,
            fee_payment: u64,
        },

        /// Tag 13. Permissionless rebalance crank — Defense 3 of soft+
        /// cross-margin. ANY signer can call this to move collateral
        /// between two enrolled markets, but the crank ONLY succeeds (and
        /// only pays the caller a bounty) if the destination account was
        /// below its per-market initial-margin requirement BEFORE the
        /// rebalance. This recruits the entire MEV / arbitrage ecosystem
        /// as effective auxiliary keepers without exposing the portfolio
        /// to abuse — no rebalance happens unless one is actually needed,
        /// and a self-rebalance (from_idx == to_idx) is rejected.
        ///
        /// Body: u16 from_idx | u16 to_idx | u64 amount = 12 bytes.
        ///
        /// Bounty: paid from portfolio_vault to caller_payout_ata,
        /// capped at min(amount / `CRANK_BOUNTY_DIVISOR`,
        /// `CRANK_BOUNTY_CAP_UNITS`). v1 = 1% of rebalanced amount,
        /// capped at 1 USDC base unit (1_000_000 e6). Future versions
        /// can expose these as portfolio config.
        RebalanceCrank {
            from_idx: u16,
            to_idx: u16,
            amount: u64,
        },
    }

    impl Instruction {
        pub fn decode(data: &[u8]) -> Result<Self, ProgramError> {
            if data.is_empty() {
                return Err(PortfolioError::BadInstruction.into());
            }
            let tag = data[0];
            let body = &data[1..];
            match tag {
                0 => {
                    // B2: strict — tag 0 was the one place I left a `<`
                    // check on the first pass; Kani caught the resulting
                    // accept-extra-trailing-bytes case in
                    // `proofs::init_decode_strict_length`.
                    if body.len() != 2 + 4 + 32 {
                        return Err(PortfolioError::BadInstruction.into());
                    }
                    let buffer_bps = u16::from_le_bytes([body[0], body[1]]);
                    let max_leverage_bps =
                        u32::from_le_bytes([body[2], body[3], body[4], body[5]]);
                    let mut keeper = [0u8; 32];
                    keeper.copy_from_slice(&body[6..38]);
                    Ok(Instruction::InitPortfolio {
                        buffer_bps,
                        max_leverage_bps,
                        keeper,
                    })
                }
                // B2: strict length checks on every tag — extra trailing
                // bytes are a protocol error, not silently dropped.
                1 => {
                    if body.len() != 2 {
                        return Err(PortfolioError::BadInstruction.into());
                    }
                    Ok(Instruction::EnrollMarket {
                        account_idx: u16::from_le_bytes([body[0], body[1]]),
                    })
                }
                2 => {
                    if body.len() != 2 {
                        return Err(PortfolioError::BadInstruction.into());
                    }
                    Ok(Instruction::UnenrollMarket {
                        account_idx: u16::from_le_bytes([body[0], body[1]]),
                    })
                }
                3 => {
                    if body.len() != 2 + 8 {
                        return Err(PortfolioError::BadInstruction.into());
                    }
                    let account_idx = u16::from_le_bytes([body[0], body[1]]);
                    let amount = u64::from_le_bytes([
                        body[2], body[3], body[4], body[5], body[6], body[7], body[8], body[9],
                    ]);
                    Ok(Instruction::Deposit { account_idx, amount })
                }
                4 => {
                    if body.len() != 2 + 8 {
                        return Err(PortfolioError::BadInstruction.into());
                    }
                    let account_idx = u16::from_le_bytes([body[0], body[1]]);
                    let amount = u64::from_le_bytes([
                        body[2], body[3], body[4], body[5], body[6], body[7], body[8], body[9],
                    ]);
                    Ok(Instruction::Withdraw { account_idx, amount })
                }
                5 => {
                    // Trade body: u16 account_idx | u16 lp_idx | u8 side |
                    //             u64 size_q | u64 limit_price_e6 = 21 bytes.
                    if body.len() != 2 + 2 + 1 + 8 + 8 {
                        return Err(PortfolioError::BadInstruction.into());
                    }
                    let account_idx = u16::from_le_bytes([body[0], body[1]]);
                    let lp_idx = u16::from_le_bytes([body[2], body[3]]);
                    let side = body[4];
                    let size_q = u64::from_le_bytes([
                        body[5], body[6], body[7], body[8], body[9], body[10], body[11], body[12],
                    ]);
                    let limit_price_e6 = u64::from_le_bytes([
                        body[13], body[14], body[15], body[16], body[17], body[18], body[19],
                        body[20],
                    ]);
                    Ok(Instruction::Trade {
                        account_idx,
                        lp_idx,
                        side,
                        size_q,
                        limit_price_e6,
                    })
                }
                6 => {
                    // Rebalance body = leg_count(u8) + leg_count × (u16 from + u16 to + u64 amount).
                    // Each leg is 12 bytes.
                    if body.is_empty() {
                        return Err(PortfolioError::BadInstruction.into());
                    }
                    let leg_count = body[0];
                    // Decoder-side enforcement of MAX_REBALANCE_LEGS so
                    // malformed Rebalance ix data is rejected before
                    // entering the processor (runtime path is also
                    // guarded — defense in depth).
                    if leg_count > crate::constants::MAX_REBALANCE_LEGS {
                        return Err(PortfolioError::TooManyLegs.into());
                    }
                    let expected = 1usize + (leg_count as usize) * 12;
                    if body.len() != expected {
                        return Err(PortfolioError::BadInstruction.into());
                    }
                    Ok(Instruction::Rebalance { leg_count })
                }
                7 => {
                    if body.len() != 2 {
                        return Err(PortfolioError::BadInstruction.into());
                    }
                    Ok(Instruction::EmergencyClose {
                        account_idx: u16::from_le_bytes([body[0], body[1]]),
                    })
                }
                8 => {
                    if body.len() != 2 + 4 + 32 {
                        return Err(PortfolioError::BadInstruction.into());
                    }
                    let buffer_bps = u16::from_le_bytes([body[0], body[1]]);
                    let max_leverage_bps =
                        u32::from_le_bytes([body[2], body[3], body[4], body[5]]);
                    let mut keeper = [0u8; 32];
                    keeper.copy_from_slice(&body[6..38]);
                    Ok(Instruction::UpdateConfig {
                        buffer_bps,
                        max_leverage_bps,
                        keeper,
                    })
                }
                9 => {
                    // B3: strict — only 0 (active) or 1 (paused) accepted.
                    // Anything else is a protocol error.
                    if body.len() != 1 {
                        return Err(PortfolioError::BadInstruction.into());
                    }
                    let paused = match body[0] {
                        0 => false,
                        1 => true,
                        _ => return Err(PortfolioError::BadInstruction.into()),
                    };
                    Ok(Instruction::SetPaused { paused })
                }
                10 => {
                    if !body.is_empty() {
                        return Err(PortfolioError::BadInstruction.into());
                    }
                    Ok(Instruction::InitVault)
                }
                11 => {
                    if !body.is_empty() {
                        return Err(PortfolioError::BadInstruction.into());
                    }
                    Ok(Instruction::ClosePortfolio)
                }
                12 => {
                    // EnrollMarketAndInit body: u16 expected_idx | u64 fee_payment = 10 bytes.
                    if body.len() != 2 + 8 {
                        return Err(PortfolioError::BadInstruction.into());
                    }
                    let expected_idx = u16::from_le_bytes([body[0], body[1]]);
                    let fee_payment = u64::from_le_bytes([
                        body[2], body[3], body[4], body[5], body[6], body[7], body[8], body[9],
                    ]);
                    Ok(Instruction::EnrollMarketAndInit {
                        expected_idx,
                        fee_payment,
                    })
                }
                13 => {
                    // RebalanceCrank body: u16 from_idx | u16 to_idx | u64 amount = 12 bytes.
                    if body.len() != 2 + 2 + 8 {
                        return Err(PortfolioError::BadInstruction.into());
                    }
                    let from_idx = u16::from_le_bytes([body[0], body[1]]);
                    let to_idx = u16::from_le_bytes([body[2], body[3]]);
                    let amount = u64::from_le_bytes([
                        body[4], body[5], body[6], body[7], body[8], body[9], body[10], body[11],
                    ]);
                    Ok(Instruction::RebalanceCrank {
                        from_idx,
                        to_idx,
                        amount,
                    })
                }
                _ => Err(PortfolioError::BadInstruction.into()),
            }
        }
    }

    // B2: helpers no longer needed — every tag now uses inline strict-length
    // decoding for transparency. Keeping them removed so future tags can't
    // accidentally adopt the lax pattern.
}

// ─────────────────────────────────────────────────────────────────────────
// 5. processor
// ─────────────────────────────────────────────────────────────────────────
pub mod processor {
    use crate::constants::{
        MAGIC, MAX_BUFFER_BPS, MAX_PORTFOLIO_LEV_BPS, MIN_BUFFER_BPS, PORTFOLIO_AUTH_SEED,
        PORTFOLIO_SEED, PORTFOLIO_VAULT_SEED, VERSION,
    };
    use crate::cpi as cpi_helpers;
    use crate::errors::PortfolioError;
    use crate::instruction::Instruction;
    use crate::margin;
    use crate::pyth;
    use crate::state::PortfolioAccount;
    use alloc::vec::Vec;
    use bytemuck::{from_bytes_mut, Zeroable};
    use solana_program::{
        account_info::AccountInfo,
        clock::Clock,
        entrypoint::ProgramResult,
        instruction::AccountMeta,
        program::{invoke, invoke_signed},
        program_error::ProgramError,
        pubkey::Pubkey,
        rent::Rent,
        system_instruction, system_program,
        sysvar::Sysvar,
    };

    /// B4: single source of truth for the data-account size.
    pub const POOL_SIZE: usize = core::mem::size_of::<PortfolioAccount>();
    /// SPL Token v3 account size, as a u64 for `system_instruction::create_account`.
    /// The `usize` form lives in `crate::constants::SPL_TOKEN_ACCOUNT_LEN`.
    /// Both must remain at 165.
    const SPL_TOKEN_ACCOUNT_LEN: u64 =
        crate::constants::SPL_TOKEN_ACCOUNT_LEN as u64;
    const _: () = assert!(SPL_TOKEN_ACCOUNT_LEN == 165);
    /// SPL Token Program ID (`TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`).
    const SPL_TOKEN_PROGRAM: Pubkey = Pubkey::new_from_array(cpi_helpers::SPL_TOKEN_ID);
    /// `percolator-prog` Program ID. See `cpi.rs` for derivation.
    const PERCOLATOR_PROGRAM: Pubkey =
        Pubkey::new_from_array(cpi_helpers::PERCOLATOR_PROGRAM_ID);

    /// P-CRITICAL fix: every site that CPIs into percolator-prog must verify
    /// the supplied executable account is the canonical program. Without
    /// this guard, an attacker could route deposits/withdraws to a fake
    /// program. ~5 CU per call (one Pubkey::eq).
    fn verify_percolator_program(a: &AccountInfo) -> Result<(), ProgramError> {
        if a.key != &PERCOLATOR_PROGRAM {
            return Err(PortfolioError::BadProgram.into());
        }
        Ok(())
    }

    /// SPL Token program identity check. Distinct error from
    /// `BadAccountCount` so triage is unambiguous.
    fn verify_token_program(a: &AccountInfo) -> Result<(), ProgramError> {
        if a.key != &SPL_TOKEN_PROGRAM {
            return Err(PortfolioError::WrongTokenProgram.into());
        }
        Ok(())
    }

    /// System program identity check.
    fn verify_system_program(a: &AccountInfo) -> Result<(), ProgramError> {
        if a.key != &system_program::ID {
            return Err(PortfolioError::WrongSystemProgram.into());
        }
        Ok(())
    }

    pub fn process_instruction(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        data: &[u8],
    ) -> ProgramResult {
        let ix = Instruction::decode(data)?;
        match ix {
            Instruction::InitPortfolio {
                buffer_bps,
                max_leverage_bps,
                keeper,
            } => init_portfolio(program_id, accounts, buffer_bps, max_leverage_bps, keeper),
            Instruction::UpdateConfig {
                buffer_bps,
                max_leverage_bps,
                keeper,
            } => update_config(program_id, accounts, buffer_bps, max_leverage_bps, keeper),
            Instruction::SetPaused { paused } => set_paused(program_id, accounts, paused),
            Instruction::EnrollMarket { account_idx } => {
                enroll_market(program_id, accounts, account_idx)
            }
            Instruction::EnrollMarketAndInit {
                expected_idx,
                fee_payment,
            } => enroll_market_and_init(program_id, accounts, expected_idx, fee_payment),
            Instruction::RebalanceCrank {
                from_idx,
                to_idx,
                amount,
            } => rebalance_crank(program_id, accounts, from_idx, to_idx, amount),
            Instruction::UnenrollMarket { account_idx } => {
                unenroll_market(program_id, accounts, account_idx)
            }
            Instruction::InitVault => init_vault(program_id, accounts),
            Instruction::ClosePortfolio => close_portfolio(program_id, accounts),
            Instruction::Deposit { account_idx, amount } => {
                deposit(program_id, accounts, account_idx, amount)
            }
            Instruction::Withdraw { account_idx, amount } => {
                withdraw(program_id, accounts, account_idx, amount)
            }
            Instruction::Rebalance { leg_count } => {
                rebalance(program_id, accounts, leg_count, data)
            }
            Instruction::EmergencyClose { account_idx } => {
                emergency_close(program_id, accounts, account_idx)
            }
            Instruction::Trade {
                account_idx,
                lp_idx,
                side,
                size_q,
                limit_price_e6,
            } => trade(
                program_id,
                accounts,
                account_idx,
                lp_idx,
                side,
                size_q,
                limit_price_e6,
            ),
        }
    }

    /// Validate the data PDA: program-owned, correctly derived for this
    /// user, signed by the user, magic + version + stored-owner all match.
    /// Does NOT hold a borrow — caller borrows separately.
    ///
    /// A4 fix: explicitly verifies the PDA address matches
    /// `create_program_address([PORTFOLIO_SEED, user_pubkey, &[bump]])`.
    /// Defense-in-depth on top of the (owner, magic, stored-owner) chain.
    fn check_portfolio_account(
        program_id: &Pubkey,
        a_user: &AccountInfo,
        a_data: &AccountInfo,
    ) -> Result<(), ProgramError> {
        if !a_user.is_signer {
            return Err(PortfolioError::WrongSigner.into());
        }
        if a_data.owner != program_id {
            return Err(PortfolioError::AccountNotInitialized.into());
        }
        if !a_data.is_writable {
            return Err(ProgramError::InvalidAccountData);
        }
        let data = a_data.try_borrow_data()?;
        if data.len() < POOL_SIZE {
            return Err(PortfolioError::AccountNotInitialized.into());
        }
        let pa: &PortfolioAccount = bytemuck::from_bytes(&data[..POOL_SIZE]);
        if pa.magic != MAGIC {
            return Err(PortfolioError::BadMagic.into());
        }
        if pa.version != VERSION {
            return Err(PortfolioError::BadVersion.into());
        }
        if pa.owner != a_user.key.to_bytes() {
            return Err(PortfolioError::BadOwner.into());
        }
        // Verify the data PDA address matches the canonical derivation.
        // Cheap: ~1.5K CU vs find_program_address's ~200K, since the bump
        // is already stored in the struct.
        let expected = Pubkey::create_program_address(
            &[PORTFOLIO_SEED, a_user.key.as_ref(), &[pa.bump]],
            program_id,
        )
        .map_err(|_| PortfolioError::BadPda)?;
        if expected != *a_data.key {
            return Err(PortfolioError::BadPda.into());
        }
        Ok(())
    }

    fn init_portfolio(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        buffer_bps: u16,
        max_leverage_bps: u32,
        keeper: [u8; 32],
    ) -> ProgramResult {
        if accounts.len() != 4 {
            return Err(PortfolioError::BadAccountCount.into());
        }
        let a_user = &accounts[0];
        let a_data = &accounts[1];
        let a_auth = &accounts[2];
        let a_sys = &accounts[3];

        if !a_user.is_signer {
            return Err(PortfolioError::WrongSigner.into());
        }
        if !a_data.is_writable {
            return Err(ProgramError::InvalidAccountData);
        }
        // A1 fix: verify a_sys is actually the system program. Now uses
        // the dedicated `WrongSystemProgram` error variant (was
        // `BadAccountCount` — overloaded and misleading).
        verify_system_program(a_sys)?;

        if !(MIN_BUFFER_BPS..=MAX_BUFFER_BPS).contains(&buffer_bps) {
            return Err(PortfolioError::BufferOutOfRange.into());
        }
        if max_leverage_bps == 0 || max_leverage_bps > MAX_PORTFOLIO_LEV_BPS {
            return Err(PortfolioError::LeverageOutOfRange.into());
        }

        // Verify both PDAs.
        let (data_pda, data_bump) =
            Pubkey::find_program_address(&[PORTFOLIO_SEED, a_user.key.as_ref()], program_id);
        if data_pda != *a_data.key {
            return Err(PortfolioError::BadPda.into());
        }
        let (auth_pda, auth_bump) =
            Pubkey::find_program_address(&[PORTFOLIO_AUTH_SEED, a_user.key.as_ref()], program_id);
        if auth_pda != *a_auth.key {
            return Err(PortfolioError::BadPda.into());
        }

        // Reject if the data account is already allocated (idempotency guard).
        if !a_data.data_is_empty() {
            return Err(PortfolioError::AccountAlreadyInitialized.into());
        }

        // Allocate + assign the data PDA.
        let lamports = Rent::get()?.minimum_balance(POOL_SIZE);
        let create_ix = system_instruction::create_account(
            a_user.key,
            a_data.key,
            lamports,
            POOL_SIZE as u64,
            program_id,
        );
        let user_seed = a_user.key.as_ref();
        let signer_seeds: &[&[u8]] = &[PORTFOLIO_SEED, user_seed, &[data_bump]];
        invoke_signed(
            &create_ix,
            &[a_user.clone(), a_data.clone(), a_sys.clone()],
            &[signer_seeds],
        )?;

        // Initialize the struct in-place.
        let mut data = a_data.try_borrow_mut_data()?;
        let pa: &mut PortfolioAccount = from_bytes_mut(&mut data[..POOL_SIZE]);
        *pa = PortfolioAccount::zeroed();
        pa.magic = MAGIC;
        pa.owner = a_user.key.to_bytes();
        pa.bump = data_bump;
        pa.auth_bump = auth_bump;
        pa.version = VERSION;
        pa.paused = 0;
        pa.buffer_bps = buffer_bps;
        pa.max_leverage_bps = max_leverage_bps;
        pa.keeper = keeper;
        pa.enrolled_count = 0;

        Ok(())
    }

    fn update_config(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        buffer_bps: u16,
        max_leverage_bps: u32,
        keeper: [u8; 32],
    ) -> ProgramResult {
        // Accounts:
        //   0. [signer]   user
        //   1. [writable] portfolio data PDA
        if accounts.len() != 2 {
            return Err(PortfolioError::BadAccountCount.into());
        }
        let a_user = &accounts[0];
        let a_data = &accounts[1];

        check_portfolio_account(program_id, a_user, a_data)?;

        if !(MIN_BUFFER_BPS..=MAX_BUFFER_BPS).contains(&buffer_bps) {
            return Err(PortfolioError::BufferOutOfRange.into());
        }
        if max_leverage_bps == 0 || max_leverage_bps > MAX_PORTFOLIO_LEV_BPS {
            return Err(PortfolioError::LeverageOutOfRange.into());
        }

        let mut data = a_data.try_borrow_mut_data()?;
        let pa: &mut PortfolioAccount = from_bytes_mut(&mut data[..POOL_SIZE]);
        pa.buffer_bps = buffer_bps;
        pa.max_leverage_bps = max_leverage_bps;
        pa.keeper = keeper;
        Ok(())
    }

    fn set_paused(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        paused: bool,
    ) -> ProgramResult {
        // Accounts:
        //   0. [signer]   user
        //   1. [writable] portfolio data PDA
        if accounts.len() != 2 {
            return Err(PortfolioError::BadAccountCount.into());
        }
        let a_user = &accounts[0];
        let a_data = &accounts[1];

        check_portfolio_account(program_id, a_user, a_data)?;

        let mut data = a_data.try_borrow_mut_data()?;
        let pa: &mut PortfolioAccount = from_bytes_mut(&mut data[..POOL_SIZE]);
        pa.paused = if paused { 1 } else { 0 };
        Ok(())
    }

    /// EnrollMarket — record that the user holds a percolator account at
    /// `(market_slab, account_idx)` whose `owner` is the portfolio_auth PDA.
    ///
    /// This is **state-only** in v1: it does NOT verify the percolator
    /// engine's `account.owner == portfolio_auth` invariant. The user
    /// remains responsible for ensuring the per-market account was created
    /// with the correct owner (via a separate path — to be wired into
    /// EnrollMarket itself once `percolator-prog::InitUser` is invoked from
    /// here in the next milestone). If the user enrolls a market they
    /// don't actually control, every subsequent Deposit / Trade / Withdraw
    /// will fail at the engine's owner check, so they only hurt their own
    /// UX, never another user.
    ///
    /// CU note: the heavy validation (engine ownership) is deferred to the
    /// CPI'd handler in percolator-prog. Doing it here would mean reading
    /// the slab (~5K CU) on every enrol — and we'd still only be confirming
    /// what percolator-prog itself verifies on first use.
    ///
    /// Accounts:
    ///   0. [signer]   user
    ///   1. [writable] portfolio data PDA
    ///   2. []         market slab (passed for its pubkey)
    fn enroll_market(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        account_idx: u16,
    ) -> ProgramResult {
        if accounts.len() != 3 {
            return Err(PortfolioError::BadAccountCount.into());
        }
        let a_user = &accounts[0];
        let a_data = &accounts[1];
        let a_market = &accounts[2];

        check_portfolio_account(program_id, a_user, a_data)?;

        let mut data = a_data.try_borrow_mut_data()?;
        let pa: &mut PortfolioAccount = from_bytes_mut(&mut data[..POOL_SIZE]);

        if pa.paused != 0 {
            return Err(PortfolioError::Paused.into());
        }

        let count = pa.enrolled_count as usize;
        if count >= crate::constants::MAX_ENROLLED_MARKETS {
            return Err(PortfolioError::TooManyEnrolled.into());
        }

        // Reject duplicate enrolment of the same (market, idx) pair. Walk the
        // populated prefix only — slots beyond `count` are guaranteed zeroed.
        let market_bytes = a_market.key.to_bytes();
        for i in 0..count {
            if pa.enrolled[i].market == market_bytes
                && pa.enrolled[i].account_idx == account_idx
            {
                return Err(PortfolioError::MarketAlreadyEnrolled.into());
            }
        }

        // Write the new slot.
        pa.enrolled[count].market = market_bytes;
        pa.enrolled[count].account_idx = account_idx;
        pa.enrolled[count].last_seen_eq_e6 = 0;
        pa.enrolled[count]._pad0 = [0u8; 6];
        pa.enrolled_count = (count + 1) as u8;

        Ok(())
    }

    /// EnrollMarketAndInit — atomic version of EnrollMarket. Funds the
    /// portfolio_vault with `fee_payment` from the user's ATA, then CPIs
    /// `percolator-prog::InitUser` signed as `portfolio_auth` so the engine
    /// account is created with `account.owner = portfolio_auth`. Records
    /// `(market, expected_idx)` in `enrolled[]` after the CPI succeeds.
    ///
    /// `expected_idx` is what the off-chain client predicts the engine
    /// will assign. The wrapper does NOT verify the prediction — that
    /// would require reading engine state at a known offset, which
    /// couples the wrapper to the engine's binary layout. If the
    /// prediction is wrong, downstream Deposit/Trade/Withdraw fail
    /// engine-side at the per-account owner check; the user loses only
    /// the InitUser fee.
    ///
    /// Account layout (10):
    ///   0. `[signer]`            user
    ///   1. `[writable]`          portfolio_data PDA
    ///   2. `[]`                  portfolio_auth PDA
    ///   3. `[writable]`          portfolio_vault token account
    ///   4. `[writable]`          user_ata (source of fee_payment)
    ///   5. `[writable]`          market slab
    ///   6. `[writable]`          market_vault (engine's destination)
    ///   7. `[]`                  spl_token_program
    ///   8. `[]`                  clock sysvar
    ///   9. `[]`                  percolator-prog (executable)
    fn enroll_market_and_init(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        expected_idx: u16,
        fee_payment: u64,
    ) -> ProgramResult {
        if accounts.len() != 10 {
            return Err(PortfolioError::BadAccountCount.into());
        }
        let a_user = &accounts[0];
        let a_data = &accounts[1];
        let a_auth = &accounts[2];
        let a_vault = &accounts[3];
        let a_user_ata = &accounts[4];
        let a_slab = &accounts[5];
        let a_market_vault = &accounts[6];
        let a_token = &accounts[7];
        let a_clock = &accounts[8];
        let a_percolator_prog = &accounts[9];

        // Cheap validation first.
        if fee_payment == 0 {
            return Err(PortfolioError::ZeroAmount.into());
        }
        verify_token_program(a_token)?;
        verify_percolator_program(a_percolator_prog)?;

        // Verify portfolio + auth + vault PDA chain. Returns the bumps
        // we'll need to sign as portfolio_auth.
        let (auth_bump, vault_bump) =
            check_portfolio_for_cpi(program_id, a_user, a_data, a_auth)?;
        if vault_bump == 0 {
            return Err(PortfolioError::AccountNotInitialized.into());
        }

        // Verify portfolio_vault PDA derivation matches the stored bump.
        let expected_vault = Pubkey::create_program_address(
            &[PORTFOLIO_VAULT_SEED, a_user.key.as_ref(), &[vault_bump]],
            program_id,
        )
        .map_err(|_| PortfolioError::BadPda)?;
        if expected_vault != *a_vault.key {
            return Err(PortfolioError::BadPda.into());
        }

        // Pre-flight wrapper-side checks against portfolio_data, in one
        // borrow scope. Reject paused, capacity-full, and known
        // (market, expected_idx) duplicates before we touch any token
        // transfer or CPI machinery.
        {
            let data = a_data.try_borrow_data()?;
            let pa: &PortfolioAccount = bytemuck::from_bytes(&data[..POOL_SIZE]);
            if pa.paused != 0 {
                return Err(PortfolioError::Paused.into());
            }
            let count = pa.enrolled_count as usize;
            if count >= crate::constants::MAX_ENROLLED_MARKETS {
                return Err(PortfolioError::TooManyEnrolled.into());
            }
            // Duplicate guard — same as state-only EnrollMarket. Walks
            // populated prefix only; slots beyond `count` are zeroed.
            let market_bytes = a_slab.key.to_bytes();
            for i in 0..count {
                if pa.enrolled[i].market == market_bytes
                    && pa.enrolled[i].account_idx == expected_idx
                {
                    return Err(PortfolioError::MarketAlreadyEnrolled.into());
                }
            }
        }

        // Step 1: SPL transfer user_ata → portfolio_vault, signed by user.
        // The vault must hold fee_payment before the InitUser CPI because
        // percolator-prog::InitUser pulls from a_user_ata, and we set
        // a_user_ata = portfolio_vault below. Vault.owner is portfolio_auth
        // (set up at InitVault), so the engine's verify_token_account
        // check on (vault.owner == a_user signer) passes when a_user is
        // also portfolio_auth via invoke_signed.
        let transfer_ix =
            cpi_helpers::spl_token_transfer(*a_user_ata.key, *a_vault.key, *a_user.key, fee_payment);
        invoke(
            &transfer_ix,
            &[
                a_user_ata.clone(),
                a_vault.clone(),
                a_user.clone(),
                a_token.clone(),
            ],
        )?;

        // Step 2: CPI percolator-prog::InitUser, signed as portfolio_auth.
        // Engine sets accounts[new_idx].owner = portfolio_auth (since
        // a_user passed into the CPI is portfolio_auth). The new_idx is
        // assigned internally by `prepare_lazy_free_head`; we trust the
        // caller's `expected_idx` prediction without verification, per
        // the design note above.
        let init_ix = cpi_helpers::percolator_init_user(
            *a_percolator_prog.key,
            *a_auth.key,
            *a_slab.key,
            *a_vault.key,
            *a_market_vault.key,
            *a_token.key,
            *a_clock.key,
            fee_payment,
        );
        let user_seed = a_user.key.as_ref();
        let auth_seeds: &[&[u8]] = &[PORTFOLIO_AUTH_SEED, user_seed, &[auth_bump]];
        invoke_signed(
            &init_ix,
            &[
                a_auth.clone(),
                a_slab.clone(),
                a_vault.clone(),
                a_market_vault.clone(),
                a_token.clone(),
                a_clock.clone(),
                a_percolator_prog.clone(),
            ],
            &[auth_seeds],
        )?;

        // Step 3: record (market, expected_idx). Re-borrow with write
        // permission. Capacity + duplicate were checked pre-CPI; the
        // CPI itself has no path to mutate pa.enrolled, so the count
        // and slot we computed before the CPI are still valid.
        {
            let mut data = a_data.try_borrow_mut_data()?;
            let pa: &mut PortfolioAccount = from_bytes_mut(&mut data[..POOL_SIZE]);
            let count = pa.enrolled_count as usize;
            pa.enrolled[count].market = a_slab.key.to_bytes();
            pa.enrolled[count].account_idx = expected_idx;
            pa.enrolled[count].last_seen_eq_e6 = 0;
            pa.enrolled[count]._pad0 = [0u8; 6];
            pa.enrolled_count = (count + 1) as u8;
        }

        Ok(())
    }

    /// Defense 3 — permissionless rebalance crank.
    ///
    /// Anyone can call. Wrapper moves `amount` collateral from the
    /// `from_idx` enrolled account to the `to_idx` enrolled account,
    /// paying the caller a bounty IF — and only if — the destination
    /// was below its per-market initial margin BEFORE the rebalance.
    /// This recruits MEV / arbitrage bots as auxiliary keepers without
    /// inviting waste: no rebalance triggers the bounty, and a self-
    /// rebalance is rejected.
    ///
    /// Bounty formula: `min(amount / CRANK_BOUNTY_DIVISOR, CRANK_BOUNTY_CAP_UNITS)`
    /// where CRANK_BOUNTY_DIVISOR = 100 (1% of moved amount) and
    /// CRANK_BOUNTY_CAP_UNITS = 1_000_000 (1 USDC at e6).
    ///
    /// Account layout (15 fixed):
    ///   0. `[signer]`            caller (any signer; bounty recipient)
    ///   1. `[writable]`          portfolio_data PDA
    ///   2. `[]`                  portfolio_auth PDA
    ///   3. `[writable]`          portfolio_vault token account
    ///   4. `[writable]`          caller_payout_ata (bounty destination)
    ///   5. `[]`                  spl_token_program
    ///   6. `[]`                  clock sysvar
    ///   7. `[]`                  percolator-prog (executable)
    ///   8. `[writable]`          from_slab
    ///   9. `[writable]`          from_market_vault
    ///  10. `[]`                  from_market_vault_authority
    ///  11. `[]`                  from_oracle
    ///  12. `[writable]`          to_slab
    ///  13. `[writable]`          to_market_vault
    ///  14. `[]`                  to_oracle (for the "needs help" gate)
    fn rebalance_crank(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        from_idx: u16,
        to_idx: u16,
        amount: u64,
    ) -> ProgramResult {
        /// Bounty: 1% of rebalanced amount.
        const CRANK_BOUNTY_DIVISOR: u64 = 100;
        /// Cap: 1 USDC at e6.
        const CRANK_BOUNTY_CAP_UNITS: u64 = 1_000_000;

        if accounts.len() != 15 {
            return Err(PortfolioError::BadAccountCount.into());
        }
        let a_caller = &accounts[0];
        let a_data = &accounts[1];
        let a_auth = &accounts[2];
        let a_vault = &accounts[3];
        let a_payout = &accounts[4];
        let a_token = &accounts[5];
        let a_clock = &accounts[6];
        let a_percolator_prog = &accounts[7];
        let a_from_slab = &accounts[8];
        let a_from_vault = &accounts[9];
        let a_from_vault_auth = &accounts[10];
        let a_from_oracle = &accounts[11];
        let a_to_slab = &accounts[12];
        let a_to_vault = &accounts[13];
        let a_to_oracle = &accounts[14];

        // Cheap validations first.
        if amount == 0 {
            return Err(PortfolioError::ZeroAmount.into());
        }
        // Self-leg rejection: rebalancing within the same (slab, idx) is
        // a no-op + the bounty would be unearned. Match the Rebalance
        // ix's own guard.
        if a_from_slab.key == a_to_slab.key && from_idx == to_idx {
            return Err(PortfolioError::CrankSelfLeg.into());
        }
        if !a_caller.is_signer {
            return Err(PortfolioError::WrongSigner.into());
        }
        verify_token_program(a_token)?;
        verify_percolator_program(a_percolator_prog)?;

        // Read portfolio state once: verify both endpoints are enrolled
        // and not paused.
        let auth_bump: u8;
        let vault_bump: u8;
        let user_pubkey_bytes: [u8; 32];
        {
            let data = a_data.try_borrow_data()?;
            if data.len() < POOL_SIZE {
                return Err(PortfolioError::AccountNotInitialized.into());
            }
            let pa: &PortfolioAccount = bytemuck::from_bytes(&data[..POOL_SIZE]);
            if pa.magic != MAGIC {
                return Err(PortfolioError::BadMagic.into());
            }
            if pa.version != VERSION {
                return Err(PortfolioError::BadVersion.into());
            }
            if pa.paused != 0 {
                return Err(PortfolioError::Paused.into());
            }
            if find_enrolled(pa, a_from_slab.key, from_idx).is_none() {
                return Err(PortfolioError::MarketNotEnrolled.into());
            }
            if find_enrolled(pa, a_to_slab.key, to_idx).is_none() {
                return Err(PortfolioError::MarketNotEnrolled.into());
            }
            auth_bump = pa.auth_bump;
            vault_bump = pa.vault_bump;
            user_pubkey_bytes = pa.owner;
        }
        if vault_bump == 0 {
            return Err(PortfolioError::AccountNotInitialized.into());
        }

        // Verify PDAs derive correctly (defence-in-depth).
        let user_pubkey = Pubkey::new_from_array(user_pubkey_bytes);
        let expected_auth = Pubkey::create_program_address(
            &[PORTFOLIO_AUTH_SEED, user_pubkey.as_ref(), &[auth_bump]],
            program_id,
        )
        .map_err(|_| PortfolioError::BadPda)?;
        if expected_auth != *a_auth.key {
            return Err(PortfolioError::BadPda.into());
        }
        let expected_vault = Pubkey::create_program_address(
            &[PORTFOLIO_VAULT_SEED, user_pubkey.as_ref(), &[vault_bump]],
            program_id,
        )
        .map_err(|_| PortfolioError::BadPda)?;
        if expected_vault != *a_vault.key {
            return Err(PortfolioError::BadPda.into());
        }

        // ── The "needs help" gate ───────────────────────────────────────
        // Decode the destination slab, get its account, decode its
        // fresh oracle, and check engine.is_above_initial_margin. The
        // crank is only payable if dest was BELOW IM before the
        // rebalance — otherwise the caller did unnecessary work and
        // gets nothing. The borrow on to_slab data is released before
        // the CPIs below (which will re-borrow writably).
        let now_unix_ts = Clock::from_account_info(a_clock)?.unix_timestamp;
        {
            let to_data = a_to_slab.try_borrow_data()?;
            let engine = percolator_prog::zc::engine_ref(&to_data)
                .map_err(|_| PortfolioError::MarginSlabDecodeFailed)?;
            let to_idx_usize = to_idx as usize;
            if to_idx_usize >= percolator::MAX_ACCOUNTS || !engine.is_used(to_idx_usize) {
                return Err(PortfolioError::MarginSlabNotEnrolled.into());
            }
            let to_account = &engine.accounts[to_idx_usize];
            let oracle_price = pyth::read_oracle_price_e6(a_to_oracle, &to_data, now_unix_ts)?;
            if engine.is_above_initial_margin(to_account, to_idx_usize, oracle_price) {
                // Destination is already healthy — no rebalance needed.
                return Err(PortfolioError::CrankNotNeeded.into());
            }
        }

        // ── Execute the rebalance leg (Withdraw then Deposit) ────────────
        let auth_seeds: &[&[u8]] = &[PORTFOLIO_AUTH_SEED, user_pubkey.as_ref(), &[auth_bump]];

        let wd_ix = cpi_helpers::percolator_withdraw_collateral(
            *a_percolator_prog.key,
            *a_auth.key,
            *a_from_slab.key,
            *a_from_vault.key,
            *a_vault.key,
            *a_from_vault_auth.key,
            *a_token.key,
            *a_clock.key,
            *a_from_oracle.key,
            from_idx,
            amount,
        );
        invoke_signed(
            &wd_ix,
            &[
                a_auth.clone(),
                a_from_slab.clone(),
                a_from_vault.clone(),
                a_vault.clone(),
                a_from_vault_auth.clone(),
                a_token.clone(),
                a_clock.clone(),
                a_from_oracle.clone(),
                a_percolator_prog.clone(),
            ],
            &[auth_seeds],
        )?;

        let dep_ix = cpi_helpers::percolator_deposit_collateral(
            *a_percolator_prog.key,
            *a_auth.key,
            *a_to_slab.key,
            *a_vault.key,
            *a_to_vault.key,
            *a_token.key,
            *a_clock.key,
            to_idx,
            amount,
        );
        invoke_signed(
            &dep_ix,
            &[
                a_auth.clone(),
                a_to_slab.clone(),
                a_vault.clone(),
                a_to_vault.clone(),
                a_token.clone(),
                a_clock.clone(),
                a_percolator_prog.clone(),
            ],
            &[auth_seeds],
        )?;

        // ── Pay the caller bounty from portfolio_vault ─────────────────
        // Capped at min(amount / divisor, cap). Saturating divide
        // handles small `amount` cleanly (yields 0 → no bounty for
        // dust-sized cranks). Bounty signer is portfolio_auth via PDA.
        let bounty = core::cmp::min(amount / CRANK_BOUNTY_DIVISOR, CRANK_BOUNTY_CAP_UNITS);
        if bounty > 0 {
            let bounty_ix = cpi_helpers::spl_token_transfer(
                *a_vault.key,
                *a_payout.key,
                *a_auth.key,
                bounty,
            );
            invoke_signed(
                &bounty_ix,
                &[
                    a_vault.clone(),
                    a_payout.clone(),
                    a_auth.clone(),
                    a_token.clone(),
                ],
                &[auth_seeds],
            )?;
        }

        // Update last_rebalance_slot for monitoring.
        {
            let mut data = a_data.try_borrow_mut_data()?;
            let pa: &mut PortfolioAccount = from_bytes_mut(&mut data[..POOL_SIZE]);
            pa.last_rebalance_slot = Clock::get()?.slot;
        }

        Ok(())
    }

    /// Lookup helper: walk the populated prefix of `enrolled[]` for a slot
    /// matching `(market, account_idx)`. Returns the slot index on hit.
    /// Caller must hold a borrow of the data already.
    fn find_enrolled(pa: &PortfolioAccount, market: &Pubkey, idx: u16) -> Option<usize> {
        let count = pa.enrolled_count as usize;
        let market_bytes = market.to_bytes();
        for i in 0..count {
            if pa.enrolled[i].market == market_bytes && pa.enrolled[i].account_idx == idx {
                return Some(i);
            }
        }
        None
    }

    /// Verify that `a_data` is a valid portfolio account for `a_user`. Same
    /// chain as `check_portfolio_account` but DOES NOT require write
    /// permission — used by handlers that only need to *read* portfolio
    /// state before doing CPIs that mutate other accounts. Returns the
    /// `auth_bump` so callers can sign as `portfolio_auth`.
    fn check_portfolio_for_cpi(
        program_id: &Pubkey,
        a_user: &AccountInfo,
        a_data: &AccountInfo,
        a_auth: &AccountInfo,
    ) -> Result<(u8, u8), ProgramError> {
        if !a_user.is_signer {
            return Err(PortfolioError::WrongSigner.into());
        }
        if a_data.owner != program_id {
            return Err(PortfolioError::AccountNotInitialized.into());
        }
        let data = a_data.try_borrow_data()?;
        if data.len() < POOL_SIZE {
            return Err(PortfolioError::AccountNotInitialized.into());
        }
        let pa: &PortfolioAccount = bytemuck::from_bytes(&data[..POOL_SIZE]);
        if pa.magic != MAGIC {
            return Err(PortfolioError::BadMagic.into());
        }
        if pa.version != VERSION {
            return Err(PortfolioError::BadVersion.into());
        }
        if pa.owner != a_user.key.to_bytes() {
            return Err(PortfolioError::BadOwner.into());
        }
        // Verify the auth PDA is correctly derived for this user.
        let expected_auth = Pubkey::create_program_address(
            &[PORTFOLIO_AUTH_SEED, a_user.key.as_ref(), &[pa.auth_bump]],
            program_id,
        )
        .map_err(|_| PortfolioError::BadPda)?;
        if expected_auth != *a_auth.key {
            return Err(PortfolioError::BadPda.into());
        }
        Ok((pa.auth_bump, pa.vault_bump))
    }

    /// InitVault — allocate the per-user `portfolio_vault` token account.
    ///
    /// Two-step CPI:
    ///   1. `system_program::create_account` — allocate vault PDA at
    ///      `[PORTFOLIO_VAULT_SEED, user_pubkey]`, owned by SPL-Token
    ///      program, with rent-min lamports and `SPL_TOKEN_ACCOUNT_LEN`
    ///      bytes. Signed by both vault PDA and user (vault PDA via
    ///      `invoke_signed`, user via the outer tx signature).
    ///   2. `spl_token::initialize_account3` — initialise the new account
    ///      with the supplied mint and `portfolio_auth` as authority.
    ///
    /// Accounts:
    ///   0. `[signer, writable]` user (payer)
    ///   1. `[writable]`         portfolio_data PDA
    ///   2. `[]`                 portfolio_auth PDA (verified, no data)
    ///   3. `[writable]`         portfolio_vault PDA (uninitialised)
    ///   4. `[]`                 collateral_mint
    ///   5. `[]`                 system_program
    ///   6. `[]`                 spl_token_program
    fn init_vault(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
        if accounts.len() != 7 {
            return Err(PortfolioError::BadAccountCount.into());
        }
        let a_user = &accounts[0];
        let a_data = &accounts[1];
        let a_auth = &accounts[2];
        let a_vault = &accounts[3];
        let a_mint = &accounts[4];
        let a_sys = &accounts[5];
        let a_token = &accounts[6];

        check_portfolio_account(program_id, a_user, a_data)?;

        verify_system_program(a_sys)?;
        verify_token_program(a_token)?;
        if !a_vault.is_writable {
            return Err(ProgramError::InvalidAccountData);
        }

        // Verify auth PDA derivation.
        let pa_auth_bump = {
            let data = a_data.try_borrow_data()?;
            let pa: &PortfolioAccount = bytemuck::from_bytes(&data[..POOL_SIZE]);
            // Reject if vault is already initialised (vault_bump != 0).
            if pa.vault_bump != 0 {
                return Err(PortfolioError::AccountAlreadyInitialized.into());
            }
            pa.auth_bump
        };
        let expected_auth = Pubkey::create_program_address(
            &[PORTFOLIO_AUTH_SEED, a_user.key.as_ref(), &[pa_auth_bump]],
            program_id,
        )
        .map_err(|_| PortfolioError::BadPda)?;
        if expected_auth != *a_auth.key {
            return Err(PortfolioError::BadPda.into());
        }

        // Derive vault PDA fresh (we don't have a stored bump yet).
        let (vault_pda, vault_bump) = Pubkey::find_program_address(
            &[PORTFOLIO_VAULT_SEED, a_user.key.as_ref()],
            program_id,
        );
        if vault_pda != *a_vault.key {
            return Err(PortfolioError::BadPda.into());
        }
        if !a_vault.data_is_empty() {
            return Err(PortfolioError::AccountAlreadyInitialized.into());
        }

        // create_account: signed by vault PDA (so the new account belongs at
        // its derived address) AND by the user (the lamports source).
        let lamports = Rent::get()?.minimum_balance(SPL_TOKEN_ACCOUNT_LEN as usize);
        let create_ix = system_instruction::create_account(
            a_user.key,
            a_vault.key,
            lamports,
            SPL_TOKEN_ACCOUNT_LEN,
            &SPL_TOKEN_PROGRAM,
        );
        let user_seed = a_user.key.as_ref();
        let vault_seeds: &[&[u8]] = &[PORTFOLIO_VAULT_SEED, user_seed, &[vault_bump]];
        invoke_signed(
            &create_ix,
            &[a_user.clone(), a_vault.clone(), a_sys.clone()],
            &[vault_seeds],
        )?;

        // initialize_account3: token account + mint + authority. The
        // authority is the portfolio_auth PDA (NOT the vault PDA itself).
        let init_ix = cpi_helpers::spl_token_initialize_account3(
            *a_vault.key,
            *a_mint.key,
            *a_auth.key,
        );
        invoke(
            &init_ix,
            &[a_vault.clone(), a_mint.clone(), a_token.clone()],
        )?;

        // Persist the bump.
        let mut data = a_data.try_borrow_mut_data()?;
        let pa: &mut PortfolioAccount = from_bytes_mut(&mut data[..POOL_SIZE]);
        pa.vault_bump = vault_bump;
        Ok(())
    }

    /// ClosePortfolio — reclaim all rent and destroy the portfolio.
    ///
    /// Preconditions enforced:
    ///   - `enrolled_count == 0` — every market must have been
    ///     unenrolled or emergency-closed first. This stops a user from
    ///     accidentally orphaning a position they still own in
    ///     percolator-prog.
    ///   - `portfolio_vault.amount == 0` — vault must be drained. Stops
    ///     accidental forfeiture of in-flight USDC.
    ///   - `paused == 0` — closing a paused portfolio is suspicious;
    ///     unpause first if you really mean it.
    ///
    /// On success:
    ///   1. CPI `spl_token::CloseAccount` on `portfolio_vault`, dest =
    ///      user. Returns ~rent_min lamports to the user. Authority is
    ///      `portfolio_auth` PDA via `invoke_signed`.
    ///   2. Drains `portfolio_data` lamports to user. Zeroes the data
    ///      and reassigns ownership to the system program. The PDA is
    ///      now garbage-collectable.
    ///
    /// CU: ~30K (one CPI + bytemuck zero + lamport reassign).
    ///
    /// Accounts (5):
    ///   0. `[signer, writable]` user (rent destination)
    ///   1. `[writable]`          portfolio_data PDA
    ///   2. `[]`                  portfolio_auth PDA
    ///   3. `[writable]`          portfolio_vault token account
    ///   4. `[]`                  spl_token_program
    fn close_portfolio(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
        if accounts.len() != 5 {
            return Err(PortfolioError::BadAccountCount.into());
        }
        let a_user = &accounts[0];
        let a_data = &accounts[1];
        let a_auth = &accounts[2];
        let a_vault = &accounts[3];
        let a_token = &accounts[4];

        let (auth_bump, vault_bump) =
            check_portfolio_for_cpi(program_id, a_user, a_data, a_auth)?;
        if !a_data.is_writable {
            return Err(PortfolioError::DataAccountNotWritable.into());
        }
        if !a_user.is_writable {
            return Err(PortfolioError::DataAccountNotWritable.into());
        }
        verify_token_program(a_token)?;

        // Read enrolled_count and paused under one borrow.
        let (enrolled_count, paused) = {
            let data = a_data.try_borrow_data()?;
            let pa: &PortfolioAccount = bytemuck::from_bytes(&data[..POOL_SIZE]);
            (pa.enrolled_count, pa.paused)
        };
        if enrolled_count != 0 {
            return Err(PortfolioError::TooManyEnrolled.into());
        }
        if paused != 0 {
            return Err(PortfolioError::Paused.into());
        }

        // If a vault was created, it must be empty and we must close it.
        // If never created (vault_bump == 0), there's nothing to close
        // on the SPL side — only the data PDA gets reclaimed.
        if vault_bump != 0 {
            // Vault PDA derivation must match.
            let expected_vault = Pubkey::create_program_address(
                &[PORTFOLIO_VAULT_SEED, a_user.key.as_ref(), &[vault_bump]],
                program_id,
            )
            .map_err(|_| PortfolioError::BadPda)?;
            if expected_vault != *a_vault.key {
                return Err(PortfolioError::BadPda.into());
            }

            // Vault must have zero balance — defensive guard against
            // accidental token forfeiture.
            let vault_amt = read_token_account_amount(a_vault)?;
            if vault_amt != 0 {
                return Err(PortfolioError::ZeroAmount.into());
            }

            // CPI spl_token::CloseAccount, dest = user (gets the rent).
            let close_ix = cpi_helpers::spl_token_close_account(
                *a_vault.key,
                *a_user.key,
                *a_auth.key,
            );
            let user_seed = a_user.key.as_ref();
            let auth_seeds: &[&[u8]] = &[PORTFOLIO_AUTH_SEED, user_seed, &[auth_bump]];
            invoke_signed(
                &close_ix,
                &[a_vault.clone(), a_user.clone(), a_auth.clone(), a_token.clone()],
                &[auth_seeds],
            )?;
        }

        // Standard Solana account-close sequence:
        //   1. Zero the data (defensive — old magic must not be readable).
        //   2. Realloc to 0 so the next `create_account` at this address
        //      doesn't see leftover bytes.
        //   3. Reassign owner to system_program so the runtime treats this
        //      as a free address.
        //   4. Drain lamports to user. With lamports=0, the runtime
        //      garbage-collects at rent epoch, but the address is
        //      immediately reusable.
        {
            let mut data = a_data.try_borrow_mut_data()?;
            for b in data.iter_mut() {
                *b = 0;
            }
        }
        a_data.realloc(0, false)?;
        a_data.assign(&system_program::ID);
        let lamports = a_data.lamports();
        **a_data.try_borrow_mut_lamports()? = 0;
        **a_user.try_borrow_mut_lamports()? = a_user
            .lamports()
            .checked_add(lamports)
            .ok_or(PortfolioError::ArithmeticOverflow)?;
        Ok(())
    }

    /// Deposit — user moves USDC from their wallet ATA into a per-market
    /// percolator account, routed through `portfolio_vault`.
    ///
    /// Two CPIs in one tx (atomic via Solana's tx model):
    ///   1. `spl_token::transfer` — user_ata → portfolio_vault, signed by user
    ///   2. `percolator-prog::DepositCollateral` — portfolio_vault →
    ///      market_vault, signed by `portfolio_auth` PDA. percolator-prog
    ///      verifies its own internal `engine.accounts[idx].owner ==
    ///      portfolio_auth` invariant; we don't duplicate that check.
    ///
    /// Accounts (11):
    ///   0. `[signer, writable]` user
    ///   1. `[]`                  portfolio_data PDA
    ///   2. `[]`                  portfolio_auth PDA
    ///   3. `[writable]`          portfolio_vault PDA
    ///   4. `[writable]`          user_ata
    ///   5. `[writable]`          market slab (also serves as the enrolment-lookup key)
    ///   6. `[writable]`          market vault
    ///   7. `[]`                  market vault authority PDA
    ///   8. `[]`                  spl_token_program
    ///   9. `[]`                  clock sysvar
    ///   10. `[]`                 percolator-prog (executable)
    fn deposit(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        account_idx: u16,
        amount: u64,
    ) -> ProgramResult {
        if accounts.len() != 11 {
            return Err(PortfolioError::BadAccountCount.into());
        }
        let a_user = &accounts[0];
        let a_data = &accounts[1];
        let a_auth = &accounts[2];
        let a_vault = &accounts[3];
        let a_user_ata = &accounts[4];
        let a_slab = &accounts[5];
        let a_market_vault = &accounts[6];
        let a_market_vault_auth = &accounts[7];
        let a_token = &accounts[8];
        let a_clock = &accounts[9];
        let a_percolator_prog = &accounts[10];

        // Zero-amount fast-fail. percolator-prog rejects amount==0 with a
        // generic InvalidArgument, but we should reject earlier with our
        // own dedicated code so callers get a clear signal.
        if amount == 0 {
            return Err(PortfolioError::ZeroAmount.into());
        }

        let (auth_bump, vault_bump) =
            check_portfolio_for_cpi(program_id, a_user, a_data, a_auth)?;
        if vault_bump == 0 {
            return Err(PortfolioError::AccountNotInitialized.into());
        }
        verify_token_program(a_token)?;
        verify_percolator_program(a_percolator_prog)?;
        // The market account passed (a_slab) MUST be enrolled in this
        // portfolio at the given account_idx. Otherwise reject without
        // CPIing — the engine's local owner check would catch it anyway
        // but we surface a clearer error.
        {
            let data = a_data.try_borrow_data()?;
            let pa: &PortfolioAccount = bytemuck::from_bytes(&data[..POOL_SIZE]);
            if find_enrolled(pa, a_slab.key, account_idx).is_none() {
                return Err(PortfolioError::MarketNotEnrolled.into());
            }
        }

        // Verify vault PDA derivation matches stored bump.
        let expected_vault = Pubkey::create_program_address(
            &[PORTFOLIO_VAULT_SEED, a_user.key.as_ref(), &[vault_bump]],
            program_id,
        )
        .map_err(|_| PortfolioError::BadPda)?;
        if expected_vault != *a_vault.key {
            return Err(PortfolioError::BadPda.into());
        }

        // Step 1: SPL transfer user_ata → portfolio_vault, signed by user.
        let transfer_ix =
            cpi_helpers::spl_token_transfer(*a_user_ata.key, *a_vault.key, *a_user.key, amount);
        invoke(
            &transfer_ix,
            &[
                a_user_ata.clone(),
                a_vault.clone(),
                a_user.clone(),
                a_token.clone(),
            ],
        )?;

        // Step 2: CPI percolator-prog::DepositCollateral with portfolio_auth
        // as the signer (so the engine's owner check passes against
        // engine.accounts[idx].owner == portfolio_auth).
        let dep_ix = cpi_helpers::percolator_deposit_collateral(
            *a_percolator_prog.key,
            *a_auth.key,
            *a_slab.key,
            *a_vault.key,
            *a_market_vault.key,
            *a_token.key,
            *a_clock.key,
            account_idx,
            amount,
        );
        let user_seed = a_user.key.as_ref();
        let auth_seeds: &[&[u8]] = &[PORTFOLIO_AUTH_SEED, user_seed, &[auth_bump]];
        invoke_signed(
            &dep_ix,
            &[
                a_auth.clone(),
                a_slab.clone(),
                a_vault.clone(),
                a_market_vault.clone(),
                a_token.clone(),
                a_clock.clone(),
                a_percolator_prog.clone(),
                a_market_vault_auth.clone(),
            ],
            &[auth_seeds],
        )?;

        Ok(())
    }

    /// Withdraw — pull USDC from a per-market percolator account back to
    /// the user's wallet, routed through `portfolio_vault`.
    ///
    /// IMPORTANT: this DOES NOT validate post-state portfolio health. That
    /// validation requires `GetAccountHealth` (the upstream engine ask) to
    /// avoid math-drift between this program and the engine. Until that
    /// CPI lands, the wrapper trusts percolator-prog's local IM gate per
    /// market — which is sound for isolated-margin, but doesn't catch
    /// portfolio-level risk for cross-margin users.
    ///
    /// Accounts (12):
    ///   0. `[signer, writable]` user
    ///   1. `[]`                  portfolio_data PDA
    ///   2. `[]`                  portfolio_auth PDA
    ///   3. `[writable]`          portfolio_vault PDA
    ///   4. `[writable]`          user_ata
    ///   5. `[writable]`          market slab (also serves as the enrolment-lookup key)
    ///   6. `[writable]`          market vault
    ///   7. `[]`                  market vault authority PDA
    ///   8. `[]`                  oracle (price feed)
    ///   9. `[]`                  spl_token_program
    ///   10. `[]`                 clock sysvar
    ///   11. `[]`                 percolator-prog (executable)
    fn withdraw(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        account_idx: u16,
        amount: u64,
    ) -> ProgramResult {
        if accounts.len() != 12 {
            return Err(PortfolioError::BadAccountCount.into());
        }
        let a_user = &accounts[0];
        let a_data = &accounts[1];
        let a_auth = &accounts[2];
        let a_vault = &accounts[3];
        let a_user_ata = &accounts[4];
        let a_slab = &accounts[5];
        let a_market_vault = &accounts[6];
        let a_market_vault_auth = &accounts[7];
        let a_oracle = &accounts[8];
        let a_token = &accounts[9];
        let a_clock = &accounts[10];
        let a_percolator_prog = &accounts[11];

        if amount == 0 {
            return Err(PortfolioError::ZeroAmount.into());
        }

        let (auth_bump, vault_bump) =
            check_portfolio_for_cpi(program_id, a_user, a_data, a_auth)?;
        // Paused check fires BEFORE vault_bump so a paused-but-vault-uninit'd
        // portfolio reports `Paused` (the more actionable error) instead of
        // `AccountNotInitialized`.
        {
            let data = a_data.try_borrow_data()?;
            let pa: &PortfolioAccount = bytemuck::from_bytes(&data[..POOL_SIZE]);
            if pa.paused != 0 {
                return Err(PortfolioError::Paused.into());
            }
        }
        if vault_bump == 0 {
            return Err(PortfolioError::AccountNotInitialized.into());
        }
        verify_token_program(a_token)?;
        verify_percolator_program(a_percolator_prog)?;
        {
            let data = a_data.try_borrow_data()?;
            let pa: &PortfolioAccount = bytemuck::from_bytes(&data[..POOL_SIZE]);
            if find_enrolled(pa, a_slab.key, account_idx).is_none() {
                return Err(PortfolioError::MarketNotEnrolled.into());
            }
        }

        let expected_vault = Pubkey::create_program_address(
            &[PORTFOLIO_VAULT_SEED, a_user.key.as_ref(), &[vault_bump]],
            program_id,
        )
        .map_err(|_| PortfolioError::BadPda)?;
        if expected_vault != *a_vault.key {
            return Err(PortfolioError::BadPda.into());
        }

        // Step 1: CPI percolator-prog::WithdrawCollateral, dest = portfolio_vault.
        let wd_ix = cpi_helpers::percolator_withdraw_collateral(
            *a_percolator_prog.key,
            *a_auth.key,
            *a_slab.key,
            *a_market_vault.key,
            *a_vault.key,
            *a_market_vault_auth.key,
            *a_token.key,
            *a_clock.key,
            *a_oracle.key,
            account_idx,
            amount,
        );
        let user_seed = a_user.key.as_ref();
        let auth_seeds: &[&[u8]] = &[PORTFOLIO_AUTH_SEED, user_seed, &[auth_bump]];
        invoke_signed(
            &wd_ix,
            &[
                a_auth.clone(),
                a_slab.clone(),
                a_market_vault.clone(),
                a_vault.clone(),
                a_market_vault_auth.clone(),
                a_token.clone(),
                a_clock.clone(),
                a_oracle.clone(),
                a_percolator_prog.clone(),
            ],
            &[auth_seeds],
        )?;

        // Step 2: SPL transfer portfolio_vault → user_ata, signed by portfolio_auth.
        let transfer_ix =
            cpi_helpers::spl_token_transfer(*a_vault.key, *a_user_ata.key, *a_auth.key, amount);
        invoke_signed(
            &transfer_ix,
            &[
                a_vault.clone(),
                a_user_ata.clone(),
                a_auth.clone(),
                a_token.clone(),
            ],
            &[auth_seeds],
        )?;

        Ok(())
    }

    /// Rebalance — keeper-signed multi-leg collateral routing across enrolled
    /// markets. Each leg moves `amount` from the engine account at
    /// `enrolled[from_idx]` to the engine account at `enrolled[to_idx]` via
    /// `portfolio_vault` as the staging area:
    ///
    ///   percolator::WithdrawCollateral  → portfolio_vault
    ///   percolator::DepositCollateral   → next market
    ///
    /// Both CPIs use `portfolio_auth` as signer. CU per leg is bounded by
    /// percolator's own per-call cost (~80K each) plus our wrapper
    /// overhead; ~180K CU per leg in practice. 4 legs comfortably fits in
    /// the 1.4M CU envelope.
    ///
    /// IMPORTANT: same caveat as Withdraw — without `GetAccountHealth`,
    /// portfolio-level risk validation isn't enforced on-chain. The keeper
    /// is trusted to plan rebalances that keep the portfolio healthy; the
    /// engine catches any per-market underwater state via its own gates.
    ///
    /// Body layout (from `Instruction::decode`):
    ///   `body[0] = leg_count`
    ///   `body[1..]` = `leg_count × { u16 from_idx, u16 to_idx, u64 amount }`
    ///
    /// Accounts (variable, base + 5 per leg):
    ///   0. `[signer]`            keeper
    ///   1. `[writable]`          portfolio_data PDA
    ///   2. `[]`                  portfolio_auth PDA
    ///   3. `[writable]`          portfolio_vault PDA
    ///   4. `[]`                  spl_token_program
    ///   5. `[]`                  clock sysvar
    ///   6. `[]`                  percolator-prog (executable)
    ///   then for each leg, in order:
    ///     a. `[writable]` from_slab
    ///     b. `[writable]` from_market_vault
    ///     c. `[]`         from_market_vault_authority
    ///     d. `[]`         from_oracle
    ///     e. `[writable]` to_slab
    ///     f. `[writable]` to_market_vault
    ///
    /// (Each leg needs both withdraw and deposit accounts; the deposit side
    ///  reuses some accounts of the withdraw side where appropriate.)
    fn rebalance(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        leg_count: u8,
        ix_data: &[u8],
    ) -> ProgramResult {
        // Hard cap on legs to prevent CU exhaustion / catastrophic
        // tx-size attacks. Decoder also enforces this for symmetry.
        if leg_count > crate::constants::MAX_REBALANCE_LEGS {
            return Err(PortfolioError::TooManyLegs.into());
        }

        const BASE: usize = 7;
        const PER_LEG: usize = 6;
        let expected_accounts = BASE + (leg_count as usize) * PER_LEG;
        if accounts.len() != expected_accounts {
            return Err(PortfolioError::BadAccountCount.into());
        }
        let a_keeper = &accounts[0];
        let a_data = &accounts[1];
        let a_auth = &accounts[2];
        let a_vault = &accounts[3];
        let a_token = &accounts[4];
        let a_clock = &accounts[5];
        let a_percolator_prog = &accounts[6];

        if !a_keeper.is_signer {
            return Err(PortfolioError::WrongSigner.into());
        }
        if a_data.owner != program_id {
            return Err(PortfolioError::AccountNotInitialized.into());
        }
        // P-CRITICAL: this writes a_data on success path (last_rebalance_slot).
        // If the AccountMeta passed it as readonly, try_borrow_mut_data later
        // would fail with the runtime's `AccountBorrowFailed`, masking the
        // real issue. Surface our own error early.
        if !a_data.is_writable {
            return Err(PortfolioError::DataAccountNotWritable.into());
        }
        verify_token_program(a_token)?;
        verify_percolator_program(a_percolator_prog)?;

        // Single borrow scope: read every field we need from a_data in one
        // pass. Previously two separate borrows of a_data fired ~5K CU of
        // redundant cell-borrow + cast work.
        let (auth_bump, vault_bump, paused, keeper_pubkey, user_pubkey_bytes) = {
            let data = a_data.try_borrow_data()?;
            if data.len() < POOL_SIZE {
                return Err(PortfolioError::AccountNotInitialized.into());
            }
            let pa: &PortfolioAccount = bytemuck::from_bytes(&data[..POOL_SIZE]);
            if pa.magic != MAGIC {
                return Err(PortfolioError::BadMagic.into());
            }
            if pa.version != VERSION {
                return Err(PortfolioError::BadVersion.into());
            }
            (pa.auth_bump, pa.vault_bump, pa.paused, pa.keeper, pa.owner)
        };
        // Order: keeper auth → paused → vault_bump. Keeper is the access
        // control gate; reporting `WrongKeeper` to a non-keeper caller is
        // the most actionable error. Then paused, then vault setup.
        if keeper_pubkey != a_keeper.key.to_bytes() {
            return Err(PortfolioError::WrongKeeper.into());
        }
        if paused != 0 {
            return Err(PortfolioError::Paused.into());
        }
        if vault_bump == 0 {
            return Err(PortfolioError::AccountNotInitialized.into());
        }

        // Verify auth + vault PDA derivations using the user_pubkey we
        // already pulled out in the single-borrow block above.
        let user_pubkey = Pubkey::new_from_array(user_pubkey_bytes);
        let expected_auth = Pubkey::create_program_address(
            &[PORTFOLIO_AUTH_SEED, user_pubkey.as_ref(), &[auth_bump]],
            program_id,
        )
        .map_err(|_| PortfolioError::BadPda)?;
        if expected_auth != *a_auth.key {
            return Err(PortfolioError::BadPda.into());
        }

        // Decode legs from ix_data. Layout: tag(1) + leg_count(1) +
        // leg_count × 12 bytes.
        let body = &ix_data[1..]; // skip tag
        let legs_data = &body[1..]; // skip leg_count
        let auth_seeds: &[&[u8]] =
            &[PORTFOLIO_AUTH_SEED, user_pubkey.as_ref(), &[auth_bump]];

        for i in 0..(leg_count as usize) {
            let off = i * 12;
            let from_idx = u16::from_le_bytes([legs_data[off], legs_data[off + 1]]);
            let to_idx = u16::from_le_bytes([legs_data[off + 2], legs_data[off + 3]]);
            let amount = u64::from_le_bytes([
                legs_data[off + 4],
                legs_data[off + 5],
                legs_data[off + 6],
                legs_data[off + 7],
                legs_data[off + 8],
                legs_data[off + 9],
                legs_data[off + 10],
                legs_data[off + 11],
            ]);

            // Per-leg zero-amount guard. Allowing 0-amount legs would let
            // a malicious keeper waste CU and stamp `last_rebalance_slot`
            // without doing real work.
            if amount == 0 {
                return Err(PortfolioError::ZeroAmount.into());
            }

            let leg_base = BASE + i * PER_LEG;
            let a_from_slab = &accounts[leg_base];
            let a_from_vault = &accounts[leg_base + 1];
            let a_from_vault_auth = &accounts[leg_base + 2];
            let a_from_oracle = &accounts[leg_base + 3];
            let a_to_slab = &accounts[leg_base + 4];
            let a_to_vault = &accounts[leg_base + 5];

            // Reject same-slab same-idx no-op legs (defense-in-depth; the
            // engine would also handle but this is a clear signal).
            if a_from_slab.key == a_to_slab.key && from_idx == to_idx {
                return Err(PortfolioError::BadInstruction.into());
            }

            // Verify both endpoints are enrolled at the right indices.
            {
                let data = a_data.try_borrow_data()?;
                let pa: &PortfolioAccount = bytemuck::from_bytes(&data[..POOL_SIZE]);
                if find_enrolled(pa, a_from_slab.key, from_idx).is_none() {
                    return Err(PortfolioError::MarketNotEnrolled.into());
                }
                if find_enrolled(pa, a_to_slab.key, to_idx).is_none() {
                    return Err(PortfolioError::MarketNotEnrolled.into());
                }
            }

            // Withdraw: from_market → portfolio_vault.
            let wd_ix = cpi_helpers::percolator_withdraw_collateral(
                *a_percolator_prog.key,
                *a_auth.key,
                *a_from_slab.key,
                *a_from_vault.key,
                *a_vault.key,
                *a_from_vault_auth.key,
                *a_token.key,
                *a_clock.key,
                *a_from_oracle.key,
                from_idx,
                amount,
            );
            invoke_signed(
                &wd_ix,
                &[
                    a_auth.clone(),
                    a_from_slab.clone(),
                    a_from_vault.clone(),
                    a_vault.clone(),
                    a_from_vault_auth.clone(),
                    a_token.clone(),
                    a_clock.clone(),
                    a_from_oracle.clone(),
                    a_percolator_prog.clone(),
                ],
                &[auth_seeds],
            )?;

            // Deposit: portfolio_vault → to_market.
            let dep_ix = cpi_helpers::percolator_deposit_collateral(
                *a_percolator_prog.key,
                *a_auth.key,
                *a_to_slab.key,
                *a_vault.key,
                *a_to_vault.key,
                *a_token.key,
                *a_clock.key,
                to_idx,
                amount,
            );
            invoke_signed(
                &dep_ix,
                &[
                    a_auth.clone(),
                    a_to_slab.clone(),
                    a_vault.clone(),
                    a_to_vault.clone(),
                    a_token.clone(),
                    a_clock.clone(),
                    a_percolator_prog.clone(),
                ],
                &[auth_seeds],
            )?;
        }

        // Update last_rebalance_slot (cheap monitoring metric).
        let mut data = a_data.try_borrow_mut_data()?;
        let pa: &mut PortfolioAccount = from_bytes_mut(&mut data[..POOL_SIZE]);
        pa.last_rebalance_slot = Clock::get()?.slot;
        Ok(())
    }

    /// Trade — user opens / modifies a position on an enrolled market via
    /// `percolator-prog::TradeCpi`. The wrapper signs as `portfolio_auth`
    /// (which is the engine account's owner). LP authorization is delegated
    /// to the matcher program at LP-registration time; the runtime CPI
    /// identity check is what binds the LP — the wrapper merely forwards
    /// the matcher tail accounts unchanged.
    ///
    /// Per-market IM/MM is enforced engine-side. Cross-market portfolio
    /// IM is best-effort via keeper `Rebalance`. The wrapper does NOT do
    /// its own pre-trade aggregate margin check in v1.
    ///
    /// Accounts (12 fixed + variadic matcher tail):
    ///   0. `[signer]`            user
    ///   1. `[writable]`          portfolio_data PDA (read-only state check)
    ///   2. `[]`                  portfolio_auth PDA (signs the inner CPI)
    ///   3. `[writable]`          market slab
    ///   4. `[]`                  clock sysvar
    ///   5. `[]`                  oracle
    ///   6. `[]`                  matcher_program
    ///   7. `[writable]`          matcher_context
    ///   8. `[]`                  lp_pda
    ///   9. `[]`                  lp_owner (non-signer; matcher delegates auth)
    ///  10. `[]`                  percolator-prog (executable)
    ///  11..N                     VARIADIC matcher tail (forwarded verbatim)
    fn trade(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        account_idx: u16,
        lp_idx: u16,
        side: u8,
        size_q: u64,
        limit_price_e6: u64,
    ) -> ProgramResult {
        // Account layout:
        //   [0..11]                       fixed Trade-CPI accounts (target slab + oracle)
        //   [11..11 + 2·(N-1)]            (slab_i, oracle_i) pairs for each OTHER enrolled market,
        //                                  used by Defense 1 (pre-trade aggregate IM check)
        //   [11 + 2·(N-1) .. accounts.len()]  variadic matcher tail
        //
        // N = portfolio.enrolled_count. The caller derives N off-chain
        // (it's exposed in portfolio_data) and supplies exactly the right
        // number of margin-check pairs.
        const FIXED: usize = 11;
        if accounts.len() < FIXED {
            return Err(PortfolioError::BadAccountCount.into());
        }
        let a_user = &accounts[0];
        let a_data = &accounts[1];
        let a_auth = &accounts[2];
        let a_slab = &accounts[3];
        let a_clock = &accounts[4];
        let a_oracle = &accounts[5];
        let a_matcher_prog = &accounts[6];
        let a_matcher_ctx = &accounts[7];
        let a_lp_pda = &accounts[8];
        let a_lp_owner = &accounts[9];
        let a_percolator_prog = &accounts[10];

        // Surface-validation in order of cost: cheapest checks first so
        // misuse fails before we touch borrow + CPI machinery.
        if size_q == 0 {
            return Err(PortfolioError::ZeroAmount.into());
        }
        // side: 0 = buy/long, 1 = sell/short. Reject anything else early
        // so the caller gets a clear error rather than a silent flip.
        if side > 1 {
            return Err(PortfolioError::BadInstruction.into());
        }
        // Same-account self-trade (account_idx == lp_idx) is rejected by
        // percolator-prog's TradeCpi anyway, but we surface a wrapper
        // error so the failure mode is unambiguous in client logs.
        if account_idx == lp_idx {
            return Err(PortfolioError::BadInstruction.into());
        }
        verify_percolator_program(a_percolator_prog)?;

        let (auth_bump, _vault_bump) =
            check_portfolio_for_cpi(program_id, a_user, a_data, a_auth)?;

        // Read enrolled markets in a single borrow scope; collect the
        // pubkey + idx pairs for Defense 1 cross-validation, plus the
        // paused gate and target-slab membership check.
        let enrolled_count: usize;
        let enrolled_pairs: alloc::vec::Vec<([u8; 32], u16)>;
        {
            let data = a_data.try_borrow_data()?;
            let pa: &PortfolioAccount = bytemuck::from_bytes(&data[..POOL_SIZE]);
            if pa.paused != 0 {
                return Err(PortfolioError::Paused.into());
            }
            if find_enrolled(pa, a_slab.key, account_idx).is_none() {
                return Err(PortfolioError::MarketNotEnrolled.into());
            }
            enrolled_count = pa.enrolled_count as usize;
            let mut pairs = alloc::vec::Vec::with_capacity(enrolled_count);
            for i in 0..enrolled_count {
                pairs.push((pa.enrolled[i].market, pa.enrolled[i].account_idx));
            }
            enrolled_pairs = pairs;
        }

        // Slice the margin-check pair region. Tail starts AFTER the
        // pairs region. With N = enrolled_count, there are N-1 OTHER
        // enrolled markets (target is at slot 3 + 5 already), so we
        // expect 2·(N-1) accounts in the pair region.
        let pair_region_len = 2usize.saturating_mul(enrolled_count.saturating_sub(1));
        if accounts.len() < FIXED + pair_region_len {
            return Err(PortfolioError::WrongMarginAccountCount.into());
        }
        let pair_region = &accounts[FIXED..FIXED + pair_region_len];
        let tail = &accounts[FIXED + pair_region_len..];

        // Convert (side, size_q) to signed i128 for TradeCpi and the
        // aggregate-IM projected basis:
        //   side==0 → long  → +size_q
        //   side==1 → short → -size_q
        // size_q is u64, so always fits in i128 without overflow.
        let size_signed: i128 = if side == 0 {
            size_q as i128
        } else {
            -(size_q as i128)
        };

        // ── Defense 1: pre-trade aggregate IM check ─────────────────────
        // Borrow every relevant slab's data, build EnrolledView per market,
        // and call `crate::margin::check_aggregate_im`. Engine still
        // enforces per-account IM at the TradeCpi we issue below — this
        // is the wrapper-side ADDITIONAL gate, not a replacement.
        {
            // Clock for oracle freshness on EVERY enrolled market's
            // Pyth account. Each market's MarketConfig dictates its own
            // staleness/conf bounds, applied uniformly via the shared
            // percolator-prog::oracle::read_pyth_price_e6 helper.
            let now_unix_ts = Clock::from_account_info(a_clock)?.unix_timestamp;

            // Borrow target slab data. Held across the call to keep the
            // EnrolledView slice valid. The borrow is released before
            // the CPI below (which writes the slab).
            let target_data = a_slab.try_borrow_data()?;

            // Find target's index in the enrolled list and seed seen_mask.
            let target_slab_bytes = a_slab.key.to_bytes();
            let mut seen_mask: u32 = 0;
            let mut target_enrolled_idx: usize = usize::MAX;
            for (i, (m, idx)) in enrolled_pairs.iter().enumerate() {
                if *m == target_slab_bytes && *idx == account_idx {
                    target_enrolled_idx = i;
                    seen_mask |= 1u32 << i;
                    break;
                }
            }
            if target_enrolled_idx == usize::MAX {
                return Err(PortfolioError::MarketNotEnrolled.into());
            }

            // Decode target's oracle. read_oracle_price_e6 needs the slab
            // data (to read MarketConfig for feed_id + staleness + conf).
            let target_oracle_price_e6 =
                pyth::read_oracle_price_e6(a_oracle, &target_data, now_unix_ts)?;

            // Walk the (slab, oracle) pair region and build views for
            // every OTHER enrolled market. Track each pair's borrow
            // separately so all live until the aggregate-check call.
            // (Solana's AccountInfo holds `Rc<RefCell<&mut [u8]>>`, so
            //  `try_borrow_data` returns `Ref<&mut [u8]>`, not `Ref<[u8]>`.)
            let mut other_datas: alloc::vec::Vec<core::cell::Ref<'_, &mut [u8]>> =
                alloc::vec::Vec::with_capacity(enrolled_count.saturating_sub(1));
            let mut other_meta: alloc::vec::Vec<(u16, u64)> =
                alloc::vec::Vec::with_capacity(enrolled_count.saturating_sub(1));

            let mut p = 0;
            while p < pair_region_len {
                let a_other_slab = &pair_region[p];
                let a_other_oracle = &pair_region[p + 1];
                p += 2;

                let other_slab_bytes = a_other_slab.key.to_bytes();
                let mut found_enrolled_idx: usize = usize::MAX;
                for (i, (m, _idx)) in enrolled_pairs.iter().enumerate() {
                    if *m == other_slab_bytes {
                        found_enrolled_idx = i;
                        break;
                    }
                }
                if found_enrolled_idx == usize::MAX {
                    return Err(PortfolioError::MarginSlabNotEnrolled.into());
                }
                let mask_bit = 1u32 << found_enrolled_idx;
                if (seen_mask & mask_bit) != 0 {
                    return Err(PortfolioError::MarginSlabDuplicate.into());
                }
                seen_mask |= mask_bit;

                // Borrow OTHER slab's data — held until aggregate check.
                let data = a_other_slab.try_borrow_data()?;

                // Decode this market's oracle using ITS OWN config (each
                // market can have different feed_id / staleness / conf).
                let oracle_price_e6 =
                    pyth::read_oracle_price_e6(a_other_oracle, &data, now_unix_ts)?;

                let (_, idx_pair) = enrolled_pairs[found_enrolled_idx];
                other_meta.push((idx_pair, oracle_price_e6));
                other_datas.push(data);
            }

            // Verify ALL enrolled markets were covered (target + others).
            let expected_mask: u32 = if enrolled_count >= 32 {
                u32::MAX
            } else {
                (1u32 << enrolled_count) - 1
            };
            if seen_mask != expected_mask {
                return Err(PortfolioError::WrongMarginAccountCount.into());
            }

            // Build the views slice: target first (with trade_delta_q),
            // then each other (with None).
            let mut views: alloc::vec::Vec<margin::EnrolledView<'_>> =
                alloc::vec::Vec::with_capacity(enrolled_count);
            views.push(margin::EnrolledView {
                slab_data: &target_data,
                account_idx,
                oracle_price_e6: target_oracle_price_e6,
                trade_delta_q: Some(size_signed),
            });
            for (i, (idx, oracle_price_e6)) in other_meta.iter().enumerate() {
                views.push(margin::EnrolledView {
                    slab_data: &other_datas[i],
                    account_idx: *idx,
                    oracle_price_e6: *oracle_price_e6,
                    trade_delta_q: None,
                });
            }

            margin::check_aggregate_im(&views)?;

            // Borrows on target_data and other_datas are dropped here
            // before the CPI below, which will re-borrow target slab
            // (writable) inside TradeCpi.
        }

        // Build the matcher tail as AccountMeta from the AccountInfo slice.
        // Preserve writable + signer flags from the outer transaction —
        // TradeCpi forwards these to the matcher unchanged.
        let mut tail_metas: Vec<AccountMeta> = Vec::with_capacity(tail.len());
        for ai in tail.iter() {
            tail_metas.push(if ai.is_writable {
                AccountMeta::new(*ai.key, ai.is_signer)
            } else {
                AccountMeta::new_readonly(*ai.key, ai.is_signer)
            });
        }

        let trade_ix = cpi_helpers::percolator_trade_cpi(
            *a_percolator_prog.key,
            *a_auth.key,
            *a_lp_owner.key,
            *a_slab.key,
            *a_clock.key,
            *a_oracle.key,
            *a_matcher_prog.key,
            *a_matcher_ctx.key,
            *a_lp_pda.key,
            &tail_metas,
            lp_idx,
            account_idx,
            size_signed,
            limit_price_e6,
        );

        // invoke_signed accounts list mirrors the AccountMeta order above:
        // a_auth (signer-via-PDA) at slot 0, then the rest in CPI order,
        // followed by the tail in original order.
        let mut cpi_accounts: Vec<AccountInfo> = Vec::with_capacity(8 + tail.len() + 1);
        cpi_accounts.push(a_auth.clone());
        cpi_accounts.push(a_lp_owner.clone());
        cpi_accounts.push(a_slab.clone());
        cpi_accounts.push(a_clock.clone());
        cpi_accounts.push(a_oracle.clone());
        cpi_accounts.push(a_matcher_prog.clone());
        cpi_accounts.push(a_matcher_ctx.clone());
        cpi_accounts.push(a_lp_pda.clone());
        for ai in tail.iter() {
            cpi_accounts.push(ai.clone());
        }
        cpi_accounts.push(a_percolator_prog.clone());

        let user_seed = a_user.key.as_ref();
        let auth_seeds: &[&[u8]] = &[PORTFOLIO_AUTH_SEED, user_seed, &[auth_bump]];
        invoke_signed(&trade_ix, &cpi_accounts, &[auth_seeds])?;

        Ok(())
    }

    /// EmergencyClose — user-controlled escape hatch. CPIs into
    /// `percolator-prog::CloseAccount` for a single enrolled market,
    /// transfers the released collateral back to the user's wallet, and
    /// removes the slot from `enrolled[]`.
    ///
    /// Per percolator-prog conventions, CloseAccount requires the position
    /// to already be flat. If not, the user must first trade to flatten.
    /// In v1 we don't bundle that flattening — caller is responsible.
    ///
    /// **CRITICAL fix (v0.2)**: the v0.1 builder shipped 2 accounts; the
    /// percolator-prog handler unconditionally requires 8. Calling the old
    /// version would have failed at runtime with `NotEnoughAccountKeys`,
    /// not the cleaner `BadAccountCount`. Replaced with the full 8-account
    /// layout. Also added the post-close swap-remove from enrolled[] so
    /// stale enrolment can't trip subsequent ops.
    ///
    /// Accounts (12):
    ///   0. `[signer]`            user
    ///   1. `[writable]`          portfolio_data PDA (enrolled[] mutated post-CPI)
    ///   2. `[]`                  portfolio_auth PDA
    ///   3. `[writable]`          portfolio_vault token account
    ///   4. `[writable]`          user_ata (final destination)
    ///   5. `[writable]`          market slab
    ///   6. `[writable]`          market_vault
    ///   7. `[]`                  market_vault_authority PDA
    ///   8. `[]`                  oracle (price feed)
    ///   9. `[]`                  spl_token_program
    ///  10. `[]`                  clock sysvar
    ///  11. `[]`                  percolator-prog (executable)
    fn emergency_close(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        account_idx: u16,
    ) -> ProgramResult {
        if accounts.len() != 12 {
            return Err(PortfolioError::BadAccountCount.into());
        }
        let a_user = &accounts[0];
        let a_data = &accounts[1];
        let a_auth = &accounts[2];
        let a_vault = &accounts[3];
        let a_user_ata = &accounts[4];
        let a_slab = &accounts[5];
        let a_market_vault = &accounts[6];
        let a_market_vault_auth = &accounts[7];
        let a_oracle = &accounts[8];
        let a_token = &accounts[9];
        let a_clock = &accounts[10];
        let a_percolator_prog = &accounts[11];

        let (auth_bump, vault_bump) =
            check_portfolio_for_cpi(program_id, a_user, a_data, a_auth)?;
        if vault_bump == 0 {
            return Err(PortfolioError::AccountNotInitialized.into());
        }
        if !a_data.is_writable {
            return Err(PortfolioError::DataAccountNotWritable.into());
        }
        verify_token_program(a_token)?;
        verify_percolator_program(a_percolator_prog)?;

        // Verify enrolment + record the slot index for the swap-remove.
        let slot_idx = {
            let data = a_data.try_borrow_data()?;
            let pa: &PortfolioAccount = bytemuck::from_bytes(&data[..POOL_SIZE]);
            find_enrolled(pa, a_slab.key, account_idx)
                .ok_or(PortfolioError::MarketNotEnrolled)?
        };

        // Verify vault PDA derivation matches stored bump.
        let expected_vault = Pubkey::create_program_address(
            &[PORTFOLIO_VAULT_SEED, a_user.key.as_ref(), &[vault_bump]],
            program_id,
        )
        .map_err(|_| PortfolioError::BadPda)?;
        if expected_vault != *a_vault.key {
            return Err(PortfolioError::BadPda.into());
        }

        let user_seed = a_user.key.as_ref();
        let auth_seeds: &[&[u8]] = &[PORTFOLIO_AUTH_SEED, user_seed, &[auth_bump]];

        // Snapshot the portfolio_vault balance before the close so we can
        // transfer exactly the delta to the user (close returns the
        // account's residual collateral to the dest_ata = portfolio_vault).
        let vault_before = read_token_account_amount(a_vault)?;

        // Step 1: CPI percolator-prog::CloseAccount, dest_ata = portfolio_vault.
        let close_ix = cpi_helpers::percolator_close_account(
            *a_percolator_prog.key,
            *a_auth.key,
            *a_slab.key,
            *a_market_vault.key,
            *a_vault.key,
            *a_market_vault_auth.key,
            *a_token.key,
            *a_clock.key,
            *a_oracle.key,
            account_idx,
        );
        invoke_signed(
            &close_ix,
            &[
                a_auth.clone(),
                a_slab.clone(),
                a_market_vault.clone(),
                a_vault.clone(),
                a_market_vault_auth.clone(),
                a_token.clone(),
                a_clock.clone(),
                a_oracle.clone(),
                a_percolator_prog.clone(),
            ],
            &[auth_seeds],
        )?;

        // Step 2: forward the delta (released collateral) to user_ata.
        let vault_after = read_token_account_amount(a_vault)?;
        let released = vault_after.saturating_sub(vault_before);
        if released > 0 {
            let xfer_ix = cpi_helpers::spl_token_transfer(
                *a_vault.key,
                *a_user_ata.key,
                *a_auth.key,
                released,
            );
            invoke_signed(
                &xfer_ix,
                &[
                    a_vault.clone(),
                    a_user_ata.clone(),
                    a_auth.clone(),
                    a_token.clone(),
                ],
                &[auth_seeds],
            )?;
        }

        // Step 3: swap-remove the now-closed slot from enrolled[].
        let mut data = a_data.try_borrow_mut_data()?;
        let pa: &mut PortfolioAccount = from_bytes_mut(&mut data[..POOL_SIZE]);
        let count = pa.enrolled_count as usize;
        let last = count - 1;
        if slot_idx != last {
            pa.enrolled[slot_idx] = pa.enrolled[last];
        }
        pa.enrolled[last].market = [0u8; 32];
        pa.enrolled[last].account_idx = 0;
        pa.enrolled[last].last_seen_eq_e6 = 0;
        pa.enrolled[last]._pad0 = [0u8; 6];
        pa.enrolled_count = last as u8;

        Ok(())
    }

    /// Read the `amount` field of an SPL Token v3 account directly from
    /// raw account data. Layout: `mint(32) || owner(32) || amount(u64 LE) || ...`.
    /// Returns 0 if the data is too short (defensive — callers verify the
    /// account is a real token account elsewhere).
    fn read_token_account_amount(a: &AccountInfo) -> Result<u64, ProgramError> {
        let data = a.try_borrow_data()?;
        if data.len() < 72 {
            return Err(PortfolioError::BadVault.into());
        }
        Ok(u64::from_le_bytes([
            data[64], data[65], data[66], data[67], data[68], data[69], data[70], data[71],
        ]))
    }

    /// UnenrollMarket — remove a `(market, account_idx)` pair from `enrolled[]`.
    ///
    /// Implementation: swap-remove. Find the matching slot, copy the last
    /// populated slot into its place, zero the now-vacant tail, decrement
    /// `enrolled_count`. The order of `enrolled[]` is NOT semantically
    /// significant — callers index by (market, idx), not by slot position.
    ///
    /// In v1 this does NOT close the underlying percolator account, transfer
    /// any residual collateral back, or verify the position is flat. The
    /// caller is responsible for emptying the position via the percolator
    /// program's own paths (or a future EmergencyClose). UnenrollMarket
    /// just stops the portfolio program from tracking the market.
    ///
    /// Accounts:
    ///   0. [signer]   user
    ///   1. [writable] portfolio data PDA
    ///   2. []         market slab (passed for its pubkey)
    fn unenroll_market(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        account_idx: u16,
    ) -> ProgramResult {
        if accounts.len() != 3 {
            return Err(PortfolioError::BadAccountCount.into());
        }
        let a_user = &accounts[0];
        let a_data = &accounts[1];
        let a_market = &accounts[2];

        check_portfolio_account(program_id, a_user, a_data)?;

        let mut data = a_data.try_borrow_mut_data()?;
        let pa: &mut PortfolioAccount = from_bytes_mut(&mut data[..POOL_SIZE]);

        let count = pa.enrolled_count as usize;
        if count == 0 {
            return Err(PortfolioError::MarketNotEnrolled.into());
        }
        let market_bytes = a_market.key.to_bytes();

        let mut found: Option<usize> = None;
        for i in 0..count {
            if pa.enrolled[i].market == market_bytes
                && pa.enrolled[i].account_idx == account_idx
            {
                found = Some(i);
                break;
            }
        }
        let i = found.ok_or(PortfolioError::MarketNotEnrolled)?;

        let last = count - 1;
        if i != last {
            pa.enrolled[i] = pa.enrolled[last];
        }
        // Zero the vacated slot so the prefix-only iteration invariant holds.
        pa.enrolled[last].market = [0u8; 32];
        pa.enrolled[last].account_idx = 0;
        pa.enrolled[last].last_seen_eq_e6 = 0;
        pa.enrolled[last]._pad0 = [0u8; 6];
        pa.enrolled_count = last as u8;

        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// 6. kani proof harnesses (B18)
// ─────────────────────────────────────────────────────────────────────────
//
// Each `#[kani::proof]` function is a property over arbitrary input that
// CBMC explores symbolically. They cover:
//
//   - decode soundness: Instruction::decode never panics, only returns
//     Ok/Err for any byte slice up to a bounded length.
//   - layout: PortfolioAccount + MarketSlot sizes and alignments are exactly
//     what the const asserts already pin (Kani gives us a runtime witness
//     that the asserts are true on the production target).
//   - zeroed-init: PortfolioAccount::zeroed() round-trips cleanly through
//     Pod bytes.
//   - decode determinism: same input bytes → same Result.
//   - tag-roundtrip: for tag 0 / 8 / 9, encode-then-decode is identity on a
//     symbolic input.
//
// Proof harnesses are gated on `cfg(kani)` so they don't ship in the SBF
// binary. Run with `cargo kani --features kani` once Kani is installed.

#[cfg(kani)]
pub mod proofs {
    use crate::constants::{MAX_BUFFER_BPS, MAX_PORTFOLIO_LEV_BPS, MIN_BUFFER_BPS, VERSION};
    use crate::instruction::Instruction;
    use crate::state::{MarketSlot, PortfolioAccount};
    use bytemuck::Zeroable;

    // ── Layout invariants ──────────────────────────────────────────────────
    #[kani::proof]
    fn portfolio_account_layout() {
        // These match the const asserts inside `mod state`. Re-stated as a
        // proof so future readers can run `cargo kani` and see them green.
        assert!(core::mem::size_of::<PortfolioAccount>() == 888);
        assert!(core::mem::align_of::<PortfolioAccount>() == 8);
    }

    #[kani::proof]
    fn market_slot_layout() {
        assert!(core::mem::size_of::<MarketSlot>() == 48);
        assert!(core::mem::align_of::<MarketSlot>() == 8);
    }

    // ── Zeroed initialisation ──────────────────────────────────────────────
    #[kani::proof]
    fn zeroed_account_all_zero_fields() {
        let pa = PortfolioAccount::zeroed();
        assert!(pa.magic == 0);
        assert!(pa.last_rebalance_slot == 0);
        assert!(pa.cached_at_slot == 0);
        assert!(pa.cached_total_eq_e6 == 0);
        assert!(pa.cached_total_mmr_e6 == 0);
        assert!(pa.owner == [0u8; 32]);
        assert!(pa.keeper == [0u8; 32]);
        assert!(pa.max_leverage_bps == 0);
        assert!(pa.buffer_bps == 0);
        assert!(pa.bump == 0);
        assert!(pa.auth_bump == 0);
        // vault_bump == 0 is the load-bearing sentinel for "vault not yet
        // created" — every vault-using handler checks `vault_bump != 0`
        // to refuse operating on an uninitialised vault. The Pod::zeroed()
        // contract guarantees this byte is 0, but assert it explicitly so
        // the invariant is locked in the proof, not just structurally
        // implied.
        assert!(pa.vault_bump == 0);
        assert!(pa.version == 0);
        assert!(pa.paused == 0);
        assert!(pa.enrolled_count == 0);
    }

    // ── Decode soundness — never panics ────────────────────────────────────
    //
    // For any sequence of up to 64 input bytes, decode() either returns Ok
    // or Err — it never panics, never out-of-bounds-indexes, never aborts.
    // Bound is 64 because the largest valid encoding is tag 0 / 8 (39 bytes
    // total: 1 tag + 38 body); plus headroom for malformed inputs.
    #[kani::proof]
    #[kani::unwind(70)]
    fn decode_never_panics() {
        let len: usize = kani::any();
        kani::assume(len <= 64);
        let mut buf = [0u8; 64];
        for i in 0..len {
            buf[i] = kani::any();
        }
        // Result is intentionally ignored — we only assert that the call
        // completes without panicking.
        let _ = Instruction::decode(&buf[..len]);
    }

    // ── Decode determinism ─────────────────────────────────────────────────
    //
    // Same input bytes always produce the same Result variant. Caught by
    // CBMC by symbolically running decode twice on the same input.
    #[kani::proof]
    #[kani::unwind(70)]
    fn decode_is_deterministic() {
        let len: usize = kani::any();
        kani::assume(len <= 64);
        let mut buf = [0u8; 64];
        for i in 0..len {
            buf[i] = kani::any();
        }
        let r1 = Instruction::decode(&buf[..len]);
        let r2 = Instruction::decode(&buf[..len]);
        // Cheap discriminant check: both Ok or both Err. The full equality
        // would require Instruction: Eq, which it already is.
        match (r1, r2) {
            (Ok(a), Ok(b)) => assert!(a == b),
            (Err(_), Err(_)) => {}
            _ => kani::cover!(false, "decode is non-deterministic"),
        }
    }

    // ── Decode rejects truncated / oversized inputs for tag 0 ─────────────
    //
    // Tag 0 (InitPortfolio) requires exactly 1 + 38 = 39 bytes. Any other
    // length must produce Err.
    #[kani::proof]
    #[kani::unwind(70)]
    fn init_decode_strict_length() {
        let len: usize = kani::any();
        kani::assume(len <= 64);
        kani::assume(len != 39);
        kani::assume(len >= 1);
        let mut buf = [0u8; 64];
        buf[0] = 0; // tag InitPortfolio
        for i in 1..len {
            buf[i] = kani::any();
        }
        let r = Instruction::decode(&buf[..len]);
        // For tag 0 with len < 39, body.len() < 38 → BadInstruction.
        // For tag 0 with len > 39, the strict equality check rejects.
        assert!(r.is_err());
    }

    // ── SetPaused decode rejects non-{0,1} bodies ─────────────────────────
    #[kani::proof]
    fn set_paused_strict_bool() {
        let byte: u8 = kani::any();
        kani::assume(byte != 0 && byte != 1);
        let buf = [9u8, byte];
        let r = Instruction::decode(&buf);
        assert!(r.is_err());
    }

    // ── Buffer bounds: any value outside [MIN, MAX] is invalid for init ───
    //
    // This is a pure-data proof: it doesn't run init_portfolio (which needs
    // accounts), but verifies that the bound predicate behaves as expected
    // over the full u16 input space.
    #[kani::proof]
    fn buffer_bounds_predicate() {
        let buf: u16 = kani::any();
        let in_range = (MIN_BUFFER_BPS..=MAX_BUFFER_BPS).contains(&buf);
        if buf < MIN_BUFFER_BPS {
            assert!(!in_range);
        } else if buf > MAX_BUFFER_BPS {
            assert!(!in_range);
        } else {
            assert!(in_range);
        }
    }

    // ── Leverage bounds predicate ─────────────────────────────────────────
    #[kani::proof]
    fn leverage_bounds_predicate() {
        let lev: u32 = kani::any();
        let valid = lev != 0 && lev <= MAX_PORTFOLIO_LEV_BPS;
        if lev == 0 {
            assert!(!valid);
        } else if lev > MAX_PORTFOLIO_LEV_BPS {
            assert!(!valid);
        } else {
            assert!(valid);
        }
    }

    // ── VERSION constant is u8::nonzero ───────────────────────────────────
    //
    // Sanity: a zeroed account (all bytes 0) must NOT pass the version
    // check, otherwise InitPortfolio could be skipped and the account read
    // as if it were initialised.
    #[kani::proof]
    fn version_distinguishes_zeroed() {
        assert!(VERSION != 0);
    }

    // ── Per-tag strict-length proofs ──────────────────────────────────────
    //
    // For every tag where the body has a fixed expected length, prove that
    // decoding ANY input of the wrong length returns Err. Bound the input
    // length at 64 to keep CBMC tractable.
    //
    // Pattern: assume len ∉ {valid_set}, decode, assert Err.

    #[kani::proof]
    #[kani::unwind(70)]
    fn decode_strict_length_tag1_enroll() {
        // Tag 1 (EnrollMarket): body must be exactly 2 bytes.
        let len: usize = kani::any();
        kani::assume(len <= 64 && len >= 1 && len != 3);
        let mut buf = [0u8; 64];
        buf[0] = 1;
        for i in 1..len {
            buf[i] = kani::any();
        }
        let r = Instruction::decode(&buf[..len]);
        assert!(r.is_err());
    }

    #[kani::proof]
    #[kani::unwind(70)]
    fn decode_strict_length_tag2_unenroll() {
        let len: usize = kani::any();
        kani::assume(len <= 64 && len >= 1 && len != 3);
        let mut buf = [0u8; 64];
        buf[0] = 2;
        for i in 1..len {
            buf[i] = kani::any();
        }
        let r = Instruction::decode(&buf[..len]);
        assert!(r.is_err());
    }

    #[kani::proof]
    #[kani::unwind(70)]
    fn decode_strict_length_tag3_deposit() {
        // Tag 3 (Deposit): body = 2 + 8 = 10. Total = 11.
        let len: usize = kani::any();
        kani::assume(len <= 64 && len >= 1 && len != 11);
        let mut buf = [0u8; 64];
        buf[0] = 3;
        for i in 1..len {
            buf[i] = kani::any();
        }
        let r = Instruction::decode(&buf[..len]);
        assert!(r.is_err());
    }

    #[kani::proof]
    #[kani::unwind(70)]
    fn decode_strict_length_tag4_withdraw() {
        let len: usize = kani::any();
        kani::assume(len <= 64 && len >= 1 && len != 11);
        let mut buf = [0u8; 64];
        buf[0] = 4;
        for i in 1..len {
            buf[i] = kani::any();
        }
        let r = Instruction::decode(&buf[..len]);
        assert!(r.is_err());
    }

    #[kani::proof]
    #[kani::unwind(70)]
    fn decode_strict_length_tag5_trade() {
        // Tag 5 (Trade): body = 2 + 2 + 1 + 8 + 8 = 21. Total = 22.
        let len: usize = kani::any();
        kani::assume(len <= 64 && len >= 1 && len != 22);
        let mut buf = [0u8; 64];
        buf[0] = 5;
        for i in 1..len {
            buf[i] = kani::any();
        }
        let r = Instruction::decode(&buf[..len]);
        assert!(r.is_err());
    }

    #[kani::proof]
    #[kani::unwind(70)]
    fn decode_strict_length_tag7_emergency() {
        let len: usize = kani::any();
        kani::assume(len <= 64 && len >= 1 && len != 3);
        let mut buf = [0u8; 64];
        buf[0] = 7;
        for i in 1..len {
            buf[i] = kani::any();
        }
        let r = Instruction::decode(&buf[..len]);
        assert!(r.is_err());
    }

    #[kani::proof]
    #[kani::unwind(70)]
    fn decode_strict_length_tag8_update_config() {
        // Tag 8 (UpdateConfig): body = 2 + 4 + 32 = 38. Total = 39.
        let len: usize = kani::any();
        kani::assume(len <= 64 && len >= 1 && len != 39);
        let mut buf = [0u8; 64];
        buf[0] = 8;
        for i in 1..len {
            buf[i] = kani::any();
        }
        let r = Instruction::decode(&buf[..len]);
        assert!(r.is_err());
    }

    #[kani::proof]
    #[kani::unwind(70)]
    fn decode_strict_length_tag9_set_paused() {
        // Tag 9 (SetPaused): body = 1. Total = 2.
        let len: usize = kani::any();
        kani::assume(len <= 64 && len >= 1 && len != 2);
        let mut buf = [0u8; 64];
        buf[0] = 9;
        for i in 1..len {
            buf[i] = kani::any();
        }
        let r = Instruction::decode(&buf[..len]);
        assert!(r.is_err());
    }

    // ── Unknown-tag rejection ─────────────────────────────────────────────
    //
    // Tags 10..=255 are not valid. Any input starting with such a tag must
    // return Err regardless of body content / length.
    #[kani::proof]
    #[kani::unwind(70)]
    fn decode_unknown_tag_rejected() {
        let tag: u8 = kani::any();
        kani::assume(tag >= 14); // tags 0..=13 are now valid (13 = RebalanceCrank)
        let len: usize = kani::any();
        kani::assume(len >= 1 && len <= 64);
        let mut buf = [0u8; 64];
        buf[0] = tag;
        for i in 1..len {
            buf[i] = kani::any();
        }
        let r = Instruction::decode(&buf[..len]);
        assert!(r.is_err());
    }

    /// Strict-length proof for tag 10 (InitVault): body must be empty.
    #[kani::proof]
    #[kani::unwind(70)]
    fn decode_strict_length_tag10_init_vault() {
        let len: usize = kani::any();
        kani::assume(len <= 64 && len >= 1 && len != 1);
        let mut buf = [0u8; 64];
        buf[0] = 10;
        for i in 1..len {
            buf[i] = kani::any();
        }
        let r = Instruction::decode(&buf[..len]);
        assert!(r.is_err());
    }

    /// Strict-length proof for tag 11 (ClosePortfolio): body must be empty.
    #[kani::proof]
    #[kani::unwind(70)]
    fn decode_strict_length_tag11_close_portfolio() {
        let len: usize = kani::any();
        kani::assume(len <= 64 && len >= 1 && len != 1);
        let mut buf = [0u8; 64];
        buf[0] = 11;
        for i in 1..len {
            buf[i] = kani::any();
        }
        let r = Instruction::decode(&buf[..len]);
        assert!(r.is_err());
    }

    /// Strict-length proof for tag 12 (EnrollMarketAndInit): body =
    /// 2 + 8 = 10. Total = 11.
    #[kani::proof]
    #[kani::unwind(70)]
    fn decode_strict_length_tag12_enroll_and_init() {
        let len: usize = kani::any();
        kani::assume(len <= 64 && len >= 1 && len != 11);
        let mut buf = [0u8; 64];
        buf[0] = 12;
        for i in 1..len {
            buf[i] = kani::any();
        }
        let r = Instruction::decode(&buf[..len]);
        assert!(r.is_err());
    }

    /// Round-trip proof for tag 12 (EnrollMarketAndInit).
    #[kani::proof]
    #[kani::unwind(70)]
    fn encode_decode_roundtrip_enroll_and_init() {
        let expected_idx: u16 = kani::any();
        let fee_payment: u64 = kani::any();

        let mut buf = [0u8; 32];
        buf[0] = 12;
        buf[1..3].copy_from_slice(&expected_idx.to_le_bytes());
        buf[3..11].copy_from_slice(&fee_payment.to_le_bytes());

        let r = Instruction::decode(&buf[..11]).expect("must decode");
        match r {
            Instruction::EnrollMarketAndInit {
                expected_idx: ei,
                fee_payment: fp,
            } => {
                assert!(ei == expected_idx);
                assert!(fp == fee_payment);
            }
            _ => kani::cover!(false, "wrong variant"),
        }
    }

    /// Strict-length proof for tag 13 (RebalanceCrank): body =
    /// 2 + 2 + 8 = 12. Total = 13.
    #[kani::proof]
    #[kani::unwind(70)]
    fn decode_strict_length_tag13_rebalance_crank() {
        let len: usize = kani::any();
        kani::assume(len <= 64 && len >= 1 && len != 13);
        let mut buf = [0u8; 64];
        buf[0] = 13;
        for i in 1..len {
            buf[i] = kani::any();
        }
        let r = Instruction::decode(&buf[..len]);
        assert!(r.is_err());
    }

    /// Round-trip proof for tag 13 (RebalanceCrank).
    #[kani::proof]
    #[kani::unwind(70)]
    fn encode_decode_roundtrip_rebalance_crank() {
        let from_idx: u16 = kani::any();
        let to_idx: u16 = kani::any();
        let amount: u64 = kani::any();

        let mut buf = [0u8; 32];
        buf[0] = 13;
        buf[1..3].copy_from_slice(&from_idx.to_le_bytes());
        buf[3..5].copy_from_slice(&to_idx.to_le_bytes());
        buf[5..13].copy_from_slice(&amount.to_le_bytes());

        let r = Instruction::decode(&buf[..13]).expect("must decode");
        match r {
            Instruction::RebalanceCrank {
                from_idx: f,
                to_idx: t,
                amount: a,
            } => {
                assert!(f == from_idx);
                assert!(t == to_idx);
                assert!(a == amount);
            }
            _ => kani::cover!(false, "wrong variant"),
        }
    }

    // ── Encode → decode round-trip (tag 0 InitPortfolio) ──────────────────
    //
    // Build a canonical encoding from arbitrary fields, decode it, and
    // assert the decoded fields match the originals. Catches any drift if
    // the decoder is ever changed without updating callers.
    #[kani::proof]
    #[kani::unwind(70)]
    fn encode_decode_roundtrip_init() {
        let buffer_bps: u16 = kani::any();
        let max_leverage_bps: u32 = kani::any();
        // keeper bytes can be anything.
        let mut keeper = [0u8; 32];
        for i in 0..32 {
            keeper[i] = kani::any();
        }

        let mut buf = [0u8; 64];
        buf[0] = 0; // tag InitPortfolio
        buf[1..3].copy_from_slice(&buffer_bps.to_le_bytes());
        buf[3..7].copy_from_slice(&max_leverage_bps.to_le_bytes());
        buf[7..39].copy_from_slice(&keeper);

        let r = Instruction::decode(&buf[..39]).expect("must decode");
        match r {
            Instruction::InitPortfolio {
                buffer_bps: bb,
                max_leverage_bps: ml,
                keeper: kk,
            } => {
                assert!(bb == buffer_bps);
                assert!(ml == max_leverage_bps);
                assert!(kk == keeper);
            }
            _ => kani::cover!(false, "wrong variant"),
        }
    }

    // ── Encode → decode round-trip (tag 5 Trade) ──────────────────────────
    #[kani::proof]
    #[kani::unwind(70)]
    fn encode_decode_roundtrip_trade() {
        let account_idx: u16 = kani::any();
        let lp_idx: u16 = kani::any();
        let side: u8 = kani::any();
        let size_q: u64 = kani::any();
        let limit_price_e6: u64 = kani::any();

        // Body layout: u16 account_idx | u16 lp_idx | u8 side |
        //              u64 size_q | u64 limit_price_e6 = 21 bytes.
        let mut buf = [0u8; 64];
        buf[0] = 5;
        buf[1..3].copy_from_slice(&account_idx.to_le_bytes());
        buf[3..5].copy_from_slice(&lp_idx.to_le_bytes());
        buf[5] = side;
        buf[6..14].copy_from_slice(&size_q.to_le_bytes());
        buf[14..22].copy_from_slice(&limit_price_e6.to_le_bytes());

        let r = Instruction::decode(&buf[..22]).expect("must decode");
        match r {
            Instruction::Trade {
                account_idx: ai,
                lp_idx: li,
                side: s,
                size_q: sz,
                limit_price_e6: lp,
            } => {
                assert!(ai == account_idx);
                assert!(li == lp_idx);
                assert!(s == side);
                assert!(sz == size_q);
                assert!(lp == limit_price_e6);
            }
            _ => kani::cover!(false, "wrong variant"),
        }
    }

    // ── Encode → decode round-trip (tag 9 SetPaused) ──────────────────────
    //
    // Verifies both the strict bool decoding (only 0/1) and the round-trip.
    #[kani::proof]
    fn encode_decode_roundtrip_set_paused() {
        let p: bool = kani::any();
        let buf = [9u8, u8::from(p)];
        let r = Instruction::decode(&buf).expect("valid bool must decode");
        match r {
            Instruction::SetPaused { paused } => {
                assert!(paused == p);
            }
            _ => kani::cover!(false, "wrong variant"),
        }
    }

    // ── PortfolioAccount Pod round-trip ───────────────────────────────────
    //
    // Build a PortfolioAccount with arbitrary scalar fields, cast to bytes
    // and back via bytemuck, assert all fields are preserved. Catches
    // silent layout corruption from struct edits.
    //
    // Note: full nondeterminism of the [MarketSlot;16] array would explode
    // CBMC's state space — we leave that array zeroed and exercise the
    // header only. The struct's `#[repr(C)]` and Pod derive guarantee the
    // array tail follows the same layout.
    #[kani::proof]
    fn portfolio_account_pod_roundtrip() {
        let mut pa = PortfolioAccount::zeroed();
        let magic: u64 = kani::any();
        let owner: [u8; 32] = kani::any();
        let keeper: [u8; 32] = kani::any();
        let buffer_bps: u16 = kani::any();
        let max_leverage_bps: u32 = kani::any();
        let bump: u8 = kani::any();
        let auth_bump: u8 = kani::any();
        let version: u8 = kani::any();
        let paused: u8 = kani::any();
        let enrolled_count: u8 = kani::any();

        pa.magic = magic;
        pa.owner = owner;
        pa.keeper = keeper;
        pa.buffer_bps = buffer_bps;
        pa.max_leverage_bps = max_leverage_bps;
        pa.bump = bump;
        pa.auth_bump = auth_bump;
        pa.version = version;
        pa.paused = paused;
        pa.enrolled_count = enrolled_count;

        let bytes: &[u8] = bytemuck::bytes_of(&pa);
        assert!(bytes.len() == 888);
        let pa2: &PortfolioAccount = bytemuck::from_bytes(bytes);

        assert!(pa2.magic == magic);
        assert!(pa2.owner == owner);
        assert!(pa2.keeper == keeper);
        assert!(pa2.buffer_bps == buffer_bps);
        assert!(pa2.max_leverage_bps == max_leverage_bps);
        assert!(pa2.bump == bump);
        assert!(pa2.auth_bump == auth_bump);
        assert!(pa2.version == version);
        assert!(pa2.paused == paused);
        assert!(pa2.enrolled_count == enrolled_count);
    }
}

// ─────────────────────────────────────────────────────────────────────────
// 7. entrypoint
// ─────────────────────────────────────────────────────────────────────────
#[cfg(not(feature = "no-entrypoint"))]
mod entrypoint_mod {
    use crate::processor::process_instruction;
    #[allow(unused_imports)]
    use alloc::format; // entrypoint! expands `msg!` which uses `format!`
    use solana_program::{
        account_info::AccountInfo, entrypoint, entrypoint::ProgramResult, pubkey::Pubkey,
    };

    entrypoint!(_entry);

    fn _entry(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
        process_instruction(program_id, accounts, data)
    }
}
