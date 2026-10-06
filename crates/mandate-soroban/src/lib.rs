//! Conversions from Soroban types into `mandate-core` types.
//!
//! This crate holds no policy logic. It only turns Soroban values (addresses,
//! symbols, authorization contexts) into the plain types that `mandate-core`
//! evaluates, and defines storage TTL constants shared by the contracts.
#![no_std]

use mandate_core::Call;
use soroban_sdk::{
    address_payload::AddressPayload, auth::ContractContext, Address, Env, Symbol, SymbolStr,
    TryFromVal,
};

/// Approximate number of ledgers per day (5-second ledgers).
pub const DAY_IN_LEDGERS: u32 = 17_280;
/// Persistent entries are extended to live this many ledgers after a write.
pub const TTL_EXTEND_TO: u32 = 30 * DAY_IN_LEDGERS;
/// Entries are only extended once their TTL falls below this threshold.
pub const TTL_THRESHOLD: u32 = TTL_EXTEND_TO - DAY_IN_LEDGERS;

/// Returns the 32-byte contract ID of a contract (`C...`) address, or `None`
/// for any other kind of address.
pub fn contract_id(address: &Address) -> Option<[u8; 32]> {
    match address.to_payload()? {
        AddressPayload::ContractIdHash(id) => Some(id.to_array()),
        AddressPayload::AccountIdPublicKeyEd25519(_) => None,
    }
}

/// Converts a symbol into an inline string buffer.
pub fn symbol_str(env: &Env, symbol: &Symbol) -> Option<SymbolStr> {
    SymbolStr::try_from_val(env, &symbol.to_symbol_val()).ok()
}

/// Builds a [`Call`] from a contract authorization context and passes it to
/// `f`. The `Call` borrows the function name, so it only lives for the
/// duration of `f`.
///
/// Returns `None` if the context targets a non-contract address or its
/// function name cannot be read; callers must treat that as a denial.
pub fn with_call<R>(
    env: &Env,
    context: &ContractContext,
    amount: Option<i128>,
    f: impl FnOnce(&Call) -> R,
) -> Option<R> {
    let contract = contract_id(&context.contract)?;
    let function = symbol_str(env, &context.fn_name)?;
    let call = Call {
        contract,
        function: function.as_ref(),
        amount,
        timestamp: env.ledger().timestamp(),
    };
    Some(f(&call))
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, testutils::Ledger as _, vec, Env};

    #[test]
    fn converts_contract_context() {
        let env = Env::default();
        env.ledger().set_timestamp(1_234);
        let target = Address::generate(&env);
        let context = ContractContext {
            contract: target.clone(),
            fn_name: Symbol::new(&env, "a_long_function_name_over_9"),
            args: vec![&env],
        };
        let (contract, function, amount, timestamp) = with_call(&env, &context, Some(7), |c| {
            (
                c.contract,
                c.function == "a_long_function_name_over_9",
                c.amount,
                c.timestamp,
            )
        })
        .unwrap();
        assert_eq!(Some(contract), contract_id(&target));
        assert!(function);
        assert_eq!(amount, Some(7));
        assert_eq!(timestamp, 1_234);
    }
}
