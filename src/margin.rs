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
        // For target: caller passes Some(signed trade delta q); we add
        // to the current account.position_basis_q to get the post-trade
        // basis. For others: None → use current basis unchanged.
        // Saturating add: overflow projects to i128::MIN/MAX, which
        // produces a worst-case notional and thus conservative reject.
        let basis_q = match view.trade_delta_q {
            Some(delta) => account.position_basis_q.saturating_add(delta),
            None => account.position_basis_q,
        };

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
        // eff == 0 short-circuits to im_req = 0; else proportional
        // floor against bps, then floor at min_nonzero_im_req.
        let im_req = if basis_q == 0 {
            0u128
        } else {
            let prop = mul_div_floor_u128(
                notional,
                engine.params.initial_margin_bps as u128,
                10_000u128,
            );
            core::cmp::max(prop, engine.params.min_nonzero_im_req)
        };

        // ── Saturating aggregate accumulation ─────────────────────────
        total_eq = total_eq.saturating_add(eq);
        total_im_req_u128 = total_im_req_u128.saturating_add(im_req);
    }

    // ── Cast aggregate IM_req to i128 conservatively ──────────────────
    // Mirrors engine's saturation: u128 → i128 via `if u > MAX { MAX }`.
    // An over-i128::MAX aggregate IM_req is treated as unsatisfiable and
    // rejects the trade (since total_eq is bounded by i128).
    let total_im_req_i128: i128 = if total_im_req_u128 > i128::MAX as u128 {
        i128::MAX
    } else {
        total_im_req_u128 as i128
    };

    if total_eq < total_im_req_i128 {
        return Err(PortfolioError::AggregateImBreach.into());
    }

    Ok(())
}
