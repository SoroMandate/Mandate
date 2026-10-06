extern crate std;

use soroban_sdk::{
    auth::{Context, ContractContext, ContractExecutable, CreateContractHostFnContext},
    contract,
    testutils::{Address as _, Events as _, Ledger as _, MockAuth, MockAuthInvoke},
    vec,
    xdr::{ContractEventBody, ScError, ScErrorCode, ScErrorType, ScVal},
    Address, BytesN, Env, Error, Event as _, IntoVal, InvokeError, String, Symbol, Vec,
};
use stellar_accounts::smart_account::{ContextRule, ContextRuleType};

use crate::*;

/// Tuesday 2024-01-02 00:00:00 UTC.
const TUE: u64 = 1_704_153_600;
const HOUR: u64 = 3_600;
const DAY: u64 = 86_400;
const WEEKDAYS: u32 = 0b001_1111;

/// Stand-in for a contract an account might call.
#[contract]
struct Target;

struct Setup {
    e: Env,
    policy: Address,
    account: Address,
    target: Address,
}

impl Setup {
    fn new() -> Self {
        let e = Env::default();
        let policy = e.register(TimeWindowPolicy, ());
        let account = Address::generate(&e);
        let target = e.register(Target, ());
        Self {
            e,
            policy,
            account,
            target,
        }
    }

    fn client(&self) -> TimeWindowPolicyClient<'_> {
        TimeWindowPolicyClient::new(&self.e, &self.policy)
    }

    /// Monday to Friday, 08:00 to 20:00 UTC.
    fn params(&self) -> TimeWindowParams {
        params(&self.e, WEEKDAYS, &[(8, 20)])
    }

    fn install(&self) {
        self.e.mock_all_auths();
        self.client()
            .install(&self.params(), &rule(&self.e, 1), &self.account);
    }

    fn call(&self) -> Context {
        Context::Contract(ContractContext {
            contract: self.target.clone(),
            fn_name: Symbol::new(&self.e, "transfer"),
            args: vec![&self.e],
        })
    }

    fn enforce_at(&self, timestamp: u64, context: Context) -> Result<(), TimeWindowError> {
        self.e.ledger().set_timestamp(timestamp);
        match self.client().try_enforce(
            &context,
            &Vec::new(&self.e),
            &rule(&self.e, 1),
            &self.account,
        ) {
            Ok(_) => Ok(()),
            Err(Ok(err)) => Err(TimeWindowError::try_from(err).unwrap()),
            Err(Err(err)) => panic!("unexpected host error: {err:?}"),
        }
    }
}

fn params(e: &Env, day_mask: u32, ranges: &[(u32, u32)]) -> TimeWindowParams {
    let mut list = Vec::new(e);
    for (start, end) in ranges {
        list.push_back(HourRange {
            start: *start,
            end: *end,
        });
    }
    TimeWindowParams {
        day_mask,
        ranges: list,
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

fn contract_error<T>(err: TimeWindowError) -> Result<T, Result<Error, InvokeError>> {
    Err(Ok(err.into()))
}

const OUTSIDE: Result<(), TimeWindowError> = Err(TimeWindowError::OutsideTimeWindow);

#[test]
fn install_stores_config_and_emits_event() {
    let s = Setup::new();
    s.install();
    let expected = TimeWindowInstalled {
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
fn hour_range_is_half_open() {
    let s = Setup::new();
    s.install();
    assert_eq!(s.enforce_at(TUE + 8 * HOUR - 1, s.call()), OUTSIDE);
    assert_eq!(s.enforce_at(TUE + 8 * HOUR, s.call()), Ok(()));
    assert_eq!(s.enforce_at(TUE + 14 * HOUR, s.call()), Ok(()));
    assert_eq!(s.enforce_at(TUE + 20 * HOUR - 1, s.call()), Ok(()));
    assert_eq!(s.enforce_at(TUE + 20 * HOUR, s.call()), OUTSIDE);
}

#[test]
fn denies_disallowed_days() {
    let s = Setup::new();
    s.install();
    let friday_noon = TUE + 3 * DAY + 12 * HOUR;
    assert_eq!(s.enforce_at(friday_noon, s.call()), Ok(()));
    assert_eq!(s.enforce_at(friday_noon + DAY, s.call()), OUTSIDE); // Saturday
    assert_eq!(s.enforce_at(friday_noon + 2 * DAY, s.call()), OUTSIDE); // Sunday
    assert_eq!(s.enforce_at(friday_noon + 3 * DAY, s.call()), Ok(())); // Monday
}

#[test]
fn midnight_spanning_window_uses_two_ranges() {
    let s = Setup::new();
    s.e.mock_all_auths();
    let every_day = 0x7f;
    s.client().install(
        &params(&s.e, every_day, &[(22, 24), (0, 2)]),
        &rule(&s.e, 1),
        &s.account,
    );
    assert_eq!(s.enforce_at(TUE + 23 * HOUR + 59 * 60, s.call()), Ok(()));
    assert_eq!(s.enforce_at(TUE + DAY, s.call()), Ok(()));
    assert_eq!(s.enforce_at(TUE + DAY + 2 * HOUR, s.call()), OUTSIDE);
}

#[test]
fn contract_matches_core_for_every_hour_of_a_week() {
    let s = Setup::new();
    s.install();
    let config = to_core(&s.params()).unwrap();
    for hour in 0..7 * 24 {
        let ts = TUE + hour * HOUR + 1_800;
        let expected = if config.contains(ts) { Ok(()) } else { OUTSIDE };
        assert_eq!(s.enforce_at(ts, s.call()), expected, "hour {hour}");
    }
}

#[test]
fn contract_creation_follows_the_window() {
    let s = Setup::new();
    s.install();
    let create = || {
        Context::CreateContractHostFn(CreateContractHostFnContext {
            executable: ContractExecutable::Wasm(BytesN::from_array(&s.e, &[7; 32])),
            salt: BytesN::from_array(&s.e, &[0; 32]),
        })
    };
    assert_eq!(s.enforce_at(TUE + 9 * HOUR, create()), Ok(()));
    assert_eq!(s.enforce_at(TUE + 21 * HOUR, create()), OUTSIDE);
}

#[test]
fn rejects_invalid_configs_at_install() {
    let s = Setup::new();
    s.e.mock_all_auths();
    let e = &s.e;
    let invalid = [
        params(e, 0, &[(8, 20)]),     // empty day mask
        params(e, 0x80, &[(8, 20)]),  // bit 7
        params(e, 0x101, &[(8, 20)]), // does not fit in a byte
        params(e, WEEKDAYS, &[]),     // no ranges
        params(e, WEEKDAYS, &[(0, 1), (2, 3), (4, 5), (6, 7), (8, 9)]),
        params(e, WEEKDAYS, &[(24, 24)]),
        params(e, WEEKDAYS, &[(25, 26)]),
        params(e, WEEKDAYS, &[(8, 25)]),
        params(e, WEEKDAYS, &[(10, 10)]),
        params(e, WEEKDAYS, &[(20, 8)]),
        params(e, WEEKDAYS, &[(8, 256 + 20)]), // would truncate to 20
    ];
    for p in invalid {
        let result = s.client().try_install(&p, &rule(e, 1), &s.account);
        assert_eq!(
            result,
            contract_error(TimeWindowError::InvalidConfig),
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
    assert_eq!(result, contract_error(TimeWindowError::AlreadyInstalled));
}

#[test]
fn rules_are_isolated_per_account_and_rule_id() {
    let s = Setup::new();
    s.install();
    s.e.ledger().set_timestamp(TUE + 9 * HOUR);
    let other_account = Address::generate(&s.e);
    let result = s
        .client()
        .try_enforce(&s.call(), &Vec::new(&s.e), &rule(&s.e, 1), &other_account);
    assert_eq!(result, contract_error(TimeWindowError::NotInstalled));
    let result = s
        .client()
        .try_enforce(&s.call(), &Vec::new(&s.e), &rule(&s.e, 2), &s.account);
    assert_eq!(result, contract_error(TimeWindowError::NotInstalled));
}

#[test]
fn account_can_reconfigure() {
    let s = Setup::new();
    s.install();
    let weekends = params(&s.e, 0b110_0000, &[(0, 24)]);
    s.client().set_config(&s.account, &1, &weekends);

    let expected = TimeWindowConfigured {
        smart_account: s.account.clone(),
        context_rule_id: 1,
        params: weekends.clone(),
    };
    assert_eq!(
        s.e.events().all().filter_by_contract(&s.policy),
        [expected.to_xdr(&s.e, &s.policy)]
    );
    assert_eq!(s.client().get_config(&s.account, &1), weekends);
    assert_eq!(s.enforce_at(TUE + 12 * HOUR, s.call()), OUTSIDE);
    assert_eq!(s.enforce_at(TUE + 4 * DAY + 3 * HOUR, s.call()), Ok(()));
}

#[test]
fn reconfigure_rejects_invalid_config() {
    let s = Setup::new();
    s.install();
    let bad = params(&s.e, 0, &[(8, 20)]);
    assert_eq!(
        s.client().try_set_config(&s.account, &1, &bad),
        contract_error(TimeWindowError::InvalidConfig)
    );
    assert_eq!(s.client().get_config(&s.account, &1), s.params());
}

#[test]
fn unauthorized_reconfigure_is_rejected() {
    let s = Setup::new();
    s.install();
    let attacker = Address::generate(&s.e);
    let always = params(&s.e, 0x7f, &[(0, 24)]);
    // Only the attacker signs; the smart account does not.
    s.e.mock_auths(&[MockAuth {
        address: &attacker,
        invoke: &MockAuthInvoke {
            contract: &s.policy,
            fn_name: "set_config",
            args: (&s.account, 1u32, always.clone()).into_val(&s.e),
            sub_invokes: &[],
        },
    }]);
    assert!(s.client().try_set_config(&s.account, &1, &always).is_err());
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
    s.e.ledger().set_timestamp(TUE + 9 * HOUR);
    let result = s
        .client()
        .try_enforce(&s.call(), &Vec::new(&s.e), &rule(&s.e, 1), &s.account);
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
    let expected = TimeWindowUninstalled {
        smart_account: s.account.clone(),
        context_rule_id: 1,
    };
    assert_eq!(
        s.e.events().all().filter_by_contract(&s.policy),
        [expected.to_xdr(&s.e, &s.policy)]
    );
    assert_eq!(
        s.client().try_get_config(&s.account, &1),
        contract_error(TimeWindowError::NotInstalled)
    );
    assert_eq!(
        s.client().try_uninstall(&rule(&s.e, 1), &s.account),
        contract_error(TimeWindowError::NotInstalled)
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
    assert_eq!(s.enforce_at(TUE + 21 * HOUR, s.call()), OUTSIDE);
    // Contract events from a failed invocation are rolled back, but the host
    // records the typed error, which simulation returns to wallets.
    let code = TimeWindowError::OutsideTimeWindow as u32;
    let diagnostics = s.e.host().get_diagnostic_events().unwrap();
    let found = diagnostics.0.iter().any(|ev| {
        let ContractEventBody::V0(body) = &ev.event.body;
        body.topics.contains(&ScVal::Error(ScError::Contract(code)))
    });
    assert!(found, "error {code} missing from diagnostic events");
}
