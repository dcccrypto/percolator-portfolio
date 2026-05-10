//! # Wrapper-side margin math — DESIGN BOUNDARY DOCUMENT, not a stub.
//!
//! After re-reading the upstream maintainer's #87 close (2026-05-10), the
//! work this module *appeared* to need is not actually wanted by the
//! design. Leaving an empty stub would invite well-meaning future
//! maintainers to fill it in unnecessarily, so this module exists to
//! document why it stays empty.
//!
//! ## Two cross-margin models
//!
//! **Soft cross-margin (what we ship):**
//!   - `portfolio_auth` PDA owns one engine account per enrolled market.
//!   - Each market's engine enforces its own IM/MM on every Deposit /
//!     Trade / Withdraw via `is_above_initial_margin` /
//!     `is_above_maintenance_margin`, using the oracle account passed by
//!     the caller (always fresh in our `Trade` ix's CPI to TradeCpi).
//!   - One USDC vault under `portfolio_auth` lets the keeper rebalance
//!     collateral between markets atomically — moving surplus from a
//!     well-capitalised account into one approaching its per-market MM
//!     before it gets liquidated.
//!   - Net behaviour: profits in market A can backstop losses in market B
//!     because the keeper moves the capital across, while the engine's
//!     per-market admission check still catches every bad trade.
//!
//! **Hard cross-margin (what we do NOT ship):**
//!   - The wrapper would compute *aggregate* portfolio equity vs
//!     *aggregate* IM_req across all enrolled markets, allow individual
//!     accounts to be below their per-market MM as long as the portfolio
//!     total covers it, and override the engine's per-account check.
//!   - This requires either an engine API to disable per-account checks
//!     under wrapper authority, OR a wrapper-side mirror of the engine's
//!     full equity / notional / IM math reading every enrolled slab.
//!
//! ## Why hard cross-margin doesn't ship
//!
//! 1. **The upstream maintainer reviewed and closed exactly that proposal.**
//!    `aeyakovenko/percolator#58`, `aeyakovenko/percolator-prog#87`,
//!    `aeyakovenko/percolator-prog#88` (closed 2026-05-10) collectively
//!    rejected: an engine `account_health_snapshot` view, an engine
//!    `transfer_owner`, and a wrapper `GetAccountHealth` ix. The stated
//!    reason was that any such API expands engine authority surface
//!    around withdrawals, close, fee credit, self-crank auth, and trade
//!    authorization — for a feature the existing per-account check
//!    already covers when the wrapper PDA is the account owner.
//!
//! 2. **The "fresh oracle" point is already satisfied.** The engine's
//!    per-account margin check inside `TradeCpi` runs against whatever
//!    oracle account the caller passes. Our `Trade` ix forwards the user-
//!    supplied oracle straight through — that's a fresh oracle this slot.
//!    The cached-price concern in #87 was about adding a *separate* view
//!    instruction that would have used `engine.last_oracle_price` (cached)
//!    as its oracle. We don't add that view.
//!
//! 3. **Mirroring the engine math wrapper-side is fragile and unnecessary.**
//!    Even with our `percolator` engine crate dep giving us the Account
//!    struct for free, we'd still need to reproduce internal scaling
//!    (unit_scale, ADL multipliers, B-tracking once Wave 5 lands) for
//!    a number we'd then compare against an aggregate the engine has no
//!    concept of. Re-pinning + re-validating that math on every engine
//!    schema change is real cost; the soft-cross-margin model gets the
//!    same user-visible behaviour through the keeper without it.
//!
//! ## What this module would contain if we built hard cross-margin later
//!
//! For the record (and so a future audit doesn't have to re-derive it):
//!
//!   per-account equity:  `engine.account_equity_maint_raw(account)`
//!                          (public on the FORK; capital + pnl − fee_debt;
//!                           does NOT use oracle — pure realized equity)
//!   per-account IM_req:   notional × initial_margin_bps / 10_000, where
//!                          notional = |effective_pos_q| × oracle / scale
//!                          (`engine.notional_checked` is private; would
//!                           need either visibility lift or wrapper-side
//!                           reproduction)
//!   per-account MM_req:   notional × maintenance_margin_bps / 10_000
//!   aggregate gate:       sum_equity ≥ sum_IM_req  (with new-trade
//!                          increment from `account_equity_trade_open_raw`)
//!
//! All of this would live behind a `hard_cross_margin` cargo feature so
//! the soft default stays the canonical ship.

#![allow(dead_code)]
