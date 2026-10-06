//! # Period-cap policy
//!
//! A Mandate policy for OpenZeppelin smart accounts. A signer governed by this
//! policy may transfer at most `limit` units of one token per fixed period of
//! `period_secs` seconds, judged by the ledger timestamp.
//!
//! The amount is read from `transfer(from, to, amount: i128)` calls on the
//! configured token. Other functions on that token are denied unless listed in
//! `allowed_functions`. Calls to other contracts are not this policy's
//! concern and are allowed.
//!
//! Spending is tracked per (smart account, token) in persistent storage, so
//! every context rule that caps the same token draws on one budget. All
//! decisions are made by [`mandate_core::period_cap`].
#![no_std]

use mandate_core::{
    period_cap::{self, PeriodCapConfig, PeriodCapState as CoreState},
    Decision, FnList, Reason, MAX_FN_LIST_LEN,
};
use mandate_soroban::{contract_id, symbol_str, with_call, TTL_EXTEND_TO, TTL_THRESHOLD};
use soroban_sdk::{
    auth::Context, contract, contracterror, contractevent, contractimpl, contracttype,
    panic_with_error, Address, Env, Symbol, TryFromVal, Vec,
};
use stellar_accounts::{
    policies::Policy,
    smart_account::{ContextRule, Signer},
};

/// Installation and configuration parameters.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PeriodCapParams {
    /// The token contract whose transfers are capped.
    pub token: Address,
    /// Maximum amount, in the token's base units, per period. Must be > 0.
    pub limit: i128,
    /// Period length in seconds. Must be > 0.
    pub period_secs: u64,
    /// Up to 10 other functions on the token that may be called without
    /// being counted, such as `balance`. Every other function is denied.
    pub allowed_functions: Vec<Symbol>,
}

/// Spending recorded for the current period.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PeriodCapState {
    pub period_start: u64,
    pub spent: i128,
}

#[contracttype]
enum StorageKey {
    /// Configuration for a (smart account, context rule ID) pair.
    Config(Address, u32),
    /// Spending state for a (smart account, token) pair.
    State(Address, Address),
}

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u32)]
pub enum PeriodCapError {
    /// No configuration exists for this smart account and context rule.
    NotInstalled = 4300,
    /// A configuration already exists for this smart account and context rule.
    AlreadyInstalled = 4301,
    /// The configuration is invalid: a non-positive limit, a zero period, a
    /// non-contract token address, or a bad allowed-function list.
    InvalidConfig = 4302,
    /// The transfer would push spending in the current period over the limit.
    CapExceeded = 4303,
    /// The call targets the token with a function that is not allowed.
    FunctionNotAllowed = 4304,
    /// The transfer amount is missing, malformed, or negative.
    InvalidAmount = 4305,
    /// Adding the transfer to the period's spending would overflow.
    Overflow = 4306,
    /// The authorization context could not be read.
    InvalidContext = 4307,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct PeriodCapInstalled {
    #[topic]
    pub smart_account: Address,
    pub context_rule_id: u32,
    pub params: PeriodCapParams,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct PeriodCapConfigured {
    #[topic]
    pub smart_account: Address,
    pub context_rule_id: u32,
    pub params: PeriodCapParams,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct PeriodCapUninstalled {
    #[topic]
    pub smart_account: Address,
    pub context_rule_id: u32,
}

/// Emitted when an allowed transfer is recorded against the cap.
#[contractevent]
#[derive(Clone, Debug)]
pub struct PeriodCapSpent {
    #[topic]
    pub smart_account: Address,
    #[topic]
    pub token: Address,
    pub context_rule_id: u32,
    pub period_start: u64,
    pub spent: i128,
}

#[contract]
pub struct PeriodCapPolicy;

#[contractimpl]
impl Policy for PeriodCapPolicy {
    type AccountParams = PeriodCapParams;

    /// Denies a context that breaks the cap by failing with a
    /// [`PeriodCapError`]. An allowed transfer is recorded.
    fn enforce(
        e: &Env,
        context: Context,
        _authenticated_signers: Vec<Signer>,
        context_rule: ContextRule,
        smart_account: Address,
    ) {
        smart_account.require_auth();
        extend_instance(e);

        let params = load_config(e, &smart_account, context_rule.id);
        let config = to_core(e, &params).unwrap_or_else(|err| panic_with_error!(e, err));
        let state_key = StorageKey::State(smart_account.clone(), params.token.clone());
        let state = load_state(e, &state_key);

        let (decision, new_state) = match &context {
            Context::Contract(call) => {
                // Read the third argument as the amount. mandate-core only
                // uses it for `transfer` calls on the token, and treats a
                // missing amount as invalid.
                let amount = call
                    .args
                    .get(2)
                    .and_then(|v| i128::try_from_val(e, &v).ok());
                with_call(e, call, amount, |c| {
                    period_cap::evaluate(&config, &state, c)
                })
                .unwrap_or_else(|| panic_with_error!(e, PeriodCapError::InvalidContext))
            }
            Context::CreateContractHostFn(_) | Context::CreateContractWithCtorHostFn(_) => {
                period_cap::evaluate_create_contract(&config, &state)
            }
        };

        // Denying fails the invocation, which rolls back any event it
        // published, so the typed error is the report. See docs/DECISIONS.md
        // (D8).
        if let Decision::Deny(reason) = decision {
            panic_with_error!(e, error_for(reason));
        }
        if new_state != state {
            if let Some(s) = new_state {
                save_state(e, &state_key, &s);
                PeriodCapSpent {
                    smart_account,
                    token: params.token,
                    context_rule_id: context_rule.id,
                    period_start: s.period_start,
                    spent: s.spent,
                }
                .publish(e);
            }
        }
    }

    fn install(
        e: &Env,
        install_params: Self::AccountParams,
        context_rule: ContextRule,
        smart_account: Address,
    ) {
        smart_account.require_auth();
        extend_instance(e);

        let key = StorageKey::Config(smart_account.clone(), context_rule.id);
        if e.storage().persistent().has(&key) {
            panic_with_error!(e, PeriodCapError::AlreadyInstalled);
        }
        if let Err(err) = to_core(e, &install_params) {
            panic_with_error!(e, err);
        }
        save_config(e, &key, &install_params);

        PeriodCapInstalled {
            smart_account,
            context_rule_id: context_rule.id,
            params: install_params,
        }
        .publish(e);
    }

    /// Removes the configuration. The spending state is kept, so that
    /// uninstalling and reinstalling cannot reset a period's spending; it
    /// expires with its TTL.
    fn uninstall(e: &Env, context_rule: ContextRule, smart_account: Address) {
        smart_account.require_auth();

        let key = StorageKey::Config(smart_account.clone(), context_rule.id);
        if !e.storage().persistent().has(&key) {
            panic_with_error!(e, PeriodCapError::NotInstalled);
        }
        e.storage().persistent().remove(&key);

        PeriodCapUninstalled {
            smart_account,
            context_rule_id: context_rule.id,
        }
        .publish(e);
    }
}

#[contractimpl]
impl PeriodCapPolicy {
    /// Replaces the configuration for an installed context rule. Only the
    /// smart account itself can call this. Spending recorded so far is kept.
    pub fn set_config(
        e: Env,
        smart_account: Address,
        context_rule_id: u32,
        params: PeriodCapParams,
    ) {
        smart_account.require_auth();
        extend_instance(&e);

        let key = StorageKey::Config(smart_account.clone(), context_rule_id);
        if !e.storage().persistent().has(&key) {
            panic_with_error!(&e, PeriodCapError::NotInstalled);
        }
        if let Err(err) = to_core(&e, &params) {
            panic_with_error!(&e, err);
        }
        save_config(&e, &key, &params);

        PeriodCapConfigured {
            smart_account,
            context_rule_id,
            params,
        }
        .publish(&e);
    }

    /// Returns the configuration for a smart account's context rule.
    pub fn get_config(e: Env, smart_account: Address, context_rule_id: u32) -> PeriodCapParams {
        load_config(&e, &smart_account, context_rule_id)
    }

    /// Returns the stored spending state for a smart account and token, or
    /// `None` if nothing has been spent. The stored period may already have
    /// ended; use [`Self::remaining`] for the live figure.
    pub fn get_state(e: Env, smart_account: Address, token: Address) -> Option<PeriodCapState> {
        load_state(&e, &StorageKey::State(smart_account, token)).map(|s| PeriodCapState {
            period_start: s.period_start,
            spent: s.spent,
        })
    }

    /// How much can still be transferred under a context rule's cap in the
    /// period containing the current ledger time.
    pub fn remaining(e: Env, smart_account: Address, context_rule_id: u32) -> i128 {
        let params = load_config(&e, &smart_account, context_rule_id);
        let config = to_core(&e, &params).unwrap_or_else(|err| panic_with_error!(&e, err));
        let state = load_state(&e, &StorageKey::State(smart_account, params.token));
        period_cap::remaining(&config, &state, e.ledger().timestamp())
    }
}

fn load_config(e: &Env, smart_account: &Address, context_rule_id: u32) -> PeriodCapParams {
    let key = StorageKey::Config(smart_account.clone(), context_rule_id);
    let params = e
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| panic_with_error!(e, PeriodCapError::NotInstalled));
    e.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
    params
}

fn save_config(e: &Env, key: &StorageKey, params: &PeriodCapParams) {
    e.storage().persistent().set(key, params);
    e.storage()
        .persistent()
        .extend_ttl(key, TTL_THRESHOLD, TTL_EXTEND_TO);
}

fn load_state(e: &Env, key: &StorageKey) -> Option<CoreState> {
    e.storage()
        .persistent()
        .get::<_, PeriodCapState>(key)
        .map(|s| CoreState {
            period_start: s.period_start,
            spent: s.spent,
        })
}

fn save_state(e: &Env, key: &StorageKey, state: &CoreState) {
    let stored = PeriodCapState {
        period_start: state.period_start,
        spent: state.spent,
    };
    e.storage().persistent().set(key, &stored);
    e.storage()
        .persistent()
        .extend_ttl(key, TTL_THRESHOLD, TTL_EXTEND_TO);
}

fn extend_instance(e: &Env) {
    e.storage()
        .instance()
        .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
}

/// Converts and validates parameters with `mandate-core`.
fn to_core(e: &Env, params: &PeriodCapParams) -> Result<PeriodCapConfig, PeriodCapError> {
    const INVALID: PeriodCapError = PeriodCapError::InvalidConfig;
    let token = contract_id(&params.token).ok_or(INVALID)?;
    // Check the length before iterating so oversized input is rejected cheaply.
    if params.allowed_functions.len() as usize > MAX_FN_LIST_LEN {
        return Err(INVALID);
    }
    let mut functions = FnList::empty();
    for function in params.allowed_functions.iter() {
        let name = symbol_str(e, &function).ok_or(INVALID)?;
        functions.push(name.as_ref()).map_err(|_| INVALID)?;
    }
    PeriodCapConfig::with_fn_list(token, params.limit, params.period_secs, functions)
        .map_err(|_| INVALID)
}

fn error_for(reason: Reason) -> PeriodCapError {
    match reason {
        Reason::CapExceeded => PeriodCapError::CapExceeded,
        Reason::FunctionNotAllowed => PeriodCapError::FunctionNotAllowed,
        Reason::InvalidAmount => PeriodCapError::InvalidAmount,
        Reason::Overflow => PeriodCapError::Overflow,
        // The period-cap rule produces no other reasons.
        Reason::NotAllowlisted | Reason::ContractCreation | Reason::OutsideTimeWindow => {
            PeriodCapError::InvalidContext
        }
    }
}

#[cfg(test)]
mod test;
