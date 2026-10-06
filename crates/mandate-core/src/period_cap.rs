//! Period-cap rule: at most `limit` units of one token may be transferred per
//! fixed period of `period_secs` seconds.
//!
//! Periods are aligned to the first spend: the first transfer opens a period
//! at its timestamp, and later periods start at
//! `period_start + k * period_secs`. Once `now >= period_start + period_secs`
//! the counter resets.

use crate::{Call, ConfigError, Decision, FnList, Reason};

/// The function whose third argument, `amount: i128`, is counted.
pub const TRANSFER_FN: &str = "transfer";

/// A validated period-cap configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeriodCapConfig {
    token: [u8; 32],
    limit: i128,
    period_secs: u64,
    allowed_functions: FnList,
}

impl PeriodCapConfig {
    /// Builds a validated configuration.
    ///
    /// `allowed_functions` lists functions on the token, other than
    /// `transfer`, that may be called without being counted (for example
    /// `balance`). Every other function on the token is denied.
    pub fn new(
        token: [u8; 32],
        limit: i128,
        period_secs: u64,
        allowed_functions: &[&str],
    ) -> Result<Self, ConfigError> {
        Self::with_fn_list(token, limit, period_secs, FnList::new(allowed_functions)?)
    }

    /// Like [`Self::new`], for callers that build the function list with
    /// [`FnList::push`].
    pub fn with_fn_list(
        token: [u8; 32],
        limit: i128,
        period_secs: u64,
        allowed_functions: FnList,
    ) -> Result<Self, ConfigError> {
        if limit <= 0 {
            return Err(ConfigError::NonPositiveLimit);
        }
        if period_secs == 0 {
            return Err(ConfigError::ZeroPeriod);
        }
        Ok(Self {
            token,
            limit,
            period_secs,
            allowed_functions,
        })
    }

    pub fn token(&self) -> [u8; 32] {
        self.token
    }

    pub fn limit(&self) -> i128 {
        self.limit
    }

    pub fn period_secs(&self) -> u64 {
        self.period_secs
    }

    pub fn allowed_functions(&self) -> &FnList {
        &self.allowed_functions
    }
}

/// Spending recorded for the current period.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeriodCapState {
    pub period_start: u64,
    pub spent: i128,
}

/// Returns the state as of `now`: the stored state if its period is still
/// open, or a fresh period containing `now` otherwise. `None` means nothing
/// has been spent yet, in which case a period starts at `now`.
///
/// If `now` is earlier than the stored period start, the stored period is
/// kept, which is the conservative choice.
pub fn current_state(
    config: &PeriodCapConfig,
    state: &Option<PeriodCapState>,
    now: u64,
) -> PeriodCapState {
    let Some(state) = *state else {
        return PeriodCapState {
            period_start: now,
            spent: 0,
        };
    };
    match state.period_start.checked_add(config.period_secs) {
        // When the boundary does not fit in a u64, the period never ends.
        Some(period_end) if now >= period_end => {
            // now >= period_end > period_start, so neither operation can
            // overflow: elapsed periods * period_secs <= now - period_start.
            let elapsed = (now - state.period_start) / config.period_secs;
            PeriodCapState {
                period_start: state.period_start + elapsed * config.period_secs,
                spent: 0,
            }
        }
        _ => state,
    }
}

/// How much can still be transferred in the period containing `now`.
pub fn remaining(config: &PeriodCapConfig, state: &Option<PeriodCapState>, now: u64) -> i128 {
    let current = current_state(config, state, now);
    config.limit.checked_sub(current.spent).unwrap_or(0).max(0)
}

/// Evaluates a call against the cap.
///
/// * Calls to other contracts are allowed and leave the state unchanged.
/// * `transfer` on the token is allowed if the amount is present,
///   non-negative, and fits under the limit; the new state records it.
/// * Functions in `allowed_functions` are allowed and not counted.
/// * Any other function on the token is denied.
///
/// A denied call never changes the state.
pub fn evaluate(
    config: &PeriodCapConfig,
    state: &Option<PeriodCapState>,
    call: &Call,
) -> (Decision, Option<PeriodCapState>) {
    let deny = |reason| (Decision::Deny(reason), *state);

    if call.contract != config.token {
        return (Decision::Allow, *state);
    }
    if call.function != TRANSFER_FN {
        return if config.allowed_functions.contains(call.function) {
            (Decision::Allow, *state)
        } else {
            deny(Reason::FunctionNotAllowed)
        };
    }
    let amount = match call.amount {
        Some(amount) if amount >= 0 => amount,
        _ => return deny(Reason::InvalidAmount),
    };
    let current = current_state(config, state, call.timestamp);
    let Some(spent) = current.spent.checked_add(amount) else {
        return deny(Reason::Overflow);
    };
    if spent > config.limit {
        return deny(Reason::CapExceeded);
    }
    (
        Decision::Allow,
        Some(PeriodCapState {
            period_start: current.period_start,
            spent,
        }),
    )
}

/// Contract-creation contexts move no tokens: they are allowed and leave the
/// state unchanged.
pub fn evaluate_create_contract(
    _config: &PeriodCapConfig,
    state: &Option<PeriodCapState>,
) -> (Decision, Option<PeriodCapState>) {
    (Decision::Allow, *state)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use proptest::prelude::*;

    const USDC: [u8; 32] = [1; 32];
    const OTHER: [u8; 32] = [2; 32];
    const DAY: u64 = 86_400;
    const T0: u64 = 1_704_153_600;

    fn config(limit: i128, period_secs: u64) -> PeriodCapConfig {
        PeriodCapConfig::new(USDC, limit, period_secs, &["balance"]).unwrap()
    }

    fn transfer(amount: i128, timestamp: u64) -> Call<'static> {
        Call {
            contract: USDC,
            function: "transfer",
            amount: Some(amount),
            timestamp,
        }
    }

    #[test]
    fn spends_up_to_the_limit_then_denies() {
        let cfg = config(50, DAY);
        let (d, s) = evaluate(&cfg, &None, &transfer(40, T0));
        assert_eq!(d, Decision::Allow);
        assert_eq!(
            s,
            Some(PeriodCapState {
                period_start: T0,
                spent: 40
            })
        );
        let (d, s) = evaluate(&cfg, &s, &transfer(10, T0 + 60));
        assert_eq!(d, Decision::Allow);
        assert_eq!(s.unwrap().spent, 50);
        let (d, s2) = evaluate(&cfg, &s, &transfer(1, T0 + 120));
        assert_eq!(d, Decision::Deny(Reason::CapExceeded));
        assert_eq!(s2, s, "a denied call must not change state");
    }

    #[test]
    fn resets_exactly_at_the_boundary() {
        let cfg = config(50, DAY);
        let s = Some(PeriodCapState {
            period_start: T0,
            spent: 50,
        });
        assert_eq!(
            evaluate(&cfg, &s, &transfer(1, T0 + DAY - 1)).0,
            Decision::Deny(Reason::CapExceeded)
        );
        let (d, s) = evaluate(&cfg, &s, &transfer(1, T0 + DAY));
        assert_eq!(d, Decision::Allow);
        assert_eq!(
            s,
            Some(PeriodCapState {
                period_start: T0 + DAY,
                spent: 1
            })
        );
    }

    #[test]
    fn skipped_periods_stay_aligned() {
        let cfg = config(50, DAY);
        let s = Some(PeriodCapState {
            period_start: T0,
            spent: 50,
        });
        let (_, s) = evaluate(&cfg, &s, &transfer(5, T0 + 3 * DAY + 7));
        assert_eq!(s.unwrap().period_start, T0 + 3 * DAY);
    }

    #[test]
    fn other_contracts_and_allowed_functions_pass_through() {
        let cfg = config(50, DAY);
        let s = Some(PeriodCapState {
            period_start: T0,
            spent: 50,
        });
        let other = Call {
            contract: OTHER,
            function: "swap",
            amount: Some(1_000),
            timestamp: T0,
        };
        assert_eq!(evaluate(&cfg, &s, &other), (Decision::Allow, s));
        let balance = Call {
            contract: USDC,
            function: "balance",
            amount: None,
            timestamp: T0,
        };
        assert_eq!(evaluate(&cfg, &s, &balance), (Decision::Allow, s));
    }

    #[test]
    fn contract_creation_is_allowed_without_spending() {
        let cfg = config(50, DAY);
        let s = Some(PeriodCapState {
            period_start: T0,
            spent: 50,
        });
        assert_eq!(evaluate_create_contract(&cfg, &s), (Decision::Allow, s));
    }

    #[test]
    fn denies_unlisted_token_functions() {
        let cfg = config(50, DAY);
        let approve = Call {
            contract: USDC,
            function: "approve",
            amount: Some(1),
            timestamp: T0,
        };
        assert_eq!(
            evaluate(&cfg, &None, &approve).0,
            Decision::Deny(Reason::FunctionNotAllowed)
        );
    }

    #[test]
    fn denies_missing_or_negative_amounts() {
        let cfg = config(50, DAY);
        let mut call = transfer(-1, T0);
        assert_eq!(
            evaluate(&cfg, &None, &call).0,
            Decision::Deny(Reason::InvalidAmount)
        );
        call.amount = None;
        assert_eq!(
            evaluate(&cfg, &None, &call).0,
            Decision::Deny(Reason::InvalidAmount)
        );
    }

    #[test]
    fn overflow_is_denied_not_panicked() {
        let cfg = config(i128::MAX, DAY);
        let s = Some(PeriodCapState {
            period_start: T0,
            spent: i128::MAX,
        });
        assert_eq!(
            evaluate(&cfg, &s, &transfer(1, T0)).0,
            Decision::Deny(Reason::Overflow)
        );
    }

    #[test]
    fn boundary_beyond_u64_never_resets() {
        let cfg = config(50, u64::MAX);
        let s = Some(PeriodCapState {
            period_start: 10,
            spent: 50,
        });
        assert_eq!(
            evaluate(&cfg, &s, &transfer(1, u64::MAX)).0,
            Decision::Deny(Reason::CapExceeded)
        );
    }

    #[test]
    fn remaining_tracks_spend_and_reset() {
        let cfg = config(50, DAY);
        assert_eq!(remaining(&cfg, &None, T0), 50);
        let s = Some(PeriodCapState {
            period_start: T0,
            spent: 45,
        });
        assert_eq!(remaining(&cfg, &s, T0 + 1), 5);
        assert_eq!(remaining(&cfg, &s, T0 + DAY), 50);
    }

    #[test]
    fn rejects_invalid_configs() {
        assert_eq!(
            PeriodCapConfig::new(USDC, 0, DAY, &[]),
            Err(ConfigError::NonPositiveLimit)
        );
        assert_eq!(
            PeriodCapConfig::new(USDC, -5, DAY, &[]),
            Err(ConfigError::NonPositiveLimit)
        );
        assert_eq!(
            PeriodCapConfig::new(USDC, 5, 0, &[]),
            Err(ConfigError::ZeroPeriod)
        );
        assert_eq!(
            PeriodCapConfig::new(USDC, 5, 1, &["no way"]),
            Err(ConfigError::InvalidFunctionName)
        );
    }

    fn calls() -> impl Strategy<Value = std::vec::Vec<(i128, u64)>> {
        // (amount, seconds since the previous call)
        proptest::collection::vec((-10i128..200, 0u64..2 * DAY), 1..60)
    }

    proptest! {
        /// Within any single period, the sum of allowed transfers never
        /// exceeds the limit, and the recorded counter never exceeds it either.
        #[test]
        fn cap_is_never_exceeded(
            limit in 1i128..500,
            period in 1u64..3 * DAY,
            seq in calls(),
        ) {
            let cfg = config(limit, period);
            let mut state = None;
            let mut now = T0;
            let mut window_start: Option<u64> = None;
            let mut allowed_in_window: i128 = 0;
            for (amount, gap) in seq {
                now += gap;
                let (decision, next) = evaluate(&cfg, &state, &transfer(amount, now));
                if let Some(s) = next {
                    prop_assert!(s.spent <= limit);
                    prop_assert!(s.spent >= 0);
                }
                if decision.is_allow() {
                    let s = next.unwrap();
                    prop_assert!(s.period_start <= now);
                    prop_assert!(now < s.period_start + period);
                    if window_start != Some(s.period_start) {
                        window_start = Some(s.period_start);
                        allowed_in_window = 0;
                    }
                    allowed_in_window += amount;
                    prop_assert!(allowed_in_window <= limit);
                    prop_assert_eq!(s.spent, allowed_in_window);
                } else {
                    prop_assert_eq!(next, state);
                }
                state = next;
            }
        }

        /// The counter resets exactly when `now` reaches
        /// `period_start + period_secs`, and not one second earlier.
        #[test]
        fn resets_exactly_at_period_boundaries(
            limit in 1i128..1_000,
            period in 1u64..10 * DAY,
            start in 0u64..1u64 << 40,
            periods in 0u64..5,
            spent_frac in 0u32..=100,
        ) {
            let cfg = config(limit, period);
            let spent = limit * spent_frac as i128 / 100;
            let state = Some(PeriodCapState { period_start: start, spent });
            let boundary = start + (periods + 1) * period;

            let before = current_state(&cfg, &state, boundary - 1);
            let at = current_state(&cfg, &state, boundary);
            prop_assert_eq!(at, PeriodCapState { period_start: boundary, spent: 0 });
            if periods == 0 {
                prop_assert_eq!(before, PeriodCapState { period_start: start, spent });
            } else {
                prop_assert_eq!(before.period_start, boundary - period);
                prop_assert_eq!(before.spent, 0);
            }
        }

        /// Arbitrary inputs never panic.
        #[test]
        fn never_panics(
            limit in 1i128..=i128::MAX,
            period in 1u64..=u64::MAX,
            start in any::<u64>(),
            spent in any::<i128>(),
            amount in any::<i128>(),
            now in any::<u64>(),
        ) {
            let cfg = config(limit, period);
            let state = Some(PeriodCapState { period_start: start, spent });
            let _ = evaluate(&cfg, &state, &transfer(amount, now));
            let _ = remaining(&cfg, &state, now);
        }
    }
}
