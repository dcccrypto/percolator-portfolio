//! Pyth Price account decoder — fresh oracle reads for trade admission.
//!
//! Why this exists: the upstream maintainer's review of `GetAccountHealth`
//! (`aeyakovenko/percolator-prog#87`) was explicit that a cached-price /
//! no-oracle view is not authoritative for trade admission under the crank
//! / target-lag design. The wrapper's pre-trade aggregate margin check
//! must decode a fresh Pyth Price account this slot — not read
//! `engine.last_oracle_price` from the slab.
//!
//! ## What this decodes
//!
//! Pyth's on-chain `PriceUpdateV2` account format (the one written by the
//! Pyth Solana Receiver). Fields used:
//!
//! - `price` (i64) — last published price, scaled by `expo`
//! - `conf` (u64) — confidence interval
//! - `expo` (i32) — base-10 exponent on `price` and `conf`
//! - `publish_time` (i64) — Unix seconds the publisher signed at
//!
//! The wrapper validates:
//! - `publish_time` is recent (within `MAX_PRICE_AGE_SECS` of `Clock.unix_timestamp`)
//! - `conf / abs(price)` is below a basis-point ceiling (config-driven)
//! - `expo` matches the market's expected `unit_scale`
//!
//! ## Scaling
//!
//! Engine math operates on `oracle_price_e6` (price × 10^6). Pyth's price
//! is `mantissa × 10^expo`. Conversion:
//!
//!   price_e6 = mantissa × 10^(expo + 6)   if expo + 6 >= 0
//!   price_e6 = mantissa / 10^(-(expo+6))  if expo + 6 < 0
//!
//! Saturation on overflow returns `0`, which fails downstream notional /
//! IM checks conservatively.
//!
//! ## What's NOT in this module yet
//!
//! - Actual byte-offset decoder (PriceUpdateV2 layout)
//! - Magic / discriminator validation
//! - Pyth program ID hardcode (verify the account is owned by the Pyth
//!   receiver, not an attacker substitute)
//! - Slot-precision freshness gate (some markets need slot-not-second)
//!
//! Each tracked under the aggregate-margin-check task.

#![allow(dead_code)]

/// Decoded fresh oracle price suitable for use in `crate::margin::notional`.
///
/// `price_e6` is always non-negative (Pyth signed prices reject negative
/// quotes upstream — but the engine's notional math is over u128 so we
/// zero-clamp here defensively).
pub struct FreshPrice {
    pub price_e6: u64,
    pub conf_e6: u64,
    pub publish_unix_secs: i64,
}

/// Maximum acceptable Pyth publish-time lag, in seconds. Conservative
/// default; will be config-driven once admission policy is wired.
pub const MAX_PRICE_AGE_SECS: i64 = 30;

/// Maximum acceptable Pyth confidence interval as a fraction of price, in
/// basis points. `conf / price >= 100 bps` (1%) is treated as too noisy
/// for admission and rejects the trade.
pub const MAX_CONF_BPS: u64 = 100;

/// Decode a Pyth `PriceUpdateV2` account into a `FreshPrice`.
///
/// **Note:** stub. Real implementation will:
///   1. Verify `account.owner == PYTH_RECEIVER_PROGRAM_ID` (hardcoded)
///   2. Verify the magic / discriminator at the head of `data`
///   3. Decode `price`, `conf`, `expo`, `publish_time` from fixed offsets
///   4. Convert mantissa+expo to `price_e6` with saturating arithmetic
///   5. Reject if publish_time is older than `MAX_PRICE_AGE_SECS`
///   6. Reject if `conf_e6 / price_e6 >= MAX_CONF_BPS / 10_000`
pub fn decode_pyth_price(_data: &[u8], _now_unix_secs: i64) -> Result<FreshPrice, PythError> {
    Err(PythError::NotYetImplemented)
}

/// Reasons a Pyth read can fail. Each maps to a distinct wrapper-level
/// `PortfolioError` variant when wired into Trade admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythError {
    /// Account owner is not the Pyth Solana Receiver program.
    WrongOwner,
    /// Account data length is too short for a `PriceUpdateV2`.
    DataTooShort,
    /// Magic / discriminator at the head of the account does not match.
    BadMagic,
    /// `publish_time` is older than `MAX_PRICE_AGE_SECS`.
    Stale,
    /// `conf / price` exceeds `MAX_CONF_BPS`.
    LowConfidence,
    /// Mantissa × 10^(expo+6) overflows u64.
    Overflow,
    /// Decoder is a stub.
    NotYetImplemented,
}
