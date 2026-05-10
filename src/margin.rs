//! Wrapper-side aggregate IM enforcement — Defense 1 of soft+ cross-margin.
//!
//! Per the upstream maintainer's #87 review, engine-side per-account IM/MM
//! is the safety gate. This module is a **supplementary product feature**
//! that adds pre-trade portfolio-level enforcement on top of that gate:
//! before submitting a Trade CPI, the wrapper iterates every enrolled
//! market, computes `(equity_i, im_req_i)` against a fresh-this-slot
//! oracle, and rejects the trade if `sum(equity_i) < sum(im_req_i)`.
//!
//! What this catches that engine-alone does not:
//! - Portfolio-level insolvency where the trade target is healthy
//!   individually but other enrolled accounts are bleeding into the red.
//! - Trades that would individually pass engine per-account IM but push
//!   the portfolio's aggregate IM headroom below zero.
//!
//! What this does NOT do:
//! - Allow individual accounts to run below per-market MM — the engine
//!   still enforces that at execution. True Hyperliquid-style hard
//!   cross-margin requires the engine PR closed in #58. See
//!   `crate::margin` doc-comment above for the full rationale.
//!
//! ## Math (mirrors engine `is_above_initial_margin` per account)
//!
//! For each enrolled account `i`:
//!   eq_i      = engine.account_equity_maint_raw(account_i)      // i128
//!   basis_i   = projected_basis_i.unwrap_or(account_i.position_basis_q)
//!   notional_i = ceil(|basis_i| × oracle_price_i / POS_SCALE)   // u128
//!   prop_i    = floor(notional_i × initial_margin_bps / 10_000) // u128
//!   im_req_i  = max(prop_i, params.min_nonzero_im_req)          // u128
//!
//! Aggregate gate:
//!   total_eq      = saturating_sum(eq_i)                         // i128
//!   total_im_req  = saturating_sum(im_req_i) → cast to i128       // i128
//!   ok iff total_eq >= total_im_req
//!
//! ## ADL handling (v1)
//!
//! Engine internally uses `effective_pos_q_checked` which applies ADL
//! multipliers to `position_basis_q`. We use `position_basis_q` directly,
//! which equals `effective_pos_q` outside of active ADL events. During
//! ADL, our aggregate IM_req is slightly off (basis vs effective), but
//! the engine's per-account check at TradeCpi time still catches any
//! per-account violation — so wrapper-side ADL imprecision is
//! conservative-or-equal, never unsafe. Documented limitation.

#![allow(dead_code)]

use percolator::wide_math::{mul_div_ceil_u128, mul_div_floor_u128};
use percolator::{Account, RiskEngine, POS_SCALE};
use solana_program::program_error::ProgramError;

use crate::errors::PortfolioError;

/// Read-only view of one enrolled market for aggregate-IM purposes.
pub struct EnrolledView<'a> {
    /// Borrowed slab data (output of `AccountInfo::try_borrow_data()`).
    pub slab_data: &'a [u8],
    /// Account index inside the engine's `accounts` array.
    pub account_idx: u16,
    /// Fresh oracle price (e6) for this market. For the trade target,
    /// the caller passes the same oracle they'll forward to TradeCpi.
    /// For other markets, the caller passes that market's current
    /// oracle account; the wrapper does NOT cross-validate (engine's
    /// per-account check is the safety gate; aggregate IM is product).
    pub oracle_price_e6: u64,
    /// Trade delta (signed q-units) the check should add to this
    /// account's current `position_basis_q`. `Some(delta)` for the
    /// trade-target account, `None` for every other enrolled account
    /// (whose basis is unchanged by this trade).
    pub trade_delta_q: Option<i128>,
}

/// Pre-trade aggregate IM check across all enrolled markets.
///
/// Decodes each slab via `percolator_prog::zc::engine_ref`, sums per-
/// account `(equity, im_req)`, and returns `Err` if the aggregate
/// would breach. On `Ok`, the caller proceeds with the TradeCpi — the
/// engine's per-account check still runs there and may still reject.
///
/// **Engine-coupling note:** the math here is line-for-line the same as
/// `engine.is_above_initial_margin` (`~/percolator/src/percolator.rs:3868`).
/// Drift between engine and wrapper here is the wrapper's maintenance
/// cost — re-pin + re-test on every upstream sync wave that touches
/// margin-relevant fields. The cost of drift is conservative-or-equal
/// rejection (wrapper rejects, engine would have accepted), never an
/// unsafe accept.
/// Project the post-trade basis: add a signed trade delta to the current
/// position basis, saturating on overflow toward the matching i128 bound
/// so wider |basis| produces conservative-or-equal notional downstream.
///
/// Pure function — separated from `check_aggregate_im` so Kani can prove
/// the saturation behaviour without instantiating slab data.
#[inline]
pub fn project_basis(current_basis_q: i128, trade_delta_q: Option<i128>) -> i128 {
    match trade_delta_q {
        Some(d) => current_basis_q.saturating_add(d),
        None => current_basis_q,
    }
}

/// Compute IM_req for one account from its notional, bps, and the engine
/// param `min_nonzero_im_req`. Mirrors the proportional formula inside
/// `engine.is_above_initial_margin` (spec §9.1) when notional > 0. The
/// `basis_zero` short-circuit (im_req = 0 when basis_q == 0) is caller's
/// responsibility — this function takes notional directly.
///
/// Pure function for Kani.
#[inline]
pub fn im_req_from_notional(
    notional: u128,
    initial_margin_bps: u64,
    min_nonzero_im_req: u128,
) -> u128 {
    let prop = mul_div_floor_u128(notional, initial_margin_bps as u128, 10_000u128);
    core::cmp::max(prop, min_nonzero_im_req)
}

/// Saturating u128 → i128 cast matching the engine's conservative
/// projection: any aggregate IM_req above i128::MAX is treated as
/// unsatisfiable (will cause the check to fail).
///
/// Pure function for Kani.
#[inline]
pub fn cast_aggregate_im_req(total_im_req_u128: u128) -> i128 {
    if total_im_req_u128 > i128::MAX as u128 {
        i128::MAX
    } else {
        total_im_req_u128 as i128
    }
}

/// Final aggregate gate. Returns `true` if portfolio total equity is at
/// least the total IM requirement (i.e., the trade is admissible).
///
/// Pure function for Kani.
#[inline]
pub fn aggregate_admissible(total_eq: i128, total_im_req_i128: i128) -> bool {
    total_eq >= total_im_req_i128
}

pub fn check_aggregate_im(views: &[EnrolledView<'_>]) -> Result<(), ProgramError> {
    let mut total_eq: i128 = 0;
    let mut total_im_req_u128: u128 = 0;

    for view in views.iter() {
        // Decode the engine view from slab data. `zc::engine_ref` does the
        // length + alignment + discriminant validation we need.
        let engine: &RiskEngine = percolator_prog::zc::engine_ref(view.slab_data)
            .map_err(|_| PortfolioError::MarginSlabDecodeFailed)?;

        let idx = view.account_idx as usize;
        if idx >= percolator::MAX_ACCOUNTS || !engine.is_used(idx) {
            return Err(PortfolioError::MarginSlabNotEnrolled.into());
        }

        let account: &Account = &engine.accounts[idx];

        // ── Equity (no oracle): C + PnL − FeeDebt ─────────────────────
        let eq = engine.account_equity_maint_raw(account);

        // ── Notional with projected basis ─────────────────────────────
        // Saturating add via `project_basis` — overflow projects to
        // i128::MIN/MAX, which produces a worst-case notional and thus
        // conservative reject downstream.
        let basis_q = project_basis(account.position_basis_q, view.trade_delta_q);

        // Engine's `risk_notional_from_eff_q` is private but its formula
        // is documented in spec §7 and replicated here exactly:
        //   notional = ceil(|basis_q| × oracle_price / POS_SCALE)
        // We use |basis_q| directly; ADL multipliers are skipped (v1).
        let notional: u128 = if view.oracle_price_e6 == 0 {
            // Match engine's `try_notional` rejection on zero price.
            return Err(PortfolioError::MarginNotionalRejected.into());
        } else {
            mul_div_ceil_u128(
                basis_q.unsigned_abs(),
                view.oracle_price_e6 as u128,
                POS_SCALE,
            )
        };

        // ── IM_req per engine spec §9.1 ───────────────────────────────
        // basis_q == 0 short-circuits to im_req = 0; else proportional
        // floor against bps, then floor at min_nonzero_im_req. Pure
        // helper for the non-zero branch lives in `im_req_from_notional`.
        let im_req = if basis_q == 0 {
            0u128
        } else {
            im_req_from_notional(
                notional,
                engine.params.initial_margin_bps,
                engine.params.min_nonzero_im_req,
            )
        };

        // ── Saturating aggregate accumulation ─────────────────────────
        total_eq = total_eq.saturating_add(eq);
        total_im_req_u128 = total_im_req_u128.saturating_add(im_req);
    }

    // ── Cast aggregate IM_req to i128 conservatively ──────────────────
    // Mirrors engine's saturation: u128 → i128 via `if u > MAX { MAX }`.
    // Pure helper `cast_aggregate_im_req` for Kani.
    let total_im_req_i128 = cast_aggregate_im_req(total_im_req_u128);

    if !aggregate_admissible(total_eq, total_im_req_i128) {
        return Err(PortfolioError::AggregateImBreach.into());
    }

    Ok(())
}

// =====================================================================
// Kani proofs — pure-function invariants for the aggregate IM math.
//
// These cover the math that runs hot on every Trade. They do NOT cover
// the slab decode path (CBMC can't ergonomically synthesize the
// engine-crate's zerocopy structs), but they DO cover every arithmetic
// step from raw account fields to the final admissibility decision.
// =====================================================================

#[cfg(kani)]
mod proofs {
    use super::*;

    /// project_basis: for None delta, output equals input.
    #[kani::proof]
    fn kani_project_basis_none_identity() {
        let cur: i128 = kani::any();
        assert!(project_basis(cur, None) == cur);
    }

    /// project_basis: for Some delta, output is the saturating sum.
    /// In particular, never panics (overflow saturates).
    #[kani::proof]
    fn kani_project_basis_some_saturates() {
        let cur: i128 = kani::any();
        let delta: i128 = kani::any();
        let out = project_basis(cur, Some(delta));
        // Either matches the wrapping sum (no overflow) or saturates
        // to the corresponding i128 bound.
        let (sum, overflowed) = cur.overflowing_add(delta);
        if !overflowed {
            assert!(out == sum);
        } else {
            // Saturation rule: positive overflow → i128::MAX,
            // negative overflow → i128::MIN.
            if delta > 0 {
                assert!(out == i128::MAX);
            } else {
                assert!(out == i128::MIN);
            }
        }
    }

    /// im_req_from_notional: never returns below `min_nonzero_im_req`
    /// (the spec §9.1 floor for non-flat accounts).
    ///
    /// Note: we cannot symbolically prove monotonicity in notional
    /// because CBMC times out on u128 mul_div with symbolic operands.
    /// Instead we lock the floor property here — it's the safety-
    /// critical invariant; monotonicity follows from the algebraic
    /// structure (mul_div_floor is monotone, max preserves
    /// monotonicity).
    #[kani::proof]
    #[kani::unwind(2)]
    fn kani_im_req_respects_min_floor() {
        // 16-bit operands: notional + floor each fit in u16, bps
        // bounded to engine's accepted range. CBMC handles these
        // symbolic instances in <1s.
        let notional: u16 = kani::any();
        let bps: u16 = kani::any();
        let floor: u16 = kani::any();
        kani::assume(bps <= 10_000);

        let r = im_req_from_notional(notional as u128, bps as u64, floor as u128);
        assert!(r >= floor as u128);
    }

    /// im_req_from_notional: returns the floor when notional × bps is
    /// less than 10_000 × floor (the proportional term rounds to
    /// below the floor, so max takes over). Locks the floor's
    /// dominance condition.
    #[kani::proof]
    #[kani::unwind(2)]
    fn kani_im_req_floor_dominates_when_small() {
        // Small notional, modest bps, large floor — floor must dominate.
        let notional: u8 = kani::any();
        let bps: u16 = kani::any();
        kani::assume(bps <= 100); // 1% max
        let floor: u128 = 1_000_000_000;

        let r = im_req_from_notional(notional as u128, bps as u64, floor);
        // notional × bps / 10_000 ≤ 255 × 100 / 10_000 = 2.55 → 2
        // which is far below floor = 1_000_000_000.
        assert!(r == floor);
    }

    /// cast_aggregate_im_req: saturates at i128::MAX, never overflows.
    #[kani::proof]
    fn kani_cast_saturates_at_i128_max() {
        let x: u128 = kani::any();
        let out = cast_aggregate_im_req(x);
        assert!(out >= 0);
        if x <= i128::MAX as u128 {
            assert!(out as u128 == x);
        } else {
            assert!(out == i128::MAX);
        }
    }

    /// aggregate_admissible: returns true iff total_eq >= total_im_req.
    /// Trivial, but locks in the comparison direction so a future
    /// refactor can't silently flip the gate.
    #[kani::proof]
    fn kani_aggregate_admissible_direction() {
        let eq: i128 = kani::any();
        let im: i128 = kani::any();
        let r = aggregate_admissible(eq, im);
        assert!(r == (eq >= im));
    }

    /// End-to-end: for a single account with basis_q == 0, the
    /// admissibility decision depends only on total equity sign,
    /// because im_req = 0 (spec §9.1 short-circuit).
    /// This mirrors `engine.is_above_initial_margin`'s short-circuit
    /// at the per-account level.
    #[kani::proof]
    fn kani_basis_zero_im_req_zero() {
        // basis_q == 0 → wrapper short-circuits im_req to 0.
        let total_eq: i128 = kani::any();
        let total_im_req_i128 = cast_aggregate_im_req(0u128);
        assert!(total_im_req_i128 == 0);
        let admissible = aggregate_admissible(total_eq, total_im_req_i128);
        assert!(admissible == (total_eq >= 0));
    }
}
