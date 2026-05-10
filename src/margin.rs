//! Wrapper-side margin math — mirrors `percolator` engine equity / notional /
//! MM / IM computations against a slab account read as raw bytes.
//!
//! Why this exists: the upstream maintainer's review of the proposed
//! `engine::account_health_snapshot` API (`aeyakovenko/percolator#58`,
//! `aeyakovenko/percolator-prog#87`) closed it as out of scope and was
//! explicit that wrappers should mirror the engine's math rather than
//! depend on an engine read-view ABI. This module is that mirror.
//!
//! Two strict constraints from that review govern this code:
//!
//! 1. **Fresh oracle, not slab cache.** Trade-admission checks must use a
//!    Pyth price decoded *this slot*, not `engine.last_oracle_price`. The
//!    cached slab price is correct for accrue paths but not for admission
//!    under the crank/target-lag design — it can lag the live mark by an
//!    entire accrual segment.
//!
//! 2. **Mirror, don't depend.** This crate intentionally does not import
//!    the `percolator` engine crate. We decode the slab via raw byte
//!    offsets and re-implement the math. Re-pinning + re-testing on every
//!    engine schema change is the wrapper's maintenance cost — the
//!    upstream API surface stays minimal.
//!
//! ## Scope of mirrored math (v1)
//!
//! Eq_maint_raw_i (engine `account_equity_maint_raw`):
//!
//!   Eq_maint = capital + pnl − fee_debt(fee_credits)
//!
//! Where `capital: u128`, `pnl: i128`, `fee_credits: u64`. Engine widens to
//! I256 for overflow safety; we use i128 saturating with a conservative
//! `i128::MIN + 1` failure-mode mirroring the engine's overflow projection.
//!
//! Notional (engine `notional_checked`):
//!
//!   notional = |position_basis_q| × oracle_price / scale
//!
//! ADL multiplier handling (engine `effective_pos_q_checked`) is **not
//! mirrored in v1**. Effective position equals basis position when the
//! market is not in ADL — the common case. When ADL is active, the engine's
//! own per-account check still gates final admission; the wrapper-side
//! aggregate check is best-effort during ADL events.
//!
//! MM_req and IM_req (engine `is_above_maintenance_margin` /
//! `is_above_initial_margin`):
//!
//!   prop  = notional × bps / 10_000   (mul_div_floor_u128)
//!   MM_req = max(prop_mm, params.min_nonzero_mm_req)
//!   IM_req = max(prop_im, params.min_nonzero_im_req)
//!
//! Spec §9.1 short-circuit: if effective position is zero, both reqs are
//! zero. We mirror this.
//!
//! ## What's NOT in this module yet
//!
//! - Account struct decoder (read raw slab bytes → typed fields)
//! - RiskEngine struct decoder (for params, market_mode, ADL state)
//! - Pyth price decoder (lives in `crate::pyth`)
//! - Aggregate-portfolio computation (sums across enrolled accounts)
//! - Wiring into `processor::trade()`
//!
//! Each of those is a separate change with its own tests + verification.
//! This file is the documented contract they'll plug into.

#![allow(dead_code)]

/// Raw maintenance equity (i128) for one account.
///
/// Mirrors `engine.account_equity_maint_raw` (~tolypercolator/src/percolator.rs:5559).
///
/// Returns `i128::MIN + 1` on conservative overflow projection — matches
/// engine's spec §3.4 overflow behaviour so this value flunks every `> 0`
/// and `> MM_req` gate.
///
/// **Note:** stub. Real implementation needs the Account struct decoder.
pub fn account_equity_maint_raw(_capital: u128, _pnl: i128, _fee_credits: u64) -> i128 {
    // Stub: real impl widens to I256 and saturates to i128::MIN + 1 on
    // overflow per spec §3.4. Tracked: aggregate-margin-check task.
    0
}

/// Notional value (u128) of a position at a given oracle price.
///
/// Mirrors `engine.notional_checked` (~tolypercolator/src/percolator.rs:5705).
///
/// **Note:** stub. Real implementation needs scale-unit handling that
/// matches `RiskParams.unit_scale`.
pub fn notional(_position_basis_q: i128, _oracle_price_e6: u64) -> u128 {
    // Stub: real impl is mul_div_floor_u128(|basis|, price, scale).
    0
}

/// Maintenance margin requirement (u128).
///
/// Mirrors the inline computation inside `engine.is_above_maintenance_margin`
/// (~tolypercolator/src/percolator.rs:5748).
///
/// **Note:** stub.
pub fn mm_req(_notional: u128, _maintenance_margin_bps: u64, _min_nonzero_mm_req: u128) -> u128 {
    0
}

/// Initial margin requirement (u128).
///
/// Mirrors the inline computation inside `engine.is_above_initial_margin`.
///
/// **Note:** stub.
pub fn im_req(_notional: u128, _initial_margin_bps: u64, _min_nonzero_im_req: u128) -> u128 {
    0
}
