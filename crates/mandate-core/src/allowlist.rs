//! Allowlist rule: only listed contracts, and optionally only listed functions
//! on them, may be called. Contract creation is always denied.

use crate::{Call, ConfigError, Decision, FnList, FnName, Reason};

/// Maximum number of contracts in one allowlist.
pub const MAX_ENTRIES: usize = 20;
/// Maximum number of function names on one allowlist entry.
pub const MAX_FUNCTIONS_PER_ENTRY: usize = crate::fn_name::MAX_FN_LIST_LEN;

/// One allowlisted contract.
///
/// When `functions` is `None`, every function on the contract is allowed.
/// Otherwise only the listed functions are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllowEntry {
    pub contract: [u8; 32],
    functions: Option<FnList>,
}

impl AllowEntry {
    /// An entry that allows every function on `contract`.
    pub fn any_function(contract: [u8; 32]) -> Self {
        Self {
            contract,
            functions: None,
        }
    }

    /// An entry that allows only the named functions on `contract`.
    pub fn with_functions(contract: [u8; 32], names: &[&str]) -> Result<Self, ConfigError> {
        Ok(Self {
            contract,
            functions: Some(FnList::new(names)?),
        })
    }

    /// An entry that allows only the functions in `functions` on `contract`.
    pub fn with_fn_list(contract: [u8; 32], functions: FnList) -> Self {
        Self {
            contract,
            functions: Some(functions),
        }
    }

    /// The listed functions, or `None` if every function is allowed.
    pub fn functions(&self) -> Option<impl Iterator<Item = &FnName>> {
        self.functions.as_ref().map(FnList::iter)
    }

    fn allows(&self, function: &str) -> bool {
        self.functions.is_none_or(|l| l.contains(function))
    }
}

/// A validated allowlist of up to [`MAX_ENTRIES`] contracts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllowlistConfig {
    entries: [Option<AllowEntry>; MAX_ENTRIES],
    len: u8,
}

impl Default for AllowlistConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl AllowlistConfig {
    /// An empty allowlist, which denies every call.
    pub const fn new() -> Self {
        Self {
            entries: [None; MAX_ENTRIES],
            len: 0,
        }
    }

    /// Builds a validated allowlist from a slice of entries.
    pub fn from_entries(entries: &[AllowEntry]) -> Result<Self, ConfigError> {
        let mut config = Self::new();
        for entry in entries {
            config.push(*entry)?;
        }
        Ok(config)
    }

    /// Appends an entry, rejecting duplicates and overflow of the capacity.
    pub fn push(&mut self, entry: AllowEntry) -> Result<(), ConfigError> {
        if self.entries().any(|e| e.contract == entry.contract) {
            return Err(ConfigError::DuplicateEntry);
        }
        let slot = self
            .entries
            .get_mut(self.len as usize)
            .ok_or(ConfigError::TooManyEntries)?;
        *slot = Some(entry);
        self.len += 1;
        Ok(())
    }

    pub fn entries(&self) -> impl Iterator<Item = &AllowEntry> {
        self.entries[..self.len as usize].iter().flatten()
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Allows the call only if its contract is listed and, when the entry lists
/// functions, its function is one of them.
pub fn evaluate(config: &AllowlistConfig, _state: &(), call: &Call) -> (Decision, ()) {
    let allowed = config
        .entries()
        .any(|e| e.contract == call.contract && e.allows(call.function));
    if allowed {
        (Decision::Allow, ())
    } else {
        (Decision::Deny(Reason::NotAllowlisted), ())
    }
}

/// Contract-creation contexts are always denied by the allowlist.
pub fn evaluate_create_contract(_config: &AllowlistConfig, _state: &()) -> (Decision, ()) {
    (Decision::Deny(Reason::ContractCreation), ())
}

#[cfg(test)]
mod tests {
    use super::*;

    const USDC: [u8; 32] = [1; 32];
    const DEX: [u8; 32] = [2; 32];

    fn call(contract: [u8; 32], function: &str) -> Call<'_> {
        Call {
            contract,
            function,
            amount: None,
            timestamp: 0,
        }
    }

    #[test]
    fn empty_allowlist_denies_everything() {
        let config = AllowlistConfig::new();
        assert_eq!(
            evaluate(&config, &(), &call(USDC, "transfer")).0,
            Decision::Deny(Reason::NotAllowlisted)
        );
    }

    #[test]
    fn any_function_entry_allows_all_functions() {
        let config = AllowlistConfig::from_entries(&[AllowEntry::any_function(DEX)]).unwrap();
        assert_eq!(
            evaluate(&config, &(), &call(DEX, "swap")).0,
            Decision::Allow
        );
        assert_eq!(
            evaluate(&config, &(), &call(DEX, "anything")).0,
            Decision::Allow
        );
        assert_eq!(
            evaluate(&config, &(), &call(USDC, "transfer")).0,
            Decision::Deny(Reason::NotAllowlisted)
        );
    }

    #[test]
    fn function_list_restricts_functions() {
        let entry = AllowEntry::with_functions(USDC, &["transfer"]).unwrap();
        let config = AllowlistConfig::from_entries(&[entry]).unwrap();
        assert_eq!(
            evaluate(&config, &(), &call(USDC, "transfer")).0,
            Decision::Allow
        );
        assert_eq!(
            evaluate(&config, &(), &call(USDC, "approve")).0,
            Decision::Deny(Reason::NotAllowlisted)
        );
    }

    #[test]
    fn empty_function_list_allows_no_functions() {
        let entry = AllowEntry::with_functions(USDC, &[]).unwrap();
        let config = AllowlistConfig::from_entries(&[entry]).unwrap();
        assert_eq!(
            evaluate(&config, &(), &call(USDC, "transfer")).0,
            Decision::Deny(Reason::NotAllowlisted)
        );
    }

    #[test]
    fn contract_creation_is_denied() {
        let config = AllowlistConfig::from_entries(&[AllowEntry::any_function(DEX)]).unwrap();
        assert_eq!(
            evaluate_create_contract(&config, &()).0,
            Decision::Deny(Reason::ContractCreation)
        );
    }

    #[test]
    fn rejects_more_than_max_entries() {
        let mut config = AllowlistConfig::new();
        for i in 0..MAX_ENTRIES {
            config
                .push(AllowEntry::any_function([i as u8; 32]))
                .unwrap();
        }
        assert_eq!(
            config.push(AllowEntry::any_function([0xff; 32])),
            Err(ConfigError::TooManyEntries)
        );
    }

    #[test]
    fn rejects_duplicates_and_bad_function_lists() {
        assert_eq!(
            AllowlistConfig::from_entries(&[
                AllowEntry::any_function(USDC),
                AllowEntry::any_function(USDC)
            ]),
            Err(ConfigError::DuplicateEntry)
        );
        let too_many = ["f"; MAX_FUNCTIONS_PER_ENTRY + 1];
        assert_eq!(
            AllowEntry::with_functions(USDC, &too_many),
            Err(ConfigError::TooManyFunctions)
        );
        assert_eq!(
            AllowEntry::with_functions(USDC, &["bad name"]),
            Err(ConfigError::InvalidFunctionName)
        );
    }
}
