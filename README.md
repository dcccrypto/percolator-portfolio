# percolator-portfolio

USDC cross-margin wrapper over isolated `percolator-prog` markets. A
sibling Solana program — engine and wrapper crates of `percolator-prog`
are unchanged. The wrapper owns each user's per-market `Account` via a
PDA, holds a per-user USDC vault, and orchestrates collateral routing
between enrolled markets.

```
              ┌──────────────────────────────────────┐
              │     percolator-portfolio (this)      │
              │                                      │
              │  PortfolioAccount PDA (per user)     │
              │  portfolio_auth PDA (signer)         │
              │  portfolio_vault token account       │
              └──────────────┬───────────────────────┘
                             │ invoke_signed
                             ▼
              ┌──────────────────────────────────────┐
              │     percolator-prog (unchanged)      │
              │                                      │
              │  Engine.account.owner = portfolio_   │
              │    auth (set at InitUser via CPI)    │
              │  Engine performs IM/MMR/liquidation  │
              │    locally per market                │
              └──────────────────────────────────────┘
```

## What this is and isn't

It's USDC cross-margin: one user, one collateral pool, multiple
percolator markets. Profits in market A can backstop losses in market B
because an off-chain keeper rebalances collateral between them.

It's **not** cross-collateral: only USDC is supported. Multi-asset
collateral (SOL, BTC, etc.) is out of scope — that's the harder problem
with a much larger oracle attack surface.

## Three PDAs per user

| PDA | Seeds | Purpose |
|---|---|---|
| `portfolio_data` | `["portfolio", user]` | The 920-byte `PortfolioAccount` struct |
| `portfolio_auth` | `["portfolio_auth", user]` | Signing PDA for every CPI. Set as `account.owner` on every enrolled engine slot and as the token authority on `portfolio_vault`. |
| `portfolio_vault` | `["portfolio_vault", user]` | SPL Token account holding USDC between user-side transfers and per-market deposit/withdraw CPIs. |

## Why the vault

`percolator-prog::DepositCollateral` checks that the SPL transfer signer
matches `engine.account.owner`. If we want the engine account owned by
`portfolio_auth` (so the keeper can rebalance autonomously), the SPL
transfer source must also be authorized by `portfolio_auth`. That's
what `portfolio_vault` is — a per-user SPL Token account whose owner
is the PDA. Each Deposit becomes two transfers (user_ata → vault →
market_vault) at the cost of a small CU overhead.

## Instructions (12 total)

| Tag | Instruction | Signer | Implemented |
|---|---|---|---|
| 0  | `InitPortfolio`     | user   | yes |
| 1  | `EnrollMarket`      | user   | state-only (no InitUser CPI yet) |
| 2  | `UnenrollMarket`    | user   | yes |
| 3  | `Deposit`           | user   | yes |
| 4  | `Withdraw`          | user   | yes |
| 5  | `Trade`             | user   | deferred (matcher integration) |
| 6  | `Rebalance`         | keeper | yes (max 4 legs) |
| 7  | `EmergencyClose`    | user   | yes |
| 8  | `UpdateConfig`      | user   | yes |
| 9  | `SetPaused`         | user   | yes |
| 10 | `InitVault`         | user   | yes |
| 11 | `ClosePortfolio`    | user   | yes |

Setup flow for a new user: `InitPortfolio` → `InitVault` → `EnrollMarket × N`
→ `Deposit`. The keeper rebalances collateral between enrolled markets
when a per-market account approaches its local maintenance margin.

## Verification

- **97 integration tests** under `tests/`: state-mutation verification
  for happy paths, specific `PortfolioError` discriminant assertions
  for every rejection path, surgical field-corruption tests, e2e tests
  loading the real `percolator-prog` BPF binary.
- **25 Kani proofs** under `cfg(kani)` covering struct layout (size +
  alignment + zeroed init), instruction decode (never panics on
  arbitrary input, deterministic, per-tag strict-length), encode/decode
  round-trip, and bounds predicates.
- **CU bounds** pinned per instruction in `tests/test_cu_benchmark.rs`.

```sh
cargo build-sbf -- --locked      # BPF binary
cargo test --locked              # all integration tests
cargo kani --features kani       # all Kani proofs
```

## Project layout

```
src/
├── portfolio.rs          program: state, instructions, processor, kani proofs
└── cpi.rs                CPI builders for percolator-prog + spl-token

tests/
├── common/
│   ├── mod.rs            shared test helpers (assert_custom_error, ...)
│   └── integration_env.rs  e2e harness — loads percolator-prog .so, mints,
│                            oracles, market init, user funding
├── test_init_portfolio.rs
├── test_config.rs
├── test_enroll.rs
├── test_adversarial.rs
├── test_close_portfolio.rs
├── test_vault_and_cpi.rs
├── test_e2e.rs           round-trips against a real percolator-prog slab
├── test_conservation.rs  system-level invariants
└── test_cu_benchmark.rs  per-instruction CU upper bounds

.github/workflows/ci.yml  build-sbf, test, kani, fmt
Cargo.lock                committed for reproducible SBF builds
```

## Toolchain pins

`Cargo.toml` pins `blake3 = 1.8.1`, `indexmap = 2.7.1`,
`hashbrown = 0.15.2`, `once_cell = 1.20.2` to keep the SBF toolchain's
rustc 1.84 working — newer versions of those crates require
`edition2024` which isn't yet stable in that toolchain.

## Status / what's not yet done

| | |
|---|---|
| `EnrollMarketAndInit` | The current `EnrollMarket` is state-only. Wiring `percolator-prog::InitUser` from inside it (so the per-market account is created with `owner = portfolio_auth` automatically) is the next item. Once it's in, the integration harness can exercise full Deposit/Withdraw/Rebalance/EmergencyClose round-trips. |
| Trade (tag 5) | `percolator-prog::TradeNoCpi` requires a co-signing LP — matcher-side coordination, separate design. |
| Off-chain keeper bot | Designed, not written. Watches enrolled markets, computes portfolio health, submits `Rebalance` when buffer breached. |
| Real program ID | Currently a placeholder. Needs `solana-keygen grind` before deployment. |
| Conservation tests | Framework is in place (`tests/test_conservation.rs`); INV-1 is active, INV-2 through INV-6 are gated on `EnrollMarketAndInit`. |

## License

Apache-2.0. See [`LICENSE`](./LICENSE).
