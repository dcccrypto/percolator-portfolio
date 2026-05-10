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

It's **USDC cross-margin**: one user, one collateral pool, multiple
percolator markets. Profits in market A backstop losses in market B —
not because the engine allows individual accounts to run below their
per-market margin (it doesn't, by design), but because the wrapper
moves collateral between markets via three layered defenses:

- **Pre-trade aggregate IM check** rejects trades that would push the
  portfolio's total equity below total IM, even when each individual
  account looks fine to the engine.
- **Permissionless rebalance crank** lets anyone (you, an MEV bot,
  anyone watching Pyth) top up an at-risk account in exchange for a
  small bounty — recruits arbitrage as auxiliary keepers.
- **A canonical keeper bot** can run alongside both, doing the
  steady-state work without paying itself the bounty.

It's **not** cross-collateral: only USDC is supported. Multi-asset
collateral (SOL, BTC, etc.) is out of scope — that's the harder problem
with a much larger oracle attack surface.

It's **not** true Hyperliquid-style hard cross-margin: individual
accounts cannot run below their per-market maintenance margin
(the engine still enforces that). What we ship instead is a
defence-in-depth that prevents accounts from ever falling that far
in practice. The user-felt behaviour is ~95% of the same thing.

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

## Instructions (14 total)

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
| 13 | `RebalanceCrank`       | **any** | yes (Defense 3 — permissionless top-up with bounty) |

Recommended setup flow: `InitPortfolio` → `InitVault` → `EnrollMarketAndInit × N`
(transfers `fee_payment` from user_ata, signs `InitUser` as `portfolio_auth`,
records the slot). Then `Deposit` to fund trading capital and `Trade` to
open positions. Cross-margin behaviour comes from the three defenses
detailed below — primarily `RebalanceCrank` (anyone can call) keeping
per-market accounts topped up before they breach MM.

## Cross-margin model: soft+ (Defense 1 + Defense 3)

This wrapper ships "soft+ cross-margin" — engine enforces per-account
IM/MM as the safety gate, the wrapper adds two ADDITIONAL gates that
make the user-felt behaviour ~95% of Hyperliquid-style hard
cross-margin. Engine API additions for true hard cross-margin (the
ability to let an individual account run below per-market MM under
wrapper authority) were closed by the maintainer in #58/#87/#88 and
are out-of-scope by design.

### Defense 1 — pre-trade aggregate IM check
Every `Trade` ix verifies that `sum(equity_i) ≥ sum(im_req_i)` across
all enrolled markets BEFORE issuing the TradeCpi. Each market's
equity is computed via the engine's public `account_equity_maint_raw`;
each market's IM requirement uses the engine's `try_notional` against
a fresh-this-slot Pyth oracle decoded via the same policy
percolator-prog applies internally (feed_id + staleness + confidence,
all from the slab's own MarketConfig). Caller passes (slab, oracle)
pairs for every enrolled market beyond the trade target; the wrapper
cross-validates membership and rejects duplicates. Math is mirrored
line-for-line from `engine.is_above_initial_margin` and Kani-proven
to saturate conservatively on overflow.

What this catches that engine-alone doesn't:
- Portfolio insolvency where the trade target is fine individually
  but other accounts are bleeding
- Trades that pass per-account IM but push portfolio aggregate
  IM headroom below zero

### Defense 3 — permissionless rebalance crank
Tag 13 `RebalanceCrank` is callable by **any** signer. Pre-check gate:
the destination account must be BELOW its per-market initial-margin
requirement; otherwise the crank rejects with `CrankNotNeeded`. On
success the wrapper signs Withdraw + Deposit CPIs as `portfolio_auth`,
then pays the caller a small bounty (min(amount / 100, 1 USDC)) from
the portfolio vault. Recruits MEV / arbitrage bots as auxiliary
keepers without inviting abuse — bounty is zero on dust-sized
rebalances, self-legs are rejected, paused portfolios block the crank.

### Atomic trade-with-rebalance (via client-side composition)

A frequent UX want is "if my account is approaching MM at trade time,
rebalance and trade atomically." This wrapper does NOT bundle a
dedicated `TradeWithRebalance` ix because the account-list cost
(adding source-market-vault + vault-authority + oracle accounts to
every Trade) pushes typical-size transactions beyond the Solana
account-count budget. Instead, **clients compose two instructions
into one transaction**:

```
[ ComputeBudget |
  portfolio::RebalanceCrank { from_idx, to_idx, amount },
  portfolio::Trade { account_idx, lp_idx, side, size_q, limit_price_e6 }
]
```

Both run in the same transaction → same atomicity guarantee. If the
rebalance crank fails (e.g., dest was already above IM), the whole
transaction reverts → the Trade doesn't go through under stale state.
SDK helpers can synthesize this composition automatically; the
on-chain program stays simple.

### Residual gap (single-block race)

The one failure mode NEITHER soft+ cross-margin NOR true hard
cross-margin can fully close is the single-block race: oracle ticks
adversely, no rebalance tx lands in block N, a liquidator lands a
KeeperCrank against an account that dropped below MM in block N.
Mitigation is operational — keep per-market MM buffers wide enough
that intra-block oracle moves can't push accounts below MM. Same
constraint every DeFi perp has.

## Verification

- **96 integration tests** under `tests/` (96/96 pass): state-mutation
  verification for happy paths, specific `PortfolioError` discriminant
  assertions for every rejection path, surgical field-corruption tests,
  e2e tests loading the real `percolator-prog` BPF binary, CU bounds
  pinned per instruction.
- **36 Kani proofs** under `cfg(kani)` (36/36 verified) covering:
  struct layout (size + alignment + zeroed init), instruction decode
  (never panics on arbitrary input, deterministic, per-tag strict-
  length), encode/decode round-trip for every tag, bounds predicates,
  and the new margin-math invariants (`project_basis` saturation,
  `im_req_from_notional` floor + concrete cases, `cast_aggregate_im_req`
  saturation, `aggregate_admissible` comparison direction, spec §9.1
  basis-zero short-circuit).
- **SBF build clean** with no warnings on our code.

```sh
cargo build-sbf -- --locked      # BPF binary
cargo test --locked              # all integration tests
cargo kani --features kani       # all Kani proofs
```

## Project layout

```
src/
├── portfolio.rs          program: state, instructions, processor, kani proofs
├── cpi.rs                CPI builders for percolator-prog + spl-token
├── margin.rs             Defense 1 aggregate-IM math + 7 Kani proofs
└── pyth.rs               oracle decoder (delegates to percolator-prog)

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
   Toly's #87 review made the point explicitly: a
   cached-`last_oracle_price` view doesn't reflect the crank/target-lag
   design, and so cannot be used as a pre-trade admission gate. The
   wrapper's Defense 1 aggregate-IM check therefore decodes a fresh
   Pyth oracle for each enrolled market on every Trade — via the same
   `percolator_prog::oracle::read_pyth_price_e6` helper percolator-prog
   uses internally, honouring each market's own staleness + confidence
   policy from its MarketConfig.

3. **Soft+ cross-margin: engine enforces per-account, wrapper enforces
   portfolio aggregate.** The engine's per-account
   `is_above_initial_margin` runs against the fresh oracle inside every
   `TradeCpi` invocation we issue — that's the *safety* gate. On top of
   that the wrapper layers two additional gates (Defenses 1 and 3) that
   make the user-felt behaviour ~95% of true hard cross-margin. The
   "cost of mirroring" engine math is borne by the wrapper, accepted
   deliberately: re-pin + re-test on every upstream sync wave is cheap
   compared to losing aggregate enforcement. See "Cross-margin model"
   above for the full mechanics.

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
- depends on `percolator` and `percolator-prog` (with `no-entrypoint`)
  as read-only type + decoder sources — so it can decode slabs and
  call public engine helpers without re-implementing struct layouts,
- does not propose engine API additions for read-views or rotation,
- ships **soft+ cross-margin**: engine enforces per-account IM/MM on
  every TradeCpi (fresh oracle); the wrapper adds pre-trade aggregate
  IM enforcement (Defense 1) and a permissionless rebalance crank
  (Defense 3) that recruits MEV bots as auxiliary keepers. True hard
  cross-margin (individuals allowed below per-market MM) is
  out-of-scope-by-design — would require the engine PR closed in #58.

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
| Off-chain keeper bot (canonical operator) | Reference implementation not written. Watches enrolled markets, computes per-account margin against fresh oracles, submits `Rebalance` when buffer breached. `RebalanceCrank` being permissionless means third parties will also crank for the bounty, but the canonical operator handles the steady-state. **This is the only thing that has to be running for cross-margin to deliver its full value in practice** — without any rebalancer, the system degrades to N isolated markets sharing a deposit vault. |
| Atomic `TradeWithRebalance` ix | Deferred — clients can compose `RebalanceCrank` + `Trade` into a single transaction for the same atomicity guarantee without bloating the on-chain account list. See "Atomic trade-with-rebalance" section above. |
| Real program ID | Currently a placeholder. Needs `solana-keygen grind` before deployment. |
| External audit | Required before mainnet. The 36 Kani proofs + 96 integration tests are the pre-audit floor; a wrapper of this shape will still want an external review. |
| Engine pin tracking | The engine + wrapper-prog repos this consumes are mid-sync to upstream Toly across an 8-wave plan. Each wave that changes RiskEngine schema (Waves 1, 4, 5, 6) requires re-pinning + re-testing — `cargo build` fails loudly on schema drift since we depend on the engine crate as a type source, so this is "noisy breakage, easy to fix" not silent corruption. Tracked in `~/wrapper-engine-deep-audit/FULL_SYNC_PLAN.md`. |

## License

Apache-2.0. See [`LICENSE`](./LICENSE).
