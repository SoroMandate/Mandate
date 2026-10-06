extern crate std;

use soroban_sdk::{
    auth::{Context, ContractContext, ContractExecutable, CreateContractHostFnContext},
    contract,
    testutils::{Address as _, Events as _, MockAuth, MockAuthInvoke},
    vec,
    xdr::{ContractEventBody, ScError, ScErrorCode, ScErrorType, ScVal},
    Address, BytesN, Env, Error, Event as _, IntoVal, InvokeError, String, Symbol, Vec,
};
use stellar_accounts::smart_account::{ContextRule, ContextRuleType};

use crate::*;

/// Stand-in for a contract an account might call.
#[contract]
struct Target;

struct Setup {
    e: Env,
    policy: Address,
    account: Address,
    usdc: Address,
    dex: Address,
}

impl Setup {
    fn new() -> Self {
        let e = Env::default();
        let policy = e.register(AllowlistPolicy, ());
        let account = Address::generate(&e);
        let usdc = e.register(Target, ());
        let dex = e.register(Target, ());
        Self {
            e,
            policy,
            account,
            usdc,
            dex,
        }
    }

    fn client(&self) -> AllowlistPolicyClient<'_> {
        AllowlistPolicyClient::new(&self.e, &self.policy)
    }

    /// USDC limited to `transfer`; any function on the DEX.
    fn params(&self) -> AllowlistParams {
        AllowlistParams {
            entries: vec![
                &self.e,
                AllowEntry {
                    contract: self.usdc.clone(),
                    functions: Some(vec![&self.e, Symbol::new(&self.e, "transfer")]),
                },
                AllowEntry {
                    contract: self.dex.clone(),
                    functions: None,
                },
            ],
        }
    }

    fn install(&self) {
        self.e.mock_all_auths();
        self.client()
            .install(&self.params(), &rule(&self.e, 1), &self.account);
    }

    fn call(&self, contract: &Address, function: &str) -> Context {
        Context::Contract(ContractContext {
            contract: contract.clone(),
            fn_name: Symbol::new(&self.e, function),
            args: vec![&self.e],
        })
    }

    fn enforce(&self, context: Context) -> Result<(), AllowlistError> {
        match self.client().try_enforce(
            &context,
            &Vec::new(&self.e),
            &rule(&self.e, 1),
            &self.account,
        ) {
            Ok(_) => Ok(()),
            Err(Ok(err)) => Err(AllowlistError::try_from(err).unwrap()),
            Err(Err(err)) => panic!("unexpected host error: {err:?}"),
        }
    }
}

fn rule(e: &Env, id: u32) -> ContextRule {
    ContextRule {
        id,
        context_type: ContextRuleType::Default,
        name: String::from_str(e, "agent"),
        signers: Vec::new(e),
        signer_ids: Vec::new(e),
        policies: Vec::new(e),
        policy_ids: Vec::new(e),
        valid_until: None,
    }
}

/// A classic `G...` account address, which cannot be allowlisted.
fn account_address(e: &Env) -> Address {
    Address::from_str(
        e,
        "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
    )
}

fn contract_error<T>(err: AllowlistError) -> Result<T, Result<Error, InvokeError>> {
    Err(Ok(err.into()))
}

#[test]
fn install_stores_config_and_emits_event() {
    let s = Setup::new();
    s.install();
    let expected = AllowlistInstalled {
        smart_account: s.account.clone(),
        context_rule_id: 1,
        params: s.params(),
    };
    assert_eq!(
        s.e.events().all().filter_by_contract(&s.policy),
        [expected.to_xdr(&s.e, &s.policy)]
    );
    assert_eq!(s.client().get_config(&s.account, &1), s.params());
}

#[test]
fn allows_listed_contracts_and_functions() {
    let s = Setup::new();
    s.install();
    assert_eq!(s.enforce(s.call(&s.usdc, "transfer")), Ok(()));
    assert_eq!(s.enforce(s.call(&s.dex, "swap")), Ok(()));
    assert_eq!(s.enforce(s.call(&s.dex, "anything_else")), Ok(()));
}

#[test]
fn denies_unlisted_functions_and_contracts() {
    let s = Setup::new();
    s.install();
    assert_eq!(
        s.enforce(s.call(&s.usdc, "approve")),
        Err(AllowlistError::NotAllowlisted)
    );
    let other = s.e.register(Target, ());
    assert_eq!(
        s.enforce(s.call(&other, "transfer")),
        Err(AllowlistError::NotAllowlisted)
    );
}

#[test]
fn denies_contract_creation() {
    let s = Setup::new();
    s.install();
    let create = Context::CreateContractHostFn(CreateContractHostFnContext {
        executable: ContractExecutable::Wasm(BytesN::from_array(&s.e, &[7; 32])),
        salt: BytesN::from_array(&s.e, &[0; 32]),
    });
    assert_eq!(s.enforce(create), Err(AllowlistError::ContractCreation));
}

#[test]
fn rules_are_isolated_per_account_and_rule_id() {
    let s = Setup::new();
    s.install();
    let other_account = Address::generate(&s.e);
    let result = s.client().try_enforce(
        &s.call(&s.usdc, "transfer"),
        &Vec::new(&s.e),
        &rule(&s.e, 1),
        &other_account,
    );
    assert_eq!(result, contract_error(AllowlistError::NotInstalled));
    let result = s.client().try_enforce(
        &s.call(&s.usdc, "transfer"),
        &Vec::new(&s.e),
        &rule(&s.e, 2),
        &s.account,
    );
    assert_eq!(result, contract_error(AllowlistError::NotInstalled));
}

#[test]
fn rejects_invalid_configs_at_install() {
    let s = Setup::new();
    s.e.mock_all_auths();
    let e = &s.e;
    let entry = |contract: &Address, functions: Option<Vec<Symbol>>| AllowEntry {
        contract: contract.clone(),
        functions,
    };

    let mut too_many = Vec::new(e);
    for _ in 0..21 {
        too_many.push_back(entry(&e.register(Target, ()), None));
    }
    let mut many_functions = Vec::new(e);
    for i in 0..11u32 {
        many_functions.push_back(Symbol::new(e, &std::format!("f{i}")));
    }
    let invalid = [
        too_many,
        vec![e, entry(&s.usdc, None), entry(&s.usdc, None)],
        vec![e, entry(&account_address(e), None)],
        vec![e, entry(&s.usdc, Some(many_functions))],
    ];
    for entries in invalid {
        let result = s
            .client()
            .try_install(&AllowlistParams { entries }, &rule(e, 1), &s.account);
        assert_eq!(result, contract_error(AllowlistError::InvalidConfig));
    }
}

#[test]
fn cannot_install_twice() {
    let s = Setup::new();
    s.install();
    let result = s
        .client()
        .try_install(&s.params(), &rule(&s.e, 1), &s.account);
    assert_eq!(result, contract_error(AllowlistError::AlreadyInstalled));
}

#[test]
fn account_can_reconfigure() {
    let s = Setup::new();
    s.install();
    let params = AllowlistParams {
        entries: vec![
            &s.e,
            AllowEntry {
                contract: s.usdc.clone(),
                functions: None,
            },
        ],
    };
    s.client().set_config(&s.account, &1, &params);

    let expected = AllowlistConfigured {
        smart_account: s.account.clone(),
        context_rule_id: 1,
        params: params.clone(),
    };
    assert_eq!(
        s.e.events().all().filter_by_contract(&s.policy),
        [expected.to_xdr(&s.e, &s.policy)]
    );
    assert_eq!(s.client().get_config(&s.account, &1), params);
    assert_eq!(s.enforce(s.call(&s.usdc, "approve")), Ok(()));
    assert_eq!(
        s.enforce(s.call(&s.dex, "swap")),
        Err(AllowlistError::NotAllowlisted)
    );
}

#[test]
fn reconfigure_rejects_invalid_config() {
    let s = Setup::new();
    s.install();
    let bad = AllowlistParams {
        entries: vec![
            &s.e,
            AllowEntry {
                contract: account_address(&s.e),
                functions: None,
            },
        ],
    };
    assert_eq!(
        s.client().try_set_config(&s.account, &1, &bad),
        contract_error(AllowlistError::InvalidConfig)
    );
    assert_eq!(s.client().get_config(&s.account, &1), s.params());
}

#[test]
fn unauthorized_reconfigure_is_rejected() {
    let s = Setup::new();
    s.install();
    let attacker = Address::generate(&s.e);
    let params = AllowlistParams {
        entries: Vec::new(&s.e),
    };
    // Only the attacker signs; the smart account does not.
    s.e.mock_auths(&[MockAuth {
        address: &attacker,
        invoke: &MockAuthInvoke {
            contract: &s.policy,
            fn_name: "set_config",
            args: (&s.account, 1u32, params.clone()).into_val(&s.e),
            sub_invokes: &[],
        },
    }]);
    assert!(s.client().try_set_config(&s.account, &1, &params).is_err());
    assert_eq!(s.client().get_config(&s.account, &1), s.params());
}

#[test]
fn unauthorized_install_and_enforce_are_rejected() {
    let s = Setup::new();
    // No auths are mocked at all.
    assert!(s
        .client()
        .try_install(&s.params(), &rule(&s.e, 1), &s.account)
        .is_err());
    s.install();
    s.e.set_auths(&[]);
    let result = s.client().try_enforce(
        &s.call(&s.usdc, "transfer"),
        &Vec::new(&s.e),
        &rule(&s.e, 1),
        &s.account,
    );
    // The host rejects the missing authorization before the policy runs.
    assert_eq!(
        result,
        Err(Ok(Error::from_type_and_code(
            ScErrorType::Context,
            ScErrorCode::InvalidAction
        )))
    );
}

#[test]
fn uninstall_removes_config() {
    let s = Setup::new();
    s.install();
    s.client().uninstall(&rule(&s.e, 1), &s.account);
    let expected = AllowlistUninstalled {
        smart_account: s.account.clone(),
        context_rule_id: 1,
    };
    assert_eq!(
        s.e.events().all().filter_by_contract(&s.policy),
        [expected.to_xdr(&s.e, &s.policy)]
    );
    assert_eq!(
        s.client().try_get_config(&s.account, &1),
        contract_error(AllowlistError::NotInstalled)
    );
    assert_eq!(
        s.client().try_uninstall(&rule(&s.e, 1), &s.account),
        contract_error(AllowlistError::NotInstalled)
    );
}

#[test]
fn config_ttl_is_extended_on_write() {
    use soroban_sdk::testutils::storage::Persistent as _;
    let s = Setup::new();
    s.install();
    let ttl = s.e.as_contract(&s.policy, || {
        s.e.storage()
            .persistent()
            .get_ttl(&StorageKey::Config(s.account.clone(), 1))
    });
    assert_eq!(ttl, TTL_EXTEND_TO);
}

#[test]
fn denial_reason_is_visible_in_diagnostics() {
    let s = Setup::new();
    s.install();
    assert_eq!(
        s.enforce(s.call(&s.usdc, "approve")),
        Err(AllowlistError::NotAllowlisted)
    );
    // Contract events from a failed invocation are rolled back, but the host
    // records the typed error, which simulation returns to wallets.
    let code = AllowlistError::NotAllowlisted as u32;
    let diagnostics = s.e.host().get_diagnostic_events().unwrap();
    let found = diagnostics.0.iter().any(|ev| {
        let ContractEventBody::V0(body) = &ev.event.body;
        body.topics.contains(&ScVal::Error(ScError::Contract(code)))
    });
    assert!(found, "error {code} missing from diagnostic events");
}
