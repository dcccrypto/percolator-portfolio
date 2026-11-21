# percolator-portfolio

A Solana program that adds USDC cross-margin on top of isolated [percolator-prog](https://github.com/aeyakovenko/percolator-prog) markets. It is a sibling program: the engine and wrapper crates are used unchanged.

This is unaudited, pre-deployment code. The program ID in `declare_id!` (`src/portfolio.rs`) is a placeholder and no deployment is documented here.

## How it works

Each user gets three PDAs:

| PDA | Seeds | Purpose |
|---|---|---|
| `portfolio_data` | `["portfolio", user]` | The `PortfolioAccount` state |
| `portfolio_auth` | `["portfolio_auth", user]` | Signs every CPI. Owner of the user's engine accounts and of the vault |
| `portfolio_vault` | `["portfolio_vault", user]` | USDC token account used to move funds between the user's wallet and the per-market deposits |

The engine still enforces margin per account. The portfolio program adds two checks on top:

- A pre-trade check that rejects trades which would push aggregate portfolio equity below aggregate initial margin.
- A permissionless `RebalanceCrank` that anyone can call to top up an at-risk market account from the portfolio vault, for a small bounty.

It is USDC only (no multi-asset collateral), and it is not hard cross-margin: an individual account cannot go below its per-market maintenance margin.

## Instructions

Tags 0 to 15, defined in `src/portfolio.rs`: `InitPortfolio`, `EnrollMarket`, `UnenrollMarket`, `Deposit`, `Withdraw`, `Trade`, `Rebalance`, `EmergencyClose`, `UpdateConfig`, `SetPaused`, `InitVault`, `ClosePortfolio`, `EnrollMarketAndInit`, `RebalanceCrank`, `RequestUpdateConfig`, `CommitUpdateConfig`. Each has a doc comment with its accounts and body layout.

Typical setup: `InitPortfolio`, `InitVault`, `EnrollMarketAndInit` per market, then `Deposit` and `Trade`.

## Layout

```
src/portfolio.rs   state, instructions, processor, Kani proofs
src/cpi.rs         CPI builders for percolator-prog and spl-token
src/margin.rs      aggregate initial-margin math
src/pyth.rs        oracle decoding (delegates to percolator-prog)
tests/             LiteSVM integration tests
```

## Build and test

`Cargo.toml` uses path dependencies on sibling checkouts, so clone the engine and wrapper next to this repo at the commits this repo pins (the `percolator-prog` build script fails with a pin mismatch otherwise):

```
parent/
  percolator/
  percolator-prog/
  percolator-portfolio/   (this repo)
```

```bash
cargo check
cargo test --locked
cargo build-sbf -- --locked     # needs the Solana CLI with cargo-build-sbf
cargo kani --features kani      # Kani proofs, needs kani-verifier
```

The SBF toolchain pins in `Cargo.toml` (blake3, indexmap, hashbrown, once_cell) exist because newer releases need edition 2024. The end-to-end tests load a built `percolator-prog` binary; see `tests/common/integration_env.rs`.

## Before deploying

Replace the placeholder program ID, check `PERCOLATOR_PROGRAM_ID` in `src/cpi.rs` against the wrapper deployment you target (it is a hardcoded constant and may not match the current devnet programs), and get an external audit. Use a multisig as upgrade authority.

## License

Apache-2.0, see [LICENSE](./LICENSE).
