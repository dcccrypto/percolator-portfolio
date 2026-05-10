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

## Instructions (13 total)

| Tag | Instruction | Signer | Implemented |
|---|---|---|---|
| 0  | `InitPortfolio`        | user   | yes |
| 1  | `EnrollMarket`         | user   | state-only (advanced flow — register a separately-init'd account) |
| 2  | `UnenrollMarket`       | user   | yes |
| 3  | `Deposit`              | user   | yes |
| 4  | `Withdraw`             | user   | yes |
| 5  | `Trade`                | user   | yes (CPI to `percolator-prog::TradeCpi`) |
| 6  | `Rebalance`            | keeper | yes (max 4 legs) |
| 7  | `EmergencyClose`       | user   | yes |
| 8  | `UpdateConfig`         | user   | yes |
| 9  | `SetPaused`            | user   | yes |
| 10 | `InitVault`            | user   | yes |
| 11 | `ClosePortfolio`       | user   | yes |
| 12 | `EnrollMarketAndInit`  | user   | yes (atomic — transfers fee, CPIs `InitUser`, registers slot) |

Recommended setup flow: `InitPortfolio` → `InitVault` → `EnrollMarketAndInit × N`
(transfers `fee_payment` from user_ata, signs `InitUser` as `portfolio_auth`,
records the slot). Then `Deposit` to fund trading capital. The keeper
rebalances collateral between enrolled markets when a per-market account
approaches its local maintenance margin.

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

## Design rationale (per upstream maintainer feedback, 2026-05-10)

The architectural shape of this wrapper was confirmed by the upstream
maintainer when reviewing three related PRs against the engine
(`aeyakovenko/percolator#58`) and wrapper (`#87`, `#88`). Each was
closed as out of scope, with the rationale that the cross-margin use
case should be handled entirely at the wrapper layer — without growing
the engine's public surface. The closes shaped four design points
that are now load-bearing here:

1. **Wrapper PDA is a stable owner.** Engine accounts are owned by
   `portfolio_auth` from `InitUser` onward, forever. There is no
   engine `transfer_owner` ABI — and the wrapper does not need one.
   Programmable custody (key rotation, M-of-N, social recovery) is
   solved by the wrapper PDA being a stable address whose internal
   signing rules can change. A user wanting Squads-style multisig
   custody initialises the portfolio with a Squads multisig PDA as
   `owner`; the engine never sees the signer set change.

2. **Engine read-views are not authoritative for trade admission.**
   Toly's review on the proposed `GetAccountHealth` ix made the point
   explicitly: a cached-`last_oracle_price` view doesn't reflect the
   crank/target-lag design, and so cannot be used as a pre-trade
   admission gate. The wrapper's pre-trade aggregate margin check (when
   it lands) MUST use a fresh Pyth price for each market, not the
   slab's cached price, and MUST mirror the engine's equity / margin
   math rather than relying on a slab snapshot. The cost of mirroring
   is the wrapper's problem, not the engine's API surface.

3. **Soft cross-margin, not hard.** The engine's per-account
   `is_above_initial_margin` runs against a fresh oracle inside every
   `TradeCpi` invocation we issue — that's the admission gate. The
   wrapper does NOT do a parallel aggregate-IM check, because doing so
   would mean either (a) duplicating engine math wrapper-side and
   re-pinning on every engine schema change, or (b) adding an engine
   API the maintainer has rejected. Cross-margin behaviour comes from
   one shared USDC vault that the keeper rebalances between markets —
   surplus from market A tops up market B before B's individual MM is
   breached. The user-visible effect is "profits in A backstop losses
   in B" without ever asking the engine to allow an individual account
   to run below its per-market MM. See `src/margin.rs` for the full
   design-boundary writeup.

4. **No engine ABI surface added for owner rotation.** The maintainer's
   concern on the proposed `UpdateAccountOwner` was that an
   owner-transfer ABI expands authority surface around withdrawals,
   close, fee-credit payment, self-crank auth, and trade authorization
   — each becomes a new attack vector if a compromised current owner
   can hand off. By keeping ownership rotation inside the wrapper PDA
   layer (where the wrapper enforces M-of-N or social recovery rules)
   instead of in the engine, the engine's authority model stays
   minimal and consistent.

These constraints are why the wrapper:
- depends on `percolator` (the engine crate) only as a read-only type
  source so structs can be addressed without re-implementing layout,
- does not propose engine API additions for read-views or rotation, and
- ships soft cross-margin: engine enforces per-account IM/MM with a
  fresh oracle on every TradeCpi we issue; the keeper handles the
  cross-market collateral movement that gives users the
  "one-pool-many-markets" UX. Hard cross-margin (true aggregate IM
  enforcement that lets one account run below its per-market MM if
  another covers it) would require engine changes the maintainer
  has rejected and is explicitly out of scope.

## Squads-style custody (recipe)

The wrapper-PDA-as-stable-owner architecture means key rotation is the
custody program's job, not the engine's or the wrapper's. To get a
multisig-controlled portfolio that rotates signers without ever
touching the engine, point `InitPortfolio` at a Squads multisig PDA
instead of a user wallet. The wrapper records that PDA as the portfolio
owner; from then on, every owner-gated wrapper instruction must be
proposed → approved → executed through Squads.

### One-time setup

1. **Create the Squads V4 multisig** off-chain. Note the resulting
   multisig PDA address — call it `MS`.
2. **Construct an `InitPortfolio` instruction** using `MS` as the user
   (signer). The portfolio PDAs derive from `MS`:
   - `portfolio_data` = `["portfolio", MS]`
   - `portfolio_auth` = `["portfolio_auth", MS]`
   - `portfolio_vault` = `["portfolio_vault", MS]`
3. **Submit through Squads** as a proposal. Once members approve and
   execute, Squads dispatches the `InitPortfolio` CPI with `MS` as the
   signing PDA. The wrapper sees `MS` as the user and stores it as the
   portfolio owner.
4. **`InitVault`** the same way — propose, approve, execute through
   Squads.

After step 4 the portfolio is live. The wrapper sees `MS` as the owner
forever; the engine sees `portfolio_auth` (derived from `MS`) as the
account owner of every enrolled market slot.

### Rotating signers

Rotation happens entirely inside Squads — add/remove members, change
threshold — using Squads' own multisig admin instructions. Neither the
wrapper nor the engine is involved. The multisig PDA `MS` stays the
same, so:
- `portfolio_data`, `portfolio_auth`, `portfolio_vault` PDAs unchanged
- engine-side `account.owner = portfolio_auth` unchanged
- no on-chain wrapper or engine state needs migration

This is exactly the property the upstream maintainer pointed to in
`aeyakovenko/percolator-prog#88`: "an external custody program can
initialize the account with its PDA as the owner and then rotate
keys/signers internally." The recipe above is that pattern.

### Caveats

- **Squads multisig must remain solvent for rent.** If `MS` runs out
  of lamports and gets cleaned up by the runtime, the portfolio is
  permanently bricked (no path to re-derive a new owner without
  engine `transfer_owner`, which doesn't exist by design).
- **Threshold-of-one is single-key custody.** A 1-of-1 Squads is
  functionally equivalent to a wallet-owned portfolio — no real
  custody benefit. Use ≥ 2-of-N for actual multisig.
- **Programmatic rebalance is fine.** The keeper field on the
  portfolio is independent of the owner. A 2-of-3 Squads can keep a
  hot keeper key for autonomous `Rebalance` while owner-gated ops
  (Withdraw, EmergencyClose, UpdateConfig) still require multisig
  approval.
- **Squads V4 only.** The recipe above assumes V4's PDA-as-signer
  model. V3 does not expose its multisig PDA the same way.

## Status / what's not yet done

| | |
|---|---|
| Hard cross-margin | Out of scope by design. `src/margin.rs` and `src/pyth.rs` document the constraint explicitly. Soft cross-margin via keeper rebalance is the canonical model; hard cross-margin would require engine API changes the maintainer has rejected. |
| Test harness `Custom(4)` | `tests/common/integration_env.rs:332` fails on `InitMarket` setup with `Custom(4)`, blocking 5 e2e tests that would otherwise exercise the full Trade / EnrollMarketAndInit flow. Pre-existing, unrelated to recent changes. Needs a separate debugging session. |
| Off-chain keeper bot | Designed, not written. Watches enrolled markets, computes portfolio health, submits `Rebalance` when buffer breached. |
| Real program ID | Currently a placeholder. Needs `solana-keygen grind` before deployment. |
| Conservation tests | Framework is in place (`tests/test_conservation.rs`); INV-1 is active, INV-2 through INV-6 are gated on `EnrollMarketAndInit`. |
| Engine pin tracking | The engine + wrapper-prog repos this consumes are mid-sync to upstream Toly across an 8-wave plan (~3-4 weeks). Each wave that changes RiskEngine schema (Waves 1, 4, 5, 6) requires re-pinning + re-testing the margin port (when present). Tracked in `~/wrapper-engine-deep-audit/FULL_SYNC_PLAN.md`. |

## License

Apache-2.0. See [`LICENSE`](./LICENSE).
