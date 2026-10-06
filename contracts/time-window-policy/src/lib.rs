//! # Time-window policy
//!
//! A Mandate policy for OpenZeppelin smart accounts. A signer governed by this
//! policy may only act on configured UTC weekdays and within configured UTC
//! hour ranges, judged by the ledger timestamp.
//!
//! One deployment serves any number of smart accounts. Each account stores its
//! own configuration per context rule, and only that account can change it.
//! All decisions are made by [`mandate_core::time_window`].
#![no_std]

use mandate_core::{
    time_window::{self, HourRange as CoreRange, TimeWindowConfig, MAX_RANGES},
    Decision,
};
use mandate_soroban::{with_call, TTL_EXTEND_TO, TTL_THRESHOLD};
use soroban_sdk::{
    auth::Context, contract, contracterror, contractevent, contractimpl, contracttype,
    panic_with_error, Address, Env, Vec,
};
use stellar_accounts::{
    policies::Policy,
    smart_account::{ContextRule, Signer},
};

/// A half-open UTC hour range `[start, end)`, with `start < end <= 24`.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct HourRange {
    pub start: u32,
    pub end: u32,
}

/// Installation and configuration parameters.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct TimeWindowParams {
    /// Allowed UTC weekdays: bit 0 is Monday, bit 6 is Sunday.
    pub day_mask: u32,
    /// One to four allowed UTC hour ranges.
    pub ranges: Vec<HourRange>,
}

#[contracttype]
enum StorageKey {
    /// Configuration for a (smart account, context rule ID) pair.
    Config(Address, u32),
}

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TimeWindowError {
    /// No configuration exists for this smart account and context rule.
    NotInstalled = 4200,
    /// A configuration already exists for this smart account and context rule.
    AlreadyInstalled = 4201,
    /// The configuration is invalid: an empty or out-of-range day mask, no
    /// ranges or more than four, or a range that is not `start < end <= 24`.
    InvalidConfig = 4202,
    /// The ledger time is outside the configured days or hours.
    OutsideTimeWindow = 4203,
    /// The authorization context could not be read.
    InvalidContext = 4204,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct TimeWindowInstalled {
    #[topic]
    pub smart_account: Address,
    pub context_rule_id: u32,
    pub params: TimeWindowParams,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct TimeWindowConfigured {
    #[topic]
    pub smart_account: Address,
    pub context_rule_id: u32,
    pub params: TimeWindowParams,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct TimeWindowUninstalled {
    #[topic]
    pub smart_account: Address,
    pub context_rule_id: u32,
}

#[contract]
pub struct TimeWindowPolicy;

#[contractimpl]
impl Policy for TimeWindowPolicy {
    type AccountParams = TimeWindowParams;

    /// Denies any context evaluated outside the window by failing with a
    /// [`TimeWindowError`].
    fn enforce(
        e: &Env,
        context: Context,
        _authenticated_signers: Vec<Signer>,
        context_rule: ContextRule,
        smart_account: Address,
    ) {
        smart_account.require_auth();
        extend_instance(e);

        let params = load(e, &smart_account, context_rule.id);
        let config = to_core(&params).unwrap_or_else(|err| panic_with_error!(e, err));

        let decision = match &context {
            Context::Contract(call) => {
                with_call(e, call, None, |c| time_window::evaluate(&config, &(), c).0)
                    .unwrap_or_else(|| panic_with_error!(e, TimeWindowError::InvalidContext))
            }
            Context::CreateContractHostFn(_) | Context::CreateContractWithCtorHostFn(_) => {
                time_window::evaluate_create_contract(&config, &(), e.ledger().timestamp()).0
            }
        };

        // Denying fails the invocation, which rolls back any event it
        // published, so the typed error is the report. See docs/DECISIONS.md
        // (D8).
        if let Decision::Deny(_) = decision {
            panic_with_error!(e, TimeWindowError::OutsideTimeWindow);
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
            panic_with_error!(e, TimeWindowError::AlreadyInstalled);
        }
        if let Err(err) = to_core(&install_params) {
            panic_with_error!(e, err);
        }
        save(e, &key, &install_params);

        TimeWindowInstalled {
            smart_account,
            context_rule_id: context_rule.id,
            params: install_params,
        }
        .publish(e);
    }

    fn uninstall(e: &Env, context_rule: ContextRule, smart_account: Address) {
        smart_account.require_auth();

        let key = StorageKey::Config(smart_account.clone(), context_rule.id);
        if !e.storage().persistent().has(&key) {
            panic_with_error!(e, TimeWindowError::NotInstalled);
        }
        e.storage().persistent().remove(&key);

        TimeWindowUninstalled {
            smart_account,
            context_rule_id: context_rule.id,
        }
        .publish(e);
    }
}

#[contractimpl]
impl TimeWindowPolicy {
    /// Replaces the configuration for an installed context rule. Only the
    /// smart account itself can call this.
    pub fn set_config(
        e: Env,
        smart_account: Address,
        context_rule_id: u32,
        params: TimeWindowParams,
    ) {
        smart_account.require_auth();
        extend_instance(&e);

        let key = StorageKey::Config(smart_account.clone(), context_rule_id);
        if !e.storage().persistent().has(&key) {
            panic_with_error!(&e, TimeWindowError::NotInstalled);
        }
        if let Err(err) = to_core(&params) {
            panic_with_error!(&e, err);
        }
        save(&e, &key, &params);

        TimeWindowConfigured {
            smart_account,
            context_rule_id,
            params,
        }
        .publish(&e);
    }

    /// Returns the configuration for a smart account's context rule.
    pub fn get_config(e: Env, smart_account: Address, context_rule_id: u32) -> TimeWindowParams {
        load(&e, &smart_account, context_rule_id)
    }
}

fn load(e: &Env, smart_account: &Address, context_rule_id: u32) -> TimeWindowParams {
    let key = StorageKey::Config(smart_account.clone(), context_rule_id);
    let params = e
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| panic_with_error!(e, TimeWindowError::NotInstalled));
    e.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
    params
}

fn save(e: &Env, key: &StorageKey, params: &TimeWindowParams) {
    e.storage().persistent().set(key, params);
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
fn to_core(params: &TimeWindowParams) -> Result<TimeWindowConfig, TimeWindowError> {
    const INVALID: TimeWindowError = TimeWindowError::InvalidConfig;
    let narrow = |v: u32| u8::try_from(v).map_err(|_| INVALID);

    // Check the length before iterating so oversized input is rejected cheaply.
    if params.ranges.len() as usize > MAX_RANGES {
        return Err(INVALID);
    }
    let mut ranges = [CoreRange { start: 0, end: 0 }; MAX_RANGES];
    let mut len = 0;
    for (slot, range) in ranges.iter_mut().zip(params.ranges.iter()) {
        *slot = CoreRange {
            start: narrow(range.start)?,
            end: narrow(range.end)?,
        };
        len += 1;
    }
    TimeWindowConfig::new(narrow(params.day_mask)?, &ranges[..len]).map_err(|_| INVALID)
}

#[cfg(test)]
mod test;
