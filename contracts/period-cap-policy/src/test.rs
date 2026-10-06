extern crate std;

use soroban_sdk::{
    auth::{Context, ContractContext, ContractExecutable, CreateContractHostFnContext},
    contract,
    testutils::{Address as _, Events as _, Ledger as _, MockAuth, MockAuthInvoke},
    vec,
    xdr::{ContractEventBody, ScError, ScErrorCode, ScErrorType, ScVal},
    Address, BytesN, Env, Error, Event as _, IntoVal, InvokeError, String, Symbol, Val, Vec,
};
use stellar_accounts::smart_account::{ContextRule, ContextRuleType};

use crate::*;
use mandate_soroban::DAY_IN_LEDGERS;

/// Tuesday 2024-01-02 00:00:00 UTC.
const T0: u64 = 1_704_153_600;
const DAY: u64 = 86_400;
const LIMIT: i128 = 50;

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
        e.ledger().set_timestamp(T0);
        let policy = e.register(PeriodCapPolicy, ());
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

    fn client(&self) -> PeriodCapPolicyClient<'_> {
        PeriodCapPolicyClient::new(&self.e, &self.policy)
    }

    /// 50 USDC per day; `balance` allowed and not counted.
    fn params(&self) -> PeriodCapParams {
        PeriodCapParams {
            token: self.usdc.clone(),
            limit: LIMIT,
            period_secs: DAY,
            allowed_functions: vec![&self.e, Symbol::new(&self.e, "balance")],
        }
    }

    fn install_with(&self, params: &PeriodCapParams, rule_id: u32) {
        self.e.mock_all_auths();
        self.client()
            .install(params, &rule(&self.e, rule_id), &self.account);
    }

    fn install(&self) {
        self.install_with(&self.params(), 1);
    }

    fn call(&self, contract: &Address, function: &str, args: Vec<Val>) -> Context {
        Context::Contract(ContractContext {
            contract: contract.clone(),
            fn_name: Symbol::new(&self.e, function),
            args,
        })
    }

    fn transfer(&self, amount: i128) -> Context {
        let to = Address::generate(&self.e);
        let args = (self.account.clone(), to, amount).into_val(&self.e);
        self.call(&self.usdc, "transfer", args)
    }

    fn enforce_rule(&self, context: Context, rule_id: u32) -> Result<(), PeriodCapError> {
        match self.client().try_enforce(
            &context,
            &Vec::new(&self.e),
            &rule(&self.e, rule_id),
            &self.account,
        ) {
            Ok(_) => Ok(()),
            Err(Ok(err)) => Err(PeriodCapError::try_from(err).unwrap()),
            Err(Err(err)) => panic!("unexpected host error: {err:?}"),
        }
    }

    fn enforce(&self, context: Context) -> Result<(), PeriodCapError> {
        self.enforce_rule(context, 1)
    }

    fn enforce_at(&self, timestamp: u64, context: Context) -> Result<(), PeriodCapError> {
        self.e.ledger().set_timestamp(timestamp);
        self.enforce(context)
    }

    fn state(&self) -> Option<PeriodCapState> {
        self.client().get_state(&self.account, &self.usdc)
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

fn state(period_start: u64, spent: i128) -> Option<PeriodCapState> {
    Some(PeriodCapState {
        period_start,
        spent,
    })
}

fn contract_error<T>(err: PeriodCapError) -> Result<T, Result<Error, InvokeError>> {
    Err(Ok(err.into()))
}

const CAP_EXCEEDED: Result<(), PeriodCapError> = Err(PeriodCapError::CapExceeded);

#[test]
fn install_stores_config_and_emits_event() {
    let s = Setup::new();
    s.install();
    let expected = PeriodCapInstalled {
        smart_account: s.account.clone(),
        context_rule_id: 1,
        params: s.params(),
    };
    assert_eq!(
        s.e.events().all().filter_by_contract(&s.policy),
        [expected.to_xdr(&s.e, &s.policy)]
    );
    assert_eq!(s.client().get_config(&s.account, &1), s.params());
    assert_eq!(s.state(), None);
    assert_eq!(s.client().remaining(&s.account, &1), LIMIT);
}

#[test]
fn records_spending_up_to_the_limit() {
    let s = Setup::new();
    s.install();

    assert_eq!(s.enforce(s.transfer(40)), Ok(()));
    let expected = PeriodCapSpent {
        smart_account: s.account.clone(),
        token: s.usdc.clone(),
        context_rule_id: 1,
        period_start: T0,
        spent: 40,
    };
    assert_eq!(
        s.e.events().all().filter_by_contract(&s.policy),
        [expected.to_xdr(&s.e, &s.policy)]
    );
    assert_eq!(s.state(), state(T0, 40));
    assert_eq!(s.client().remaining(&s.account, &1), 10);

    assert_eq!(s.enforce_at(T0 + 60, s.transfer(10)), Ok(()));
    assert_eq!(s.state(), state(T0, 50));
    assert_eq!(s.client().remaining(&s.account, &1), 0);
}

#[test]
fn denies_over_the_limit_without_changing_state() {
    let s = Setup::new();
    s.install();
    assert_eq!(s.enforce(s.transfer(45)), Ok(()));
    assert_eq!(s.enforce(s.transfer(6)), CAP_EXCEEDED);
    assert_eq!(s.state(), state(T0, 45));
    assert_eq!(s.enforce(s.transfer(LIMIT + 1)), CAP_EXCEEDED);
    assert_eq!(s.enforce(s.transfer(5)), Ok(()));
}

#[test]
fn resets_exactly_at_the_period_boundary() {
    let s = Setup::new();
    s.install();
    assert_eq!(s.enforce(s.transfer(LIMIT)), Ok(()));
    assert_eq!(s.enforce_at(T0 + DAY - 1, s.transfer(1)), CAP_EXCEEDED);
    assert_eq!(s.client().remaining(&s.account, &1), 0);

    s.e.ledger().set_timestamp(T0 + DAY);
    assert_eq!(s.client().remaining(&s.account, &1), LIMIT);
    assert_eq!(s.enforce(s.transfer(30)), Ok(()));
    assert_eq!(s.state(), state(T0 + DAY, 30));
}

#[test]
fn periods_stay_aligned_after_idle_time() {
    let s = Setup::new();
    s.install();
    assert_eq!(s.enforce(s.transfer(1)), Ok(()));
    assert_eq!(s.enforce_at(T0 + 3 * DAY + 500, s.transfer(2)), Ok(()));
    assert_eq!(s.state(), state(T0 + 3 * DAY, 2));
}

#[test]
fn other_contracts_and_allowed_functions_are_not_counted() {
    let s = Setup::new();
    s.install();
    assert_eq!(s.enforce(s.transfer(LIMIT)), Ok(()));
    let swap = (1_000_000i128, 2u32).into_val(&s.e);
    assert_eq!(s.enforce(s.call(&s.dex, "swap", swap)), Ok(()));
    let balance = (s.account.clone(),).into_val(&s.e);
    assert_eq!(s.enforce(s.call(&s.usdc, "balance", balance)), Ok(()));
    assert_eq!(s.state(), state(T0, LIMIT));
}

#[test]
fn denies_other_token_functions() {
    let s = Setup::new();
    s.install();
    let spender = Address::generate(&s.e);
    let approve = (s.account.clone(), spender.clone(), 1i128, 100u32).into_val(&s.e);
    assert_eq!(
        s.enforce(s.call(&s.usdc, "approve", approve)),
        Err(PeriodCapError::FunctionNotAllowed)
    );
    // transfer_from moves the account's tokens too, so it must not bypass
    // the cap.
    let to = Address::generate(&s.e);
    let transfer_from = (spender, s.account.clone(), to, 1i128).into_val(&s.e);
    assert_eq!(
        s.enforce(s.call(&s.usdc, "transfer_from", transfer_from)),
        Err(PeriodCapError::FunctionNotAllowed)
    );
}

#[test]
fn denies_missing_negative_or_malformed_amounts() {
    let s = Setup::new();
    s.install();
    let invalid = Err(PeriodCapError::InvalidAmount);
    assert_eq!(s.enforce(s.transfer(-1)), invalid);
    let to = Address::generate(&s.e);
    let missing = (s.account.clone(), to.clone()).into_val(&s.e);
    assert_eq!(s.enforce(s.call(&s.usdc, "transfer", missing)), invalid);
    let wrong_type = (s.account.clone(), to, 5u32).into_val(&s.e);
    assert_eq!(s.enforce(s.call(&s.usdc, "transfer", wrong_type)), invalid);
    assert_eq!(s.state(), None);
}

#[test]
fn overflow_is_denied_not_panicked() {
    let s = Setup::new();
    let mut params = s.params();
    params.limit = i128::MAX;
    s.install_with(&params, 1);
    assert_eq!(s.enforce(s.transfer(i128::MAX)), Ok(()));
    assert_eq!(s.enforce(s.transfer(1)), Err(PeriodCapError::Overflow));
    assert_eq!(s.state(), state(T0, i128::MAX));
}

#[test]
fn rules_on_the_same_token_share_one_budget() {
    let s = Setup::new();
    s.install();
    let mut tighter = s.params();
    tighter.limit = 30;
    s.install_with(&tighter, 2);

    assert_eq!(s.enforce_rule(s.transfer(25), 1), Ok(()));
    assert_eq!(s.enforce_rule(s.transfer(10), 2), CAP_EXCEEDED);
    assert_eq!(s.enforce_rule(s.transfer(5), 2), Ok(()));
    assert_eq!(s.state(), state(T0, 30));
}

#[test]
fn contract_creation_is_allowed() {
    let s = Setup::new();
    s.install();
    let create = Context::CreateContractHostFn(CreateContractHostFnContext {
        executable: ContractExecutable::Wasm(BytesN::from_array(&s.e, &[7; 32])),
        salt: BytesN::from_array(&s.e, &[0; 32]),
    });
    assert_eq!(s.enforce(create), Ok(()));
    assert_eq!(s.state(), None);
}

#[test]
fn rejects_invalid_configs_at_install() {
    let s = Setup::new();
    s.e.mock_all_auths();
    let e = &s.e;
    let mut many_functions = Vec::new(e);
    for i in 0..11u32 {
        many_functions.push_back(Symbol::new(e, &std::format!("f{i}")));
    }
    let base = s.params();
    let invalid = [
        PeriodCapParams {
            limit: 0,
            ..base.clone()
        },
        PeriodCapParams {
            limit: -1,
            ..base.clone()
        },
        PeriodCapParams {
            period_secs: 0,
            ..base.clone()
        },
        PeriodCapParams {
            token: Address::from_str(
                e,
                "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
            ),
            ..base.clone()
        },
        PeriodCapParams {
            allowed_functions: many_functions,
            ..base
        },
    ];
    for p in invalid {
        let result = s.client().try_install(&p, &rule(e, 1), &s.account);
        assert_eq!(
            result,
            contract_error(PeriodCapError::InvalidConfig),
            "{p:?}"
        );
    }
}

#[test]
fn cannot_install_twice() {
    let s = Setup::new();
    s.install();
    let result = s
        .client()
        .try_install(&s.params(), &rule(&s.e, 1), &s.account);
    assert_eq!(result, contract_error(PeriodCapError::AlreadyInstalled));
}

#[test]
fn rules_are_isolated_per_account_and_rule_id() {
    let s = Setup::new();
    s.install();
    let other_account = Address::generate(&s.e);
    let result = s.client().try_enforce(
        &s.transfer(1),
        &Vec::new(&s.e),
        &rule(&s.e, 1),
        &other_account,
    );
    assert_eq!(result, contract_error(PeriodCapError::NotInstalled));
    assert_eq!(
        s.enforce_rule(s.transfer(1), 2),
        Err(PeriodCapError::NotInstalled)
    );
}

#[test]
fn account_can_reconfigure_and_keeps_spending() {
    let s = Setup::new();
    s.install();
    assert_eq!(s.enforce(s.transfer(40)), Ok(()));

    let mut lower = s.params();
    lower.limit = 30;
    s.client().set_config(&s.account, &1, &lower);
    let expected = PeriodCapConfigured {
        smart_account: s.account.clone(),
        context_rule_id: 1,
        params: lower.clone(),
    };
    assert_eq!(
        s.e.events().all().filter_by_contract(&s.policy),
        [expected.to_xdr(&s.e, &s.policy)]
    );
    assert_eq!(s.client().get_config(&s.account, &1), lower);
    assert_eq!(s.client().remaining(&s.account, &1), 0);
    assert_eq!(s.enforce(s.transfer(1)), CAP_EXCEEDED);
}

#[test]
fn reconfigure_rejects_invalid_config() {
    let s = Setup::new();
    s.install();
    let mut bad = s.params();
    bad.period_secs = 0;
    assert_eq!(
        s.client().try_set_config(&s.account, &1, &bad),
        contract_error(PeriodCapError::InvalidConfig)
    );
    assert_eq!(s.client().get_config(&s.account, &1), s.params());
}

#[test]
fn unauthorized_reconfigure_is_rejected() {
    let s = Setup::new();
    s.install();
    let attacker = Address::generate(&s.e);
    let mut generous = s.params();
    generous.limit = i128::MAX;
    // Only the attacker signs; the smart account does not.
    s.e.mock_auths(&[MockAuth {
        address: &attacker,
        invoke: &MockAuthInvoke {
            contract: &s.policy,
            fn_name: "set_config",
            args: (&s.account, 1u32, generous.clone()).into_val(&s.e),
            sub_invokes: &[],
        },
    }]);
    assert!(s
        .client()
        .try_set_config(&s.account, &1, &generous)
        .is_err());
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
    let result =
        s.client()
            .try_enforce(&s.transfer(1), &Vec::new(&s.e), &rule(&s.e, 1), &s.account);
    // The host rejects the missing authorization before the policy runs.
    assert_eq!(
        result,
        Err(Ok(Error::from_type_and_code(
            ScErrorType::Context,
            ScErrorCode::InvalidAction
        )))
    );
    s.e.mock_all_auths();
    assert_eq!(s.state(), None);
}

#[test]
fn uninstall_removes_config_but_keeps_spending() {
    let s = Setup::new();
    s.install();
    assert_eq!(s.enforce(s.transfer(LIMIT)), Ok(()));
    s.client().uninstall(&rule(&s.e, 1), &s.account);
    let expected = PeriodCapUninstalled {
        smart_account: s.account.clone(),
        context_rule_id: 1,
    };
    assert_eq!(
        s.e.events().all().filter_by_contract(&s.policy),
        [expected.to_xdr(&s.e, &s.policy)]
    );
    assert_eq!(
        s.client().try_get_config(&s.account, &1),
        contract_error(PeriodCapError::NotInstalled)
    );
    assert_eq!(
        s.client().try_uninstall(&rule(&s.e, 1), &s.account),
        contract_error(PeriodCapError::NotInstalled)
    );

    // Reinstalling does not reset the period's spending.
    s.install();
    assert_eq!(s.enforce(s.transfer(1)), CAP_EXCEEDED);
}

#[test]
fn state_ttl_is_extended_on_every_write() {
    use soroban_sdk::testutils::storage::Persistent as _;
    let s = Setup::new();
    s.install();
    let key = StorageKey::State(s.account.clone(), s.usdc.clone());
    let ttl = || {
        s.e.as_contract(&s.policy, || s.e.storage().persistent().get_ttl(&key))
    };

    assert_eq!(s.enforce(s.transfer(1)), Ok(()));
    assert_eq!(ttl(), TTL_EXTEND_TO);

    // Let the TTL drop below the threshold, then write again.
    let sequence = s.e.ledger().sequence();
    s.e.ledger()
        .set_sequence_number(sequence + 2 * DAY_IN_LEDGERS);
    assert!(ttl() < TTL_THRESHOLD);
    assert_eq!(s.enforce(s.transfer(1)), Ok(()));
    assert_eq!(ttl(), TTL_EXTEND_TO);

    let config_key = StorageKey::Config(s.account.clone(), 1);
    let config_ttl = s.e.as_contract(&s.policy, || {
        s.e.storage().persistent().get_ttl(&config_key)
    });
    assert_eq!(config_ttl, TTL_EXTEND_TO);
}

#[test]
fn denial_reason_is_visible_in_diagnostics() {
    let s = Setup::new();
    s.install();
    assert_eq!(s.enforce(s.transfer(LIMIT + 1)), CAP_EXCEEDED);
    // Contract events from a failed invocation are rolled back, but the host
    // records the typed error, which simulation returns to wallets.
    let code = PeriodCapError::CapExceeded as u32;
    let diagnostics = s.e.host().get_diagnostic_events().unwrap();
    let found = diagnostics.0.iter().any(|ev| {
        let ContractEventBody::V0(body) = &ev.event.body;
        body.topics.contains(&ScVal::Error(ScError::Contract(code)))
    });
    assert!(found, "error {code} missing from diagnostic events");
}
