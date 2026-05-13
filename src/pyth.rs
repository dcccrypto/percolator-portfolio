//! Pyth Price account decoder used by Defense 1's aggregate IM check.
//!
//! Delegates to `percolator_prog::oracle::read_pyth_price_e6`, which is the
//! same helper percolator-prog uses internally for its own oracle reads —
//! so the staleness / confidence / feed_id policy applied wrapper-side is
//! byte-for-byte identical to what TradeCpi enforces. We don't reinvent
//! oracle validation, we reuse the maintained one.
//!
//! ## Why this lives in a wrapper-side module
//!
//! Defense 1 (`crate::margin::check_aggregate_im`) needs the e6 price of
//! every enrolled market's oracle to compute per-account notional. The
//! engine's TradeCpi reads the target market's oracle as part of its
//! own per-account check, but the wrapper is the only layer that sees
//! the full portfolio — so the wrapper has to decode every other
//! market's oracle itself.
//!
//! Each market's `MarketConfig.index_feed_id`, `max_staleness_secs`,
//! and `conf_filter_bps` come from `percolator_prog::state::read_config`
//! applied to the slab's raw data. Different markets can have different
//! freshness / confidence policies; we honour each market's own settings.

#![allow(dead_code)]

use solana_program::{account_info::AccountInfo, program_error::ProgramError};

/// Decode a Pyth PriceUpdateV2 account for one enrolled market and
/// return its current price in e6 fixed-point units.
///
/// Policy (feed_id, staleness, confidence) comes from the slab's own
/// MarketConfig — different markets can have different oracle
/// settings, and we honour each one. This is the same policy
/// percolator-prog applies inside TradeCpi for its own check.
///
/// Errors are surfaced as `ProgramError::Custom` from percolator-prog's
/// own helpers (OracleStale, OracleConfTooWide, InvalidOracleKey,
/// OracleInvalid, IllegalOwner) — they propagate up to the wrapper
/// trade handler unchanged so client-side logs stay informative.
pub fn read_oracle_price_e6(
    oracle_ai: &AccountInfo,
    slab_data: &[u8],
    now_unix_ts: i64,
) -> Result<u64, ProgramError> {
    // M-2: `percolator_prog::state::read_config` silently panics if
    // `slab_data.len() < HEADER_LEN + CONFIG_LEN` (index-out-of-bounds on
    // the byte-copy). Reject under-length slabs explicitly with a clear
    // wrapper error instead of an SBF fault.
    // Constants: percolator_prog::constants::{HEADER_LEN, CONFIG_LEN}
    // (~/percolator-prog/src/percolator.rs:69-70).
    let min_len =
        percolator_prog::constants::HEADER_LEN + percolator_prog::constants::CONFIG_LEN;
    if slab_data.len() < min_len {
        return Err(crate::errors::PortfolioError::MarginSlabDecodeFailed.into());
    }
    let config = percolator_prog::state::read_config(slab_data);
    let (price_e6, _publish_time) = percolator_prog::oracle::read_pyth_price_e6(
        oracle_ai,
        &config.index_feed_id,
        now_unix_ts,
        config.max_staleness_secs,
        config.conf_filter_bps,
    )?;
    Ok(price_e6)
}
