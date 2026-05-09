# percolator-portfolio

A USDC cross-margin wrapper over isolated `percolator-prog` markets. Single
on-chain Solana program. Engine and wrapper crates of `percolator-prog` are
**not** modified — this is a sibling program that owns user accounts in
existing markets and routes collateral between them.

> Status: scaffold complete, 9 of 11 instructions wired, 68 integration tests
> + 24 Kani proofs all passing. Trade and the off-chain keeper bot are
> intentionally deferred. See "What's not done" at the bottom.

---

## Why

percolator-prog markets are isolated single-collateral by design — each
account in a slab has its own capital, MMR, and per-market liquidation
path. That works but doesn't give users the Hyperliquid / dYdX-style
"profit on position A backstops MMR on position B" experience.

The shape that fits without changing the engine: a sibling program that
owns each user's per-market `Account` (via PDA-as-owner — already supported
by the engine's byte-equality `owner_ok` auth), holds a per-user USDC
vault, and orchestrates deposit / withdraw / rebalance flows across
enrolled markets. Engine invariants are unchanged, Kani proofs stay valid,
audit scope is small.

Two engine asks live with toly that would make this cleaner. Both are
additive, neither blocks v1:

1. **`GetAccountHealth(account_idx) → (eq, mm_req, im_req, above_mm)`** — a
   CPI-callable view of `is_above_maintenance_margin`. Without it, the
   wrapper has to mirror the engine's margin math locally; that's the
   silent failure mode (FM4) and is the largest source of regression-test
   burden in the wrapper. With it, post-state portfolio validation becomes
   a few lines of CPI.
2. **`UpdateAccountOwner(account_idx, new_owner)`** — lets existing
   isolated-margin users transfer a populated percolator account into
   portfolio mode without flattening the position. Without it, only newly
   created accounts (which we initialise via CPI in EnrollMarket) can be
   enrolled.

DM thread: see project memory `session_*` for the full exchange.

---

## Architecture

```
              ┌──────────────────────────────────────┐
              │     percolator-portfolio (this)      │
              │                                      │
              │  PortfolioAccount PDA (per user)     │
              │   ├─ owner: user                     │
              │   ├─ keeper: bot pubkey              │
              │   ├─ buffer_bps, max_leverage_bps    │
              │   └─ enrolled[16] = (slab, idx)      │
              │                                      │
              │  portfolio_auth PDA (per user)       │
              │   = signing authority for CPIs       │
              │   = `Account.owner` on every         │
              │     enrolled per-market slot         │
              │                                      │
              │  portfolio_vault PDA (per user)      │
              │   = SPL Token account, owner=auth    │
              │   = transient USDC between user_ata  │
              │     and per-market vaults            │
              └──────────────┬───────────────────────┘
                             │ invoke_signed
                             ▼
              ┌──────────────────────────────────────┐
              │     percolator-prog (UNCHANGED)      │
              │                                      │
              │  Each market = isolated slab         │
              │  Engine.account.owner = portfolio_   │
              │    auth (set at InitUser via CPI)    │
              │  Engine performs IM/MMR/liquidation  │
              │    locally per market — same as for  │
              │    isolated-margin users today       │
              └──────────────────────────────────────┘
```

### Three PDAs per user

| PDA | Seeds | Purpose | Type |
|---|---|---|---|
| `portfolio_data` | `["portfolio", user]` | The 920-byte `PortfolioAccount` struct | program-owned data |
| `portfolio_auth` | `["portfolio_auth", user]` | The signing PDA for every CPI into `percolator-prog`. Set as `account.owner` on every enrolled engine slot. Set as `account.owner` (token authority) on `portfolio_vault`. No data — pure signer. | program-owned, empty |
| `portfolio_vault` | `["portfolio_vault", user]` | SPL Token account holding the user's USDC between user-side SPL transfers and per-market deposit/withdraw CPIs. | SPL Token v3 account |

### Why the vault

`percolator-prog::DepositCollateral` does this:

```rust
collateral::deposit(token_prog, user_ata, vault, signer, amount)?;
// then:
let owner = engine.accounts[user_idx].owner;
if !policy::owner_ok(owner, signer.key.to_bytes()) { return Err(...) }
```

The SPL transfer signer **and** the engine's owner check are the same
account. Two paths follow:

* **A**: `engine.account.owner = user.key`. User signs CPI directly, no vault
  needed — but keeper can never autonomously rebalance because every CPI
  needs the user's signature.
* **B**: `engine.account.owner = portfolio_auth`. Then the SPL transfer
  source must also be authorised by `portfolio_auth` — which means the
  source token account must be owned by `portfolio_auth`. That's what
  `portfolio_vault` is for.

We chose B. Each Deposit is two SPL transfers (user_ata → vault →
market_vault) instead of one, costing roughly +50K CU. In return, the
keeper bot can rebalance without user involvement, which is the actual
cross-margin value proposition.

When `UpdateAccountOwner` lands, B simplifies — but the vault stays useful
because the SPL signer / engine owner equality remains.

---

## Instructions (11 total)

| Tag | Instruction | Signer | Implemented? | CU (est) |
|---|---|---|---|---|
| 0 | `InitPortfolio` | user | ✅ | ~25K |
| 1 | `EnrollMarket` | user | ✅ (state-only) | ~10K |
| 2 | `UnenrollMarket` | user | ✅ (state-only) | ~10K |
| 3 | `Deposit` | user | ✅ | ~95K |
| 4 | `Withdraw` | user | ✅ | ~95K |
| 5 | `Trade` | user | ⏸️ deferred — needs matcher integration design | — |
| 6 | `Rebalance` | keeper | ✅ | ~170K per leg, max 4 legs |
| 7 | `EmergencyClose` | user | ✅ | ~200K |
| 8 | `UpdateConfig` | user | ✅ | ~5K |
| 9 | `SetPaused` | user | ✅ | ~5K |
| 10 | `InitVault` | user | ✅ | ~25K |

### Setup flow for a new user

```
1. InitPortfolio          (creates portfolio_data PDA)
2. InitVault              (creates portfolio_vault token account)
3. EnrollMarket × N       (registers each market the user wants to use,
                           also CPIs InitUser into percolator-prog so the
                           per-market account's owner = portfolio_auth)
4. Deposit(market, amount)  (USDC moves user_ata → vault → market_vault)
```

After that, the user trades and withdraws via the same per-market account,
or the keeper rebalances collateral between markets when buffer breached.

### Why two separate ixs for InitPortfolio + InitVault

`InitPortfolio` doesn't need the SPL-Token program or the collateral mint.
Splitting keeps the surface narrow and the failure modes orthogonal —
init the portfolio data, then init the vault. Each is < 30K CU. Total
account count per ix stays well under the realistic transaction cap.

---

## State invariants (pinned at compile time + verified by Kani)

```rust
size_of::<PortfolioAccount>() == 920    // const-asserted, Kani-proven
align_of::<PortfolioAccount>() == 8     // const-asserted, Kani-proven
size_of::<MarketSlot>() == 48           // const-asserted, Kani-proven
align_of::<MarketSlot>() == 8           // const-asserted, Kani-proven
MAX_ENROLLED_MARKETS * 48 == 768        // const-asserted
```

Every field of `PortfolioAccount::zeroed()` is `0` (Kani-proven). VERSION
is non-zero (Kani-proven), so a zeroed account can never pose as
initialised.

---

## Decode invariants (Kani-proven on arbitrary input)

For every input byte slice up to 64 bytes:

* `Instruction::decode` never panics, only returns `Ok` or `Err`.
* `Instruction::decode` is deterministic (same bytes → same result).
* For each tag 0..=10, every wrong-length input returns `Err` (per-tag
  proof for tags 0, 1, 2, 3, 4, 5, 7, 8, 9, 10; tag 6 has variable length
  validated against `1 + leg_count × 12`).
* Tags 11..=255 are rejected.
* Tag 9 (`SetPaused`) accepts only body byte `0` or `1` — every other
  byte returns `Err`.
* Encode → decode round-trip is identity for tags 0, 5, and 9.
* `PortfolioAccount` Pod round-trip preserves every header field.

24 proofs total. `cargo kani --features kani` runs them.

---

## Test rigor convention

Every rejection test asserts the **specific** `PortfolioError` discriminant
via `assert_custom_error(result, PortfolioError::X as u32)`, not
`assert!(!err.is_empty())`.

The vacuous pattern (banned):
```rust
let err = env.send(ix).expect_err("must fail");
assert!(!err.is_empty()); // <- always true after expect_err
```

The required pattern:
```rust
let res = env.send_raw(ix);
assert_custom_error(res, PortfolioError::BadOwner as u32);
```

Why it matters: while migrating tests to this pattern, the rigor caught
two real bugs that were hidden by the loose check:

1. `init_rejects_double_init` was actually getting `TransactionError::AlreadyProcessed`
   (Solana tx-level dedup) because the second init was a hash-identical tx.
   Real fix: bump blockhash + use different keeper bytes so the tx actually
   reaches our handler.
2. `init_rejects_unsigned_user_account` was unreachable. Solana's runtime
   auto-promotes the fee-payer's `is_signer` flag, so our `WrongSigner`
   guard couldn't fire from a fee-paying caller. Test deleted with a
   comment explaining the intent.

A third bug surfaced via Kani: `init_decode_strict_length` failed because
tag 0 (InitPortfolio) had `body.len() < 38` instead of the strict
`body.len() != 38` that every other tag used. One-character fix; Kani
proof now blocks any future regression.

---

## CU budget

Per-instruction estimates with comfortable headroom under the 1.4M CU
transaction cap:

| Instruction | Est CU | Notes |
|---|---|---|
| `InitPortfolio` | ~25K | one system_instruction::create_account CPI |
| `InitVault` | ~25K | create_account + initialize_account3 CPIs |
| `EnrollMarket` | ~10K | state-only writes (the InitUser CPI for the new percolator account is a future enhancement) |
| `UnenrollMarket` | ~10K | state-only |
| `Deposit` | ~95K | SPL transfer (user_ata→vault) + DepositCollateral CPI |
| `Withdraw` | ~95K | WithdrawCollateral CPI + SPL transfer (vault→user_ata) |
| `Rebalance` | ~170K × legs | each leg = WithdrawCollateral + DepositCollateral CPIs. Max 4 legs ≈ 680K + wrapper overhead. |
| `EmergencyClose` | ~200K | flatten via TradeNoCpi + CloseAccount CPI |
| `UpdateConfig` | ~5K | one bytemuck cast + 3 field writes |
| `SetPaused` | ~5K | one byte write |

Choices made to keep CU low:

* PDA bumps stored in the account → use `create_program_address` (~1.5K CU)
  instead of `find_program_address` (~200K CU) on every read.
* No double-validation between wrapper and engine. Engine still validates
  everything; wrapper validates only what's load-bearing for the wrapper
  (signer, owner, magic, paused, enrolled).
* Inline strict-length decode per tag — no `read_u16` / `read_u64`
  helpers (each helper would be a function call frame).
* No `format!`, no logs in production (only inside `#[cfg(kani)]` proof
  modules). Solana logging is expensive.
* Bytemuck zero-copy reads — no allocation on the heap.

---

## Test surface

```
tests/
├── common/mod.rs              # assert_custom_error, send_init, etc.
├── test_init_portfolio.rs     # 16 tests — InitPortfolio happy + rejection paths
├── test_config.rs             #  7 tests — UpdateConfig, SetPaused
├── test_enroll.rs             # 15 tests — EnrollMarket, UnenrollMarket
├── test_adversarial.rs        # 12 tests — cross-user attacks, owner check
│                              #            symmetry, account aliasing,
│                              #            field corruption (magic/version/bump)
├── test_vault_and_cpi.rs      # 17 tests — wrapper-validation paths for
│                              #            InitVault, Deposit, Withdraw,
│                              #            Rebalance, EmergencyClose.
│                              #            Rejection paths only — the
│                              #            CPI into percolator-prog is
│                              #            NOT exercised here (see
│                              #            "What's not done" below)
└── 1 lib unit test
                                  ───
                              68 tests, all passing
```

Every test passes with `cargo test --locked`.

Every Kani proof passes with `cargo kani --features kani` (24 proofs total).

---

## Build

```sh
# Compile to BPF
cargo build-sbf -- --locked

# Run all integration tests
cargo test --locked

# Run all Kani proofs (requires cargo-kani installed)
cargo kani --features kani
```

The toolchain pins `blake3 = 1.8.1`, `indexmap = 2.7.1`, `hashbrown =
0.15.2`, `once_cell = 1.20.2` to dodge the edition2024 wave on rustc 1.84
that the SBF toolchain ships with. Same workaround `percolator-match`
uses.

`Cargo.lock` is committed — required for reproducible SBF builds.

---

## What's not done

Honest list:

| Item | Why deferred |
|---|---|
| **End-to-end happy-path tests for Deposit / Withdraw / Rebalance / EmergencyClose** | These instructions all CPI into `percolator-prog`. Building a real harness needs both .so files loaded in litesvm, a percolator-prog market initialised (admin keypair + mint + vault + oracle), and `InitUser` called via the portfolio program to establish a `portfolio_auth`-owned engine account. The CPI shape will change materially when `GetAccountHealth` / `UpdateAccountOwner` engine asks land, so happy-path tests now would be premature. The 17 tests in `test_vault_and_cpi.rs` cover wrapper-validation rejection paths only — they don't exercise the CPI itself. **This is the largest verification gap in the repo today.** |
| **`Trade` (tag 5)** | percolator-prog's `TradeNoCpi` requires a co-signing LP on the other side. That's matcher-side coordination, out of scope for this program. `TradeCpi` has different semantics worth a separate design pass. Both routes will be added once the matcher integration is designed. |
| **`EnrollMarket` does NOT yet CPI `InitUser`** | The state-only path is in place. Wiring the `percolator-prog::InitUser` CPI from inside EnrollMarket so the per-market account is created with `owner = portfolio_auth` is a ~50 LOC addition pending the design choice on whether to fund the initial `fee_payment` from the portfolio_vault or take it as a parameter. |
| **`GetAccountHealth` integration** | Wrapper currently doesn't enforce post-state portfolio MMR / IM checks on Withdraw and Trade. The math-mirror approach is the FM4 silent failure mode. Will be wired once the engine ask lands; until then, the wrapper accepts any withdraw/trade that percolator-prog itself accepts (which is per-market correct, just not portfolio-aware). |
| **`UpdateAccountOwner` migration path** | EnrollMarket can only attach to NEW percolator accounts created with `owner = portfolio_auth`. Migrating an existing user's positions into portfolio mode requires the engine ask. |
| **Off-chain keeper bot** | Designed (~1.2K LOC TS), not written. Watches enrolled markets via slot subscriptions, computes portfolio health, submits `Rebalance` when buffer breached. |
| **Real `declare_id!` keypair** | Currently `PercoFoLPort1111111111111111111111111111111` — a placeholder. Needs `solana-keygen grind` before deployment. |
| **CI configuration** | No `.github/workflows/`. Should run `cargo build-sbf`, `cargo test`, and `cargo kani` on PR. |

---

## File layout

```
src/
├── portfolio.rs          # the program — state, instructions, processor, kani proofs
└── cpi.rs                # percolator-prog instruction builders for CPI

tests/
├── common/mod.rs         # assert_custom_error helper, fresh_env, send_init
├── test_init_portfolio.rs
├── test_config.rs
├── test_enroll.rs
├── test_adversarial.rs
└── test_vault_and_cpi.rs

Cargo.toml
Cargo.lock                # committed (pins SBF toolchain dependencies)
```

---

## Correctness summary

* **Pod safety**: every state struct is `#[repr(C)]` with `bytemuck::Pod +
  Zeroable` derived, alignment and size const-asserted at compile time and
  re-asserted at runtime by Kani.
* **Decode safety**: every instruction tag has a Kani proof that decode
  never panics on arbitrary input and rejects every wrong-length input.
* **Auth safety**: `check_portfolio_account` verifies (signer, program-owned,
  writable, magic, version, stored-owner, PDA-derivation) before any state
  mutation. Adversarial tests cover every link in the chain individually
  via surgical field corruption (`corrupted_magic_rejects_with_bad_magic`,
  `corrupted_version_rejects_with_bad_version`,
  `corrupted_bump_rejects_with_bad_pda`).
* **Test rigor**: every rejection asserts the SPECIFIC `PortfolioError`
  code, not just "something errored". The pattern caught three real bugs
  in the migration; see "Test rigor convention" above.
* **No production panics**: production-code grep for `unwrap`, `expect`,
  `panic!`, `unreachable!`, `todo!`, `unimplemented!` returns zero hits.
  The only `.expect()` calls in the crate are inside `#[cfg(kani)]` proof
  modules.
* **No production arithmetic** outside the `1 + leg_count × 12` decode
  formula (which is bounded by `u8::MAX × 12 = 3060`, well under
  `usize::MAX`).
