//! # Allowlist policy
//!
//! A Mandate policy for OpenZeppelin smart accounts. A signer governed by this
//! policy may only call listed contracts and, when an entry lists functions,
//! only those functions. Contract-creation contexts are always denied.
//!
//! One deployment serves any number of smart accounts. Each account stores its
//! own configuration per context rule, and only that account can change it.
//! All decisions are made by [`mandate_core::allowlist`].
#![no_std]

use mandate_core::{
    allowlist::{self, AllowEntry as CoreEntry, AllowlistConfig, MAX_ENTRIES},
    Decision, FnList, Reason, MAX_FN_LIST_LEN,
};
use mandate_soroban::{contract_id, symbol_str, with_call, TTL_EXTEND_TO, TTL_THRESHOLD};
use soroban_sdk::{
    auth::Context, contract, contracterror, contractevent, contractimpl, contracttype,
    panic_with_error, Address, Env, Symbol, Vec,
};
use stellar_accounts::{
    policies::Policy,
    smart_account::{ContextRule, Signer},
};

/// One allowlisted contract. `functions: None` allows every function on it.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct AllowEntry {
    pub contract: Address,
    pub functions: Option<Vec<Symbol>>,
}

/// Installation and configuration parameters: up to 20 entries.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct AllowlistParams {
    pub entries: Vec<AllowEntry>,
}

#[contracttype]
enum StorageKey {
    /// Configuration for a (smart account, context rule ID) pair.
    Config(Address, u32),
}

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u32)]
pub enum AllowlistError {
    /// No configuration exists for this smart account and context rule.
    NotInstalled = 4100,
    /// A configuration already exists for this smart account and context rule.
    AlreadyInstalled = 4101,
    /// The configuration is invalid: too many entries or functions, a
    /// duplicate contract, a non-contract address, or a bad function name.
    InvalidConfig = 4102,
    /// The called contract, or the function on it, is not allowlisted.
    NotAllowlisted = 4103,
    /// The context creates a contract, which this policy always denies.
    ContractCreation = 4104,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct AllowlistInstalled {
    #[topic]
    pub smart_account: Address,
    pub context_rule_id: u32,
    pub params: AllowlistParams,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct AllowlistConfigured {
    #[topic]
    pub smart_account: Address,
    pub context_rule_id: u32,
    pub params: AllowlistParams,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct AllowlistUninstalled {
    #[topic]
    pub smart_account: Address,
    pub context_rule_id: u32,
}

#[contract]
pub struct AllowlistPolicy;

#[contractimpl]
impl Policy for AllowlistPolicy {
    type AccountParams = AllowlistParams;

    /// Denies any context that is not allowlisted by failing with an
    /// [`AllowlistError`].
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
        let config = to_core(e, &params).unwrap_or_else(|err| panic_with_error!(e, err));

        let decision = match &context {
            Context::Contract(call) => {
                with_call(e, call, None, |c| allowlist::evaluate(&config, &(), c).0)
                    .unwrap_or(Decision::Deny(Reason::NotAllowlisted))
            }
            Context::CreateContractHostFn(_) | Context::CreateContractWithCtorHostFn(_) => {
                allowlist::evaluate_create_contract(&config, &()).0
            }
        };

        // Denying means failing the invocation, which rolls back any event
        // it published, so the denial is reported through the typed error
        // alone. See docs/DECISIONS.md (D8).
        if let Decision::Deny(reason) = decision {
            panic_with_error!(e, error_for(reason));
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
            panic_with_error!(e, AllowlistError::AlreadyInstalled);
        }
        if let Err(err) = to_core(e, &install_params) {
            panic_with_error!(e, err);
        }
        save(e, &key, &install_params);

        AllowlistInstalled {
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
            panic_with_error!(e, AllowlistError::NotInstalled);
        }
        e.storage().persistent().remove(&key);

        AllowlistUninstalled {
            smart_account,
            context_rule_id: context_rule.id,
        }
        .publish(e);
    }
}

#[contractimpl]
impl AllowlistPolicy {
    /// Replaces the configuration for an installed context rule. Only the
    /// smart account itself can call this.
    pub fn set_config(
        e: Env,
        smart_account: Address,
        context_rule_id: u32,
        params: AllowlistParams,
    ) {
        smart_account.require_auth();
        extend_instance(&e);

        let key = StorageKey::Config(smart_account.clone(), context_rule_id);
        if !e.storage().persistent().has(&key) {
            panic_with_error!(&e, AllowlistError::NotInstalled);
        }
        if let Err(err) = to_core(&e, &params) {
            panic_with_error!(&e, err);
        }
        save(&e, &key, &params);

        AllowlistConfigured {
            smart_account,
            context_rule_id,
            params,
        }
        .publish(&e);
    }

    /// Returns the configuration for a smart account's context rule.
    pub fn get_config(e: Env, smart_account: Address, context_rule_id: u32) -> AllowlistParams {
        load(&e, &smart_account, context_rule_id)
    }
}

fn load(e: &Env, smart_account: &Address, context_rule_id: u32) -> AllowlistParams {
    let key = StorageKey::Config(smart_account.clone(), context_rule_id);
    let params = e
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| panic_with_error!(e, AllowlistError::NotInstalled));
    e.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
    params
}

fn save(e: &Env, key: &StorageKey, params: &AllowlistParams) {
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
fn to_core(e: &Env, params: &AllowlistParams) -> Result<AllowlistConfig, AllowlistError> {
    const INVALID: AllowlistError = AllowlistError::InvalidConfig;
    // Check lengths before iterating so oversized input is rejected cheaply.
    if params.entries.len() as usize > MAX_ENTRIES {
        return Err(INVALID);
    }
    let mut config = AllowlistConfig::new();
    for entry in params.entries.iter() {
        let id = contract_id(&entry.contract).ok_or(INVALID)?;
        let core_entry = match entry.functions {
            None => CoreEntry::any_function(id),
            Some(functions) => {
                if functions.len() as usize > MAX_FN_LIST_LEN {
                    return Err(INVALID);
                }
                let mut list = FnList::empty();
                for function in functions.iter() {
                    let name = symbol_str(e, &function).ok_or(INVALID)?;
                    list.push(name.as_ref()).map_err(|_| INVALID)?;
                }
                CoreEntry::with_fn_list(id, list)
            }
        };
        config.push(core_entry).map_err(|_| INVALID)?;
    }
    Ok(config)
}

fn error_for(reason: Reason) -> AllowlistError {
    match reason {
        Reason::ContractCreation => AllowlistError::ContractCreation,
        _ => AllowlistError::NotAllowlisted,
    }
}

#[cfg(test)]
mod test;
