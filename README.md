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
| `portfolio_data` | `["portfolio", user]` | The 888-byte `PortfolioAccount` struct |
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

Post-audit hardening: CRIT-1 (spoofed `portfolio_data` blocked), CRIT-2
(crank bounty pre-check + dedicated error), CRIT-3 (separate `has_vault`
flag — no `vault_bump==0` collision), CRIT-5 (loop bounds clamped to
`MAX_ENROLLED_MARKETS`), CRIT-6 (enrollment rejects duplicate markets so
Defense 1's pair-region lookup is unambiguous), CRIT-7 (vault delta uses
`checked_sub`), H-1 (`caller_payout_ata.owner` bound to caller), H-4
(`read_token_account_amount` enforces SPL Token owner). H-2 + H-3 align
`margin.rs` with engine's exact IM equity formula and oracle-range
rejection. All shipped in commit `4be426b`.

### Defense 1 — pre-trade aggregate IM check
Every `Trade` ix verifies that `sum(equity_i) ≥ sum(im_req_i)` across
all enrolled markets BEFORE issuing the TradeCpi. Each market's
equity is computed via the engine's public `account_equity_init_raw`
(H-2 fix — matches what `is_above_initial_margin` itself uses, with
positive PnL clamped to `≤ 0 + matured haircut`); each market's IM
requirement uses the engine's `try_notional` against a fresh-this-slot
Pyth oracle decoded via the same policy percolator-prog applies
internally (feed_id + staleness + confidence, all from the slab's own
MarketConfig). Caller passes (slab, oracle) pairs for every enrolled
market beyond the trade target; the wrapper cross-validates membership
and rejects duplicates. Math is mirrored line-for-line from
`engine.is_above_initial_margin` and Kani-proven to saturate
conservatively on overflow.

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

- **136 integration tests** under `tests/`: state-mutation verification
  for happy paths, specific `PortfolioError` discriminant assertions for
  every rejection path, surgical field-corruption tests, e2e tests
  loading the real `percolator-prog` BPF binary, CU bounds pinned per
  instruction, post-audit regression tests for each CRIT/HIGH fix.
- **37 Kani proofs** under `cfg(kani)` covering: struct layout (size +
  alignment + zeroed init), instruction decode (never panics on
  arbitrary input, deterministic, per-tag strict-length), encode/decode
  round-trip for every tag, bounds predicates, and the margin-math
  invariants (`project_basis` saturation, `im_req_from_notional` floor
  + concrete cases, `cast_aggregate_im_req` saturation,
  `aggregate_admissible` comparison direction, spec §9.1 basis-zero
  short-circuit).
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
├── test_enroll_and_init.rs   tag 12 atomic enroll + InitUser CPI
├── test_trade.rs             tag 5 Trade ix + Defense 1 pair region
├── test_rebalance_crank.rs   tag 13 RebalanceCrank + bounty + audit fixes
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
| Off-chain keeper bot (canonical operator) | Reference design documented in "Integration examples" (Example 3) and the audit synthesis; production implementation not yet written. The canonical operator watches enrolled markets, computes per-account margin against fresh oracles, and submits `Rebalance` (or `RebalanceCrank` from a third-party fleet) when buffers are breached. **This is the only thing that has to be running for cross-margin to deliver its full value in practice** — without any rebalancer, the system degrades to N isolated markets sharing a deposit vault. CRIT-2's mitigation means the canonical operator should also keep portfolio vaults topped up with enough idle USDC to cover bounty payouts. |
| Real program ID | Placeholder `PercoFoLPort1111111111111111111111111111111` baked into `declare_id!` at `src/portfolio.rs:26`. Needs `solana-keygen grind` before any non-localnet deployment. See `## Deploy checklist`. |
| External audit | Required before mainnet. The 37 Kani proofs + 136 integration tests + completed in-house audit synthesis are the pre-audit floor; a wrapper of this shape will still want an external review. |
| Engine pin tracking | The engine + wrapper-prog repos this consumes are mid-sync to upstream Toly across an 8-wave plan. Each wave that changes RiskEngine schema (Waves 1, 4, 5, 6) requires re-pinning + re-testing — `cargo build` fails loudly on schema drift since we depend on the engine crate as a type source, so this is "noisy breakage, easy to fix" not silent corruption. See `## Engine-coupled symbols` for the index of every load-bearing import; tracked operationally in `~/wrapper-engine-deep-audit/FULL_SYNC_PLAN.md`. |

## Audit findings addressed

Five-agent audit pass completed prior to external review. Each finding
has a precise inline reference (`CRIT-N`, `H-N`) at its fix site for
cross-referencing. All shipped in commit `4be426b`.

| ID | Severity | Description | Fix site |
|---|---|---|---|
| CRIT-1 | Critical | `RebalanceCrank` trusted a spoofed `portfolio_data` — added `a_data.owner == program_id` so attackers can no longer drain bounty by mimicking a victim's struct bytes. | `src/portfolio.rs:1348` |
| CRIT-2 | Critical (mitigated) | Bounty depends on pre-existing vault balance; added pre-check + `BountyVaultUnderfunded` so cranker bots skip empty portfolios without burning CU. Full fix requires redesign — documented as operational constraint. | `src/portfolio.rs:1457` |
| CRIT-3 | Critical | `vault_bump == 0` sentinel collided with a legitimate canonical bump for ~1/256 users. Added separate `has_vault: u8` flag at offset 116; all eight sentinel sites updated. | `src/portfolio.rs:311` (state), all `has_vault` checks |
| CRIT-5 | Critical | `enrolled_count` is `u8` (0..=255) but `enrolled` has 16 slots — corrupted state could OOB-panic the program. Every loop bound clamped to `MAX_ENROLLED_MARKETS`. | `src/portfolio.rs:1084,1203,2485,2837,2914` |
| CRIT-6 | Critical | Allowing multi-account-same-market broke Defense 1's pair-region lookup (matches by market pubkey alone). Enrollment now rejects any duplicate of `market`. | `src/portfolio.rs:1096,1209` |
| CRIT-7 | Critical | `emergency_close` used `saturating_sub` for `vault_after − vault_before`; an underflow silently returned 0 and the user got nothing. Replaced with `checked_sub` → `ArithmeticOverflow`. | `src/portfolio.rs:2806` |
| H-1 | High | `caller_payout_ata` not bound to caller — social-engineering vector. Added SPL Token owner-byte check at offset 32. | `src/portfolio.rs:1474` |
| H-2 | High | `margin.rs` used `account_equity_maint_raw` (full positive PnL), which is more permissive than engine's `is_above_initial_margin` (clamps PnL to ≤ 0 + matured haircut). Switched to `account_equity_init_raw`. | `src/margin.rs:167` |
| H-3 | High | `margin.rs` only rejected `oracle_price == 0`; engine also rejects `> MAX_ORACLE_PRICE`. Aligned the check. | `src/margin.rs:179` |
| H-4 | High | `read_token_account_amount` accepted any 72+-byte account — attacker could feed bytes that decode as a fake balance. Added SPL Token owner check. | `src/portfolio.rs:2860` |

Regression tests cover each fix in `tests/test_enroll.rs`,
`tests/test_rebalance_crank.rs`, and `tests/test_vault_and_cpi.rs` —
e.g. `enroll_rejects_same_market_different_idx` (CRIT-6),
`crank_rejects_spoofed_data` (CRIT-1).

## Integration examples

Snippets below use placeholder imports for an SDK module
(`@percolator/portfolio-sdk`) that mirrors the on-chain instruction
layout. The wrapper itself is on-chain only; the SDK is the planned
TypeScript companion. Use `findProgramAddressSync` against the program
ID baked into `src/portfolio.rs:26`. All instruction tags + account
orders below are extracted from `src/portfolio.rs::processor::*`.

Examples 2 and 3 reuse the `conn`, `user`, `portfolioData`,
`portfolioAuth`, `portfolioVault`, and `userUsdcAta` bindings from
Example 1 — imports omitted there for brevity (`SYSVAR_CLOCK_PUBKEY`
from `@solana/web3.js`, `TOKEN_PROGRAM_ID` from `@solana/spl-token`).

### Example 1 — Set up a portfolio with one market

```typescript
import {
  Connection, Keypair, PublicKey, Transaction, sendAndConfirmTransaction,
} from "@solana/web3.js";
import { TOKEN_PROGRAM_ID, getAssociatedTokenAddressSync } from "@solana/spl-token";
import {
  PORTFOLIO_PROGRAM_ID,         // PercoFoLPort1111111111111111111111111111111
  PERCOLATOR_PROGRAM_ID,        // ESa89R5Es3rJ5mnwGybVRG1GrNt9etP11Z5V2QWD4edv
  USDC_MINT,
  ixInitPortfolio, ixInitVault, ixEnrollMarketAndInit, ixDeposit,
} from "@percolator/portfolio-sdk";

const conn = new Connection("https://api.mainnet-beta.solana.com");
const user = Keypair.generate();                          // or wallet adapter
const marketSlab = new PublicKey("…");                    // existing percolator-prog market
const marketVault = new PublicKey("…");                   // market's collateral vault

// Three PDAs derived from user.publicKey
const [portfolioData] = PublicKey.findProgramAddressSync(
  [Buffer.from("portfolio"),       user.publicKey.toBuffer()], PORTFOLIO_PROGRAM_ID);
const [portfolioAuth] = PublicKey.findProgramAddressSync(
  [Buffer.from("portfolio_auth"),  user.publicKey.toBuffer()], PORTFOLIO_PROGRAM_ID);
const [portfolioVault] = PublicKey.findProgramAddressSync(
  [Buffer.from("portfolio_vault"), user.publicKey.toBuffer()], PORTFOLIO_PROGRAM_ID);

const userUsdcAta = getAssociatedTokenAddressSync(USDC_MINT, user.publicKey);

const tx = new Transaction()
  .add(ixInitPortfolio({ user: user.publicKey, portfolioData, portfolioAuth,
                         bufferBps: 200, maxLeverageBps: 50_000,
                         keeper: user.publicKey.toBytes() /* self-keep initially */ }))
  .add(ixInitVault({ user: user.publicKey, portfolioData, portfolioAuth,
                     portfolioVault, mint: USDC_MINT }))
  .add(ixEnrollMarketAndInit({
        user: user.publicKey, portfolioData, portfolioAuth, portfolioVault,
        userAta: userUsdcAta, marketSlab, marketVault,
        expectedIdx: 7,           // read from slab pre-tx; wrapper records without verifying
        feePayment: 50_000_000n,  // 50 USDC: split engine-side into fee + initial capital
      }))
  .add(ixDeposit({
        user: user.publicKey, portfolioData, portfolioAuth, portfolioVault,
        userAta: userUsdcAta, marketSlab, marketVault,
        marketVaultAuthority: new PublicKey("…"),    // engine-derived
        accountIdx: 7, amount: 1_000_000_000n,       // 1_000 USDC trading capital
      }));

await sendAndConfirmTransaction(conn, tx, [user]);
```

### Example 2 — Open a leveraged position with cross-margin health enforcement

The load-bearing piece of Defense 1 is the **margin-pair region**:
`Trade` expects `(slab, oracle)` for every OTHER enrolled market beyond
the trade target. The wrapper reads each one's fresh Pyth price, decodes
the slab via `percolator_prog::zc::engine_ref`, and runs the aggregate
IM check before issuing `TradeCpi`. Omit a market and the ix rejects
with `WrongMarginAccountCount`; pass a duplicate and you get
`MarginSlabDuplicate`.

```typescript
import { ComputeBudgetProgram, AccountMeta } from "@solana/web3.js";
import {
  ixTrade, ixRebalanceCrank, deriveMarginPairs,
} from "@percolator/portfolio-sdk";

// Off-chain helper: read portfolio_data, pull every enrolled (slab, oracle)
// EXCEPT the trade target, sorted in any order (the wrapper sorts by mask bit).
const marginPairs: AccountMeta[] = await deriveMarginPairs(conn, portfolioData, {
  tradeTargetSlab: solUsdcSlab,
});

// Matcher tail varies by LP and matcher program — passed straight to TradeCpi.
const matcherTail: AccountMeta[] = await buildMatcherTail(conn, lpPda);

const tradeIx = ixTrade({
  // Fixed accounts [0..11]
  user: user.publicKey,
  portfolioData, portfolioAuth,
  marketSlab: solUsdcSlab, clockSysvar: SYSVAR_CLOCK_PUBKEY,
  oracle: solUsdcOracle,
  matcherProgram: matcherProgramId, matcherContext: matcherCtx,
  lpPda, lpOwner, percolatorProgram: PERCOLATOR_PROGRAM_ID,
  // Defense 1 pair region: 2·(N-1) accounts where N = enrolled_count
  marginPairs,
  // Variadic matcher tail (forwarded to percolator-prog::TradeCpi)
  matcherTail,
  // Ix body
  accountIdx: 7,           // SOL/USDC slot
  lpIdx: 3,                // an existing LP on this slab
  side: 0,                 // 0=long, 1=short
  sizeQ: 5_000_000_000n,   // q-units (engine POS_SCALE)
  limitPriceE6: 145_000_000n,  // $145.00 in e6
});

// If you suspect the trade-target account is near IM, compose RebalanceCrank
// + Trade in ONE tx for atomicity — wrapper has no dedicated TradeWithRebalance
// ix because the combined account-count would exceed the per-tx budget.
const tx = new Transaction()
  .add(ComputeBudgetProgram.setComputeUnitLimit({ units: 800_000 }))
  .add(ixRebalanceCrank({
        caller: user.publicKey,         // user can self-crank — bounty just returns to vault
        portfolioData, portfolioAuth, portfolioVault,
        callerPayoutAta: userUsdcAta,
        tokenProgram: TOKEN_PROGRAM_ID, clockSysvar: SYSVAR_CLOCK_PUBKEY,
        percolatorProgram: PERCOLATOR_PROGRAM_ID,
        fromSlab: btcUsdcSlab, fromVault: btcUsdcMarketVault,
        fromVaultAuthority: btcUsdcVaultAuth, fromOracle: btcUsdcOracle,
        toSlab: solUsdcSlab, toVault: solUsdcMarketVault, toOracle: solUsdcOracle,
        fromIdx: 4, toIdx: 7, amount: 100_000_000n,    // 100 USDC top-up
      }))
  .add(tradeIx);

// Either both land or the whole transaction reverts → no Trade against stale state.
await sendAndConfirmTransaction(conn, tx, [user]);
```

### Example 3 — Run `RebalanceCrank` as an MEV / arbitrage bot

`RebalanceCrank` (tag 13) is callable by any signer. The wrapper pays
`min(amount / 100, 1_000_000)` from the portfolio vault — 1% of the
moved amount capped at 1 USDC base unit. The crank rejects with
`CrankNotNeeded` unless the destination account is genuinely below its
per-market initial-margin requirement at call time, so legitimate use
is the only payable path. Off-chain bots scan portfolios by reading
`portfolio_data` accounts (owner = program ID) and each enrolled
market's slab via `getAccountInfo`, then decode locally with the same
zero-copy types `engine_ref` returns.

```typescript
import { ixRebalanceCrank, decodePortfolioAccount, decodeEngine } from "@percolator/portfolio-sdk";

// 1. Discover portfolios by program-owned getProgramAccounts.
const portfolios = await conn.getProgramAccounts(PORTFOLIO_PROGRAM_ID, {
  filters: [{ dataSize: 888 }, { memcmp: { offset: 0, bytes: MAGIC_B58 } }],
});

for (const { pubkey: portfolioData, account } of portfolios) {
  const pa = decodePortfolioAccount(account.data);     // bytemuck-equivalent
  if (pa.paused !== 0 || pa.enrolledCount === 0) continue;

  // 2. Compute portfolio vault — bounty source. CRIT-2: skip if underfunded.
  const portfolioVault = derivePortfolioVault(pa.owner);
  const vaultBal = await getTokenBalance(conn, portfolioVault);
  if (vaultBal < 1_000n) continue;     // 0.001 USDC minimum slack

  // 3. For every (market, idx) pair, decode the slab and check engine IM.
  for (const slot of pa.enrolled.slice(0, pa.enrolledCount)) {
    const slabAi = await conn.getAccountInfo(new PublicKey(slot.market));
    const engine = decodeEngine(slabAi!.data);          // mirrors zc::engine_ref
    const oraclePrice = await readPythE6(conn, slot.market, slabAi!.data);
    if (engine.isAboveInitialMargin(slot.accountIdx, oraclePrice)) continue;

    // 4. Found a below-IM account. Pick a source market with surplus equity.
    const source = pickSourceMarket(pa, slot);           // your own heuristic
    if (!source) continue;

    // 5. Construct the 15-account RebalanceCrank ix (see src/portfolio.rs:1281).
    const tx = new Transaction().add(ixRebalanceCrank({
      caller: bot.publicKey,
      portfolioData, portfolioAuth: derivePortfolioAuth(pa.owner),
      portfolioVault, callerPayoutAta: botUsdcAta,
      tokenProgram: TOKEN_PROGRAM_ID, clockSysvar: SYSVAR_CLOCK_PUBKEY,
      percolatorProgram: PERCOLATOR_PROGRAM_ID,
      fromSlab: new PublicKey(source.market),  fromVault: source.marketVault,
      fromVaultAuthority: source.vaultAuth,    fromOracle: source.oracle,
      toSlab: new PublicKey(slot.market),      toVault: slot.marketVault,
      toOracle: slot.oracle,
      fromIdx: source.accountIdx, toIdx: slot.accountIdx,
      amount: pickAmount(slot, source),         // bounty = min(amount/100, 1e6)
    }));
    await sendAndConfirmTransaction(conn, tx, [bot]);
  }
}
```

Economics: at $1 cap and ~5K CU for the bounty CPI plus ~170K CU per
Withdraw + Deposit, the bot pays roughly $0.0005 in priority fees to
earn up to $1 — net positive whenever the destination is genuinely
below IM and the move is ≥ 100 USDC. Below that, the bounty floors to a
proportional fraction; below ~10 USDC the bot should skip on cost.

## Security threat model

Each row enumerates an attacker capability, the actual blast radius
under current invariants, and the recovery path. Mitigations cite the
fix site where applicable.

### Owner-key compromise (user's private key)

A compromised owner can submit `Trade`, `Withdraw`, `EmergencyClose`,
`UpdateConfig`, `SetPaused`, and `ClosePortfolio`. They CANNOT touch
any other user's portfolio because every wrapper PDA derivation uses
`pa.owner` (set immutably at `InitPortfolio`) as a seed. Trades route
through the engine's per-account IM/MM check — the attacker can't open
naked-short positions that exit the engine's risk envelope. **Recovery
path**: re-init under a fresh user pubkey, or — preferred — initialise
the portfolio with a Squads V4 multisig PDA as owner (see "Squads-style
custody" above) so a single key compromise doesn't grant unilateral
control. Time-locked `UpdateConfig` is a planned defense.

### Keeper-key compromise

A compromised keeper can call `Rebalance` between enrolled markets at
adversarial timing, but `Withdraw + Deposit` is net-zero on the vault
and the keeper has **no withdraw authority** to any external ATA.
Worst case: keeper rebalances at attacker timing, creating temporary
mis-allocation; the engine's per-account IM check catches every
downstream `Trade` issued against the now-mis-allocated state, and a
`RebalanceCrank` immediately undoes the bad rebalance (paying the
caller a bounty in the process). **Recovery path**: owner calls
`UpdateConfig` to rotate `keeper` to a fresh pubkey.

### Oracle compromise / staleness

The engine's `percolator_prog::oracle::read_pyth_price_e6` enforces
fresh + low-confidence reads per each market's own `MarketConfig`
(feed_id, `max_staleness_secs`, `conf_filter_bps`). Defense 1 calls the
**same helper** for every enrolled market — so a stale or wide-conf
oracle for ANY market rejects the `Trade` (`MarginNotionalRejected` or
upstream oracle errors). `RebalanceCrank`'s "needs help" gate also
calls it for the destination market.

### Slab substitution

`verify_percolator_program` (`src/portfolio.rs::verify_percolator_program`)
rejects any non-canonical executable. `engine.is_used(idx)` rejects
substituting an uninitialised slot. Defense 1 cross-validates that
every supplied slab matches a `pa.enrolled[].market` pubkey, with
`MarginSlabDuplicate` blocking the trivial double-count exploit and
`MarginSlabNotEnrolled` blocking foreign slabs. CRIT-6 closes the
multi-account-same-market hole that would have made the lookup
ambiguous.

### Reentrancy via matcher tail

`percolator-prog::TradeCpi` matcher invocation is the only outbound
CPI inside `Trade`. The engine itself validates `matcher_program`
identity match before invoking, and the matcher signs back via
`lp_pda` `invoke_signed`. The wrapper performs **no state mutation
between Trade CPIs** — there's exactly one CPI per `Trade` call, and
all wrapper state writes happen after the CPI returns or not at all
(`processor::trade` reads `auth_bump` then issues a single
`invoke_signed`). `RefCell` borrows on slab data are released before
each CPI; SBF runtime aliasing checks fail loudly if any caller
violates this.

### Bounty drainage

Bounty is capped at 1 USDC per `RebalanceCrank` (`CRANK_BOUNTY_CAP_UNITS
= 1_000_000`). It only pays when the destination is **genuinely below
IM** (engine `is_above_initial_margin` returns false in the pre-check
gate). After CRIT-1 (`a_data.owner == program_id`) and H-1
(`caller_payout_ata.owner == caller`), the attacker surface collapses
to "earn legitimate cranking bounty by being faster than other
crankers" — an operational cost paid out of the user's idle vault
balance, not an extraction. CRIT-2 mitigation means an empty vault
fails fast (`BountyVaultUnderfunded`), so attackers can't burn user CU
via doomed transactions.

### Multi-account-same-market exploits

Closed by CRIT-6: enrollment rejects any duplicate `market` pubkey
inside `enrolled[]`. Without this, Defense 1's pair-region lookup
(matches by market pubkey alone) would have been ambiguous, and the
attacker could have engineered every Trade to fail with
`WrongMarginAccountCount` (structural DoS).

### Borrow-checker exploits

Every `RefCell` borrow on an `AccountInfo`'s slab data is scoped to a
block that ends before the matching CPI. The SBF runtime additionally
enforces aliasing at every `invoke` / `invoke_signed`, so any
double-mutable-borrow shape is caught at boundary time. The Trade
handler is the most complex case: it holds `Ref<&mut [u8]>` for the
target slab + every other-market slab during the aggregate-IM check,
then drops them before the TradeCpi `invoke_signed`. This pattern is
mirrored at `src/portfolio.rs::processor::trade` (lines around the
pair-region walk) and is verified by `cargo test` against the live SBF
runtime.

## Upgrade authority

| Phase | Authority | Rationale |
|---|---|---|
| Initial 90 days | 2-of-3 Squads V4 multisig | Standard hot-fix window for any post-launch security or compatibility issue surfaced by audit follow-up or upstream engine drift. |
| Post-90 days | Revoke (immutable program) | No further upgrade authority required once the audit window has closed; eliminates the supply-chain attack surface entirely. |

Engine schema drift is the one ongoing source of forced upgrades —
each wave that touches RiskEngine layout requires re-pin + re-test, but
the **on-chain wrapper binary** only needs an upgrade when an engine
field used by Defense 1's pair-region path changes shape (see
`## Engine-coupled symbols`). The 8-wave plan currently identifies
Waves 1/4/5/6 as schema-affecting; the rest are wrapper-binary-safe.

### Account-data migration policy (`VERSION` bump)

Bumping `crate::constants::VERSION` immediately bricks every existing
account because `check_portfolio_account` returns `BadVersion` for
`pa.version != VERSION`. Before bumping:

1. Reserve a new instruction tag (e.g., `MigrateV1ToV2`) BEFORE
   shipping the bump, so SDKs ship the migration path in lockstep with
   the new binary.
2. The migration handler accepts an account with `version == 1`,
   translates the layout in-place (respecting Pod alignment), then
   writes the new `VERSION`.
3. Keep the `version == 1` accepting path for at least one full release
   cycle so users have time to migrate.
4. Bump only after the migration path is on mainnet and the SDK has
   shipped a default-migrate flow.

This policy lives at `src/portfolio.rs::constants::VERSION` and is
load-bearing — do not skip steps 1-3.

### Revoking upgrade authority (post-90-days)

```sh
# From the Squads multisig (propose → approve → execute):
solana program set-upgrade-authority \
  <PROGRAM_ID> \
  --new-upgrade-authority null \
  --upgrade-authority <SQUADS_MULTISIG_PDA> \
  --skip-new-upgrade-authority-signer-check
```

After execution, the program is permanently immutable. Verify with
`solana program show <PROGRAM_ID>` — the `Upgrade Authority` field
should read `none`.

## Engine-coupled symbols

The wrapper imports engine types and helpers as a stability strategy:
rather than re-implementing engine math (and risking silent divergence
when the engine evolves), we depend on the engine crate as a **type
source** and let `cargo build` fail loudly on schema drift. The eight
high-risk couplings are:

| Wrapper site | Engine symbol | Source file:line | Sync risk |
|---|---|---|---|
| `src/margin.rs:48` | `wide_math::{mul_div_ceil_u128, mul_div_floor_u128}` | `~/percolator/src/wide_math.rs` | LOW (numerical helpers, ABI-stable) |
| `src/margin.rs:49` | `Account`, `RiskEngine`, `POS_SCALE` | `~/percolator/src/percolator.rs` | HIGH (schema drift breaks `zc::engine_ref` decode + every field access below) |
| `src/margin.rs:150` | `percolator_prog::zc::engine_ref` | `~/percolator-prog/src/percolator.rs:1305` | HIGH (slab decode entry point) |
| `src/margin.rs:167` | `engine.account_equity_init_raw(account, idx)` | `~/percolator/src/percolator.rs:4617` | MEDIUM (H-2 fix; mirrored at the call site — re-pin and re-verify on each sync wave) |
| `src/margin.rs:206-207` | `engine.params.initial_margin_bps`, `min_nonzero_im_req` | `~/percolator/src/percolator.rs` (`RiskParams` struct) | MEDIUM (field name changes are loud; semantic changes need re-port) |
| `src/pyth.rs:44` | `percolator_prog::state::read_config` | `~/percolator-prog/src/percolator.rs:3433` | HIGH (MarketConfig layout drives oracle policy) |
| `src/pyth.rs:45` | `percolator_prog::oracle::read_pyth_price_e6` | `~/percolator-prog/src/percolator.rs:3640` | HIGH (oracle freshness/conf policy must stay aligned with engine TradeCpi) |
| `src/cpi.rs:261-264` | `PERCOLATOR_PROGRAM_ID` constant (`ESa89R5…edv`) | `~/percolator-prog/src/percolator.rs:40` (`declare_id!`) | LOW (program ID rotation is a rare, deliberate event; mismatch fails closed) |

Eight high-risk couplings, all documented at the call site. This table
is the single index — the full sync-wave migration matrix belongs in
the engine-sync runbook
(`~/wrapper-engine-deep-audit/FULL_SYNC_PLAN.md`).

## Deploy checklist

Run from top to bottom. Each step builds on the previous; do not skip.

- [ ] **Grind a real program ID**: `solana-keygen grind --starts-with Perco:1`
      (or your chosen prefix). Save the keypair somewhere safe.
- [ ] **Update `declare_id!`** at `src/portfolio.rs:26` with the new
      pubkey. Commit the change.
- [ ] **Update `PERCOLATOR_PROGRAM_ID`** in `src/cpi.rs:261-264` if the
      engine repo has rotated. Cross-check against
      `~/percolator-prog/src/percolator.rs:40`.
- [ ] **Rebuild**: `cargo build-sbf -- --locked`. Confirm
      `target/deploy/percolator_portfolio-keypair.json` ID matches the
      `declare_id!` value (the `.so` carries the ID).
- [ ] **Verify SLAB_LEN** in `tests/common/integration_env.rs` matches
      the current `--features small` of `percolator-prog`. Drift here
      is a silent test corruption — the schema-pinned constant must
      match the binary you load in tests.
- [ ] **Run the full pre-deploy suite**: `cargo test --locked` (136
      integration tests must pass), `cargo kani --features kani` (37
      proofs must verify), `cargo fmt --check`, `cargo clippy
      --all-targets -- -D warnings`.
- [ ] **Confirm upgrade authority is a Squads V4 multisig** with at
      least 2-of-3 threshold. Solo upgrade-authority keys are not
      acceptable for an audit-gated launch.
- [ ] **Deploy to devnet first**:
      `solana program deploy --program-id <KEYPAIR> --upgrade-authority <SQUADS_PDA> target/deploy/percolator_portfolio.so`.
      Sanity-check end-to-end: `InitPortfolio → InitVault → Deposit
      → EnrollMarketAndInit → Trade → RebalanceCrank → Withdraw →
      ClosePortfolio`.
- [ ] **External audit gate**: do not proceed to mainnet without a
      signed external audit (auditor TBD; record the firm + commit
      SHA in this section before launch).
- [ ] **Mainnet deploy via Squads-approved upgrade**: propose →
      approve → execute the deploy transaction through Squads. Verify
      `solana program show <PROGRAM_ID>` matches the expected
      upgrade authority.
- [ ] **Monitor first N portfolios**. Tail logs for `Custom(*)`
      `ProgramError` codes; map them via
      `src/portfolio.rs::errors::PortfolioError` (each variant has a
      stable `u32` discriminant). Page on any unexpected variant.
- [ ] **Schedule upgrade-authority revocation** for T+90 days from
      mainnet deploy. Add the calendar entry pointing at the Squads
      proposal template (see "Upgrade authority" section).

## License

Apache-2.0. See [`LICENSE`](./LICENSE).
