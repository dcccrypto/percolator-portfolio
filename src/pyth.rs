//! # Pyth Price account decoder — DESIGN BOUNDARY DOCUMENT, not a stub.
//!
//! The earlier scaffold here implied a wrapper-side fresh-oracle decode
//! was needed for trade admission. After re-reading the upstream
//! maintainer's #87 close (2026-05-10), it is not — the engine's
//! `TradeCpi` already takes an oracle account and runs the
//! `is_above_initial_margin` check against that fresh price during the
//! CPI we issue. Our wrapper does not need a parallel decode.
//!
//! ## Where the oracle actually flows
//!
//! 1. User submits `portfolio_program::Trade` with a Pyth Price account
//!    in the variadic matcher tail (or in TradeCpi's fixed slot 4 —
//!    the engine reads from there).
//! 2. Wrapper signs `invoke_signed` to `percolator-prog::TradeCpi`.
//! 3. TradeCpi handler reads the oracle account, validates freshness +
//!    confidence per the engine's policy, and calls
//!    `engine.is_above_initial_margin(account, idx, oracle_price, ...)`.
//! 4. If that returns false, the CPI errors and the txn reverts.
//!
//! At no point does the wrapper need its own decoded price.
//!
//! ## What changed from the earlier scaffold
//!
//! The earlier `FreshPrice` struct, `PythError` enum, `MAX_PRICE_AGE_SECS`,
//! `MAX_CONF_BPS`, and `decode_pyth_price` stubs are removed. None of
//! them have callers and none would have callers under the
//! soft-cross-margin model (see `crate::margin`). If a future change
//! adds wrapper-side admission (e.g., per-portfolio leverage cap that
//! the engine doesn't enforce), this module would be revived with a
//! decoder against `pyth_solana_receiver_sdk::price_update::PriceUpdateV2`.
//!
//! ## If we did need to decode here later
//!
//! `pyth_solana_receiver_sdk` exposes `PriceUpdateV2` directly with a
//! borsh deserialiser; the wrapper would gain that crate as a dep, decode
//! `account.data` into `PriceUpdateV2 { price_message, ... }`, gate on
//! `publish_time` and `conf / abs(price)` ratios, and convert the signed
//! mantissa+exponent to whatever scale the wrapper math expects. Same
//! pattern the percolator-prog matcher uses today — no novel design.

#![allow(dead_code)]
