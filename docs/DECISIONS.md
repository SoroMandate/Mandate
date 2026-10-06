# Design decisions

This file records version pins and design choices. Add a new entry when you change one, and say why.

## Version pins

| Component | Version | Notes |
|---|---|---|
| `soroban-sdk` | `26.1.0` | Matches the version OpenZeppelin `stellar-contracts` v0.7.2 builds against, so the policies and the OZ smart account share one SDK in tests. (28.0.0 was the newest release when checked; upgrading means upgrading OZ too.) |
| OpenZeppelin `stellar-contracts` (`stellar-accounts` crate) | `v0.7.2` | Latest stable tag. v0.8.0 was still at release candidate stage. |
| `stellar-cli` | `27.0.0` | Used to build, optimize, and deploy. |
| Rust wasm target | `wasm32v1-none` | The target current Soroban guidance recommends. |
| `proptest` | `1.x` | Dev-dependency of `mandate-core`. |

## D1: `mandate-core` is `no_std` and allocation-free

The core crate uses fixed-capacity inline types (`FnName`, `FnList`, `AllowlistConfig`) instead of `Vec` and `String`. That way the Soroban contracts can use it without an allocator, and the same code compiles for `wasm32v1-none`, for `wasm-bindgen`, and for native builds.

Capacities:
- Allowlist: 20 entries (`allowlist::MAX_ENTRIES`).
- Function names per list: 10 (`MAX_FN_LIST_LEN`), 32 bytes each, using the Soroban symbol charset `[A-Za-z0-9_]`.
- Time-window hour ranges: 4 (`time_window::MAX_RANGES`).

## D2: Uniform rule signature

Every rule exposes `evaluate(&config, &state, &call) -> (Decision, NewState)`. The stateless rules (allowlist and time window) use `()` as their state. Period cap uses `Option<PeriodCapState>`, where `None` means nothing has been spent yet.

## D3: Contract-creation contexts

`Call` describes only contract invocations, matching the specification. Contract-creation contexts are handled by a separate pure function on each rule that cares about them, so the decision still lives in core:
- `allowlist::evaluate_create_contract`: always `Deny(ContractCreation)`.
- `time_window::evaluate_create_contract`: applies the same time window.
- `period_cap`: contract creation moves no tokens, so the contract allows it without consulting a rule function.

## D4: Validation happens in constructors

Configs can only be built through validating constructors (`AllowlistConfig::from_entries`, `TimeWindowConfig::new`, `PeriodCapConfig::new`), which return `ConfigError`. An invalid config therefore cannot reach `evaluate`. The rules enforced are:
- Allowlist: at most 20 entries, no duplicate contracts, valid function names.
- Time window: a non-empty day mask using only bits 0–6 (Monday is bit 0), 1 to 4 ranges, and each range satisfying `start < 24`, `end <= 24`, and `start < end`. Ranges that wrap past midnight are written as two ranges.
- Period cap: `limit > 0`, `period_secs > 0`, valid names in the allowed-function list.

## D5: Period-cap semantics

- **Fixed, aligned periods.** The first transfer opens a period at its timestamp. When `now >= period_start + period_secs`, the counter resets and the new `period_start` is `period_start + k * period_secs`, the latest boundary not after `now`. Period boundaries are therefore deterministic, and an idle account does not shift them.
- **Overflow.** `spent + amount` uses `checked_add`, and an overflow becomes `Deny(Overflow)`. If `period_start + period_secs` overflows a `u64`, the boundary can never be reached, so the period never resets. That is the conservative result, and it cannot panic.
- **Clock going backwards.** If `now < period_start`, which the simulator allows, the stored period is kept and no reset happens.
- **Amounts.** A `transfer` with a missing or negative amount is denied with `InvalidAmount`.
- **Other functions on the token.** These are denied with `FunctionNotAllowed` unless they appear in the config's `allowed_functions` list. Allowed functions are not counted toward the cap.
- **Other contracts.** Calls to other contracts are allowed, because the period cap only governs its token. Use the allowlist to restrict which contracts can be called at all.
- **Denials.** A denied call never changes the state.

## D6: Weekday computation

UTC weekday = `((timestamp / 86400) + 3) % 7`, with Monday as 0. The Unix epoch fell on a Thursday. No calendar library is needed, and leap seconds do not appear in Unix time.

## D7: Stable reason and error codes

`Reason` and `ConfigError` use `#[repr(u32)]` with fixed discriminants, plus `as_str()` names. The contracts, the CLI, and the simulator all report the same names.

## D8: Denials are reported through typed errors, not events

OpenZeppelin's `Policy::enforce` denies a context by failing the invocation. A failed invocation rolls back every contract event it published, and testing showed these events do not appear in diagnostic output either. A "denied" event emitted just before the panic would therefore never reach anyone, so the policies do not emit one.

The reason for a denial is carried by the policy's `#[contracterror]` code instead. The host records it as a diagnostic `error` event (for example `Error(Contract, #4103)`), and RPC transaction simulation returns that event to wallets. A test checks this for each policy. Install, reconfigure, and uninstall do succeed, and they emit ordinary contract events.

## D9: Storage is keyed by (smart account, context rule ID)

Following OpenZeppelin's own policies, one policy deployment serves many smart accounts. Each account keeps a separate configuration for each context rule it attaches the policy to. Every mutating entry point (`install`, `set_config`, `uninstall`) calls `smart_account.require_auth()`. `enforce` does the same, matching OpenZeppelin, so one account can never read or change another account's state.

The read-only getters therefore take `(smart_account, context_rule_id)`.

## D10: `mandate-soroban` glue crate

Converting `Address` to `[u8; 32]`, `Symbol` to `&str`, and `ContractContext` to `Call`, along with the TTL constants, is needed by all three contracts. That shared code lives in `crates/mandate-soroban`, which contains no policy logic. Only contract (`C...`) addresses can be converted. Anything else is rejected as an invalid configuration, or treated as a denial in `enforce`. The crate uses the `hazmat-address` feature of `soroban-sdk` solely to read a contract address's 32-byte ID.

## D11: Error code ranges

Each policy owns a range of error codes so that errors from several policies on one account can be told apart: allowlist `4100–4199`, time window `4200–4299`, period cap `4300–4399`. OpenZeppelin's policies use `32xx`.

## D12: TTLs

Configuration and state entries live in persistent storage. They are extended to 30 days whenever they are written or read during `enforce`, and only once their remaining TTL drops below 29 days. Each policy contract also extends its own instance TTL on `install`, `set_config`, and `enforce`.

## D13: Dependency pin for tests

`soroban-env-host` 26.1.3 accepts `ed25519-dalek` 3.x, but its testutils only compile against 2.x. `Cargo.lock` pins `ed25519-dalek` to 2.2.0, matching OpenZeppelin's lockfile. Do not run a blanket `cargo update` without checking this pin.
