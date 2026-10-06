use crate::ConfigError;

/// Maximum length of a Soroban symbol, and therefore of a function name.
pub const MAX_FN_NAME_LEN: usize = 32;

/// A validated contract function name stored inline, without allocation.
///
/// Names follow Soroban symbol rules: 1 to 32 characters from `[A-Za-z0-9_]`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FnName {
    buf: [u8; MAX_FN_NAME_LEN],
    len: u8,
}

impl FnName {
    pub fn new(name: &str) -> Result<Self, ConfigError> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > MAX_FN_NAME_LEN {
            return Err(ConfigError::InvalidFunctionName);
        }
        if !bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            return Err(ConfigError::InvalidFunctionName);
        }
        let mut buf = [0u8; MAX_FN_NAME_LEN];
        buf[..bytes.len()].copy_from_slice(bytes);
        Ok(Self {
            buf,
            len: bytes.len() as u8,
        })
    }

    pub fn as_str(&self) -> &str {
        // Only ASCII bytes are ever stored, so this cannot fail.
        core::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }
}

/// Maximum number of names in a [`FnList`].
pub const MAX_FN_LIST_LEN: usize = 10;

/// An inline list of up to [`MAX_FN_LIST_LEN`] function names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FnList {
    names: [Option<FnName>; MAX_FN_LIST_LEN],
    len: u8,
}

impl FnList {
    pub const fn empty() -> Self {
        Self {
            names: [None; MAX_FN_LIST_LEN],
            len: 0,
        }
    }

    pub fn new(names: &[&str]) -> Result<Self, ConfigError> {
        if names.len() > MAX_FN_LIST_LEN {
            return Err(ConfigError::TooManyFunctions);
        }
        let mut list = Self::empty();
        for (slot, name) in list.names.iter_mut().zip(names) {
            *slot = Some(FnName::new(name)?);
        }
        list.len = names.len() as u8;
        Ok(list)
    }

    /// Appends a name, for callers that cannot build a `&[&str]` up front.
    pub fn push(&mut self, name: &str) -> Result<(), ConfigError> {
        let slot = self
            .names
            .get_mut(self.len as usize)
            .ok_or(ConfigError::TooManyFunctions)?;
        *slot = Some(FnName::new(name)?);
        self.len += 1;
        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = &FnName> {
        self.names[..self.len as usize].iter().flatten()
    }

    pub fn contains(&self, function: &str) -> bool {
        self.iter().any(|n| n.as_str() == function)
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl core::fmt::Debug for FnName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    #[test]
    fn accepts_valid_symbols() {
        assert_eq!(FnName::new("transfer").unwrap().as_str(), "transfer");
        let long = "a".repeat(32);
        assert_eq!(FnName::new(&long).unwrap().as_str(), long);
    }

    #[test]
    fn rejects_invalid_symbols() {
        for bad in ["", "has space", "dash-name", "ünïcode", &"a".repeat(33)] {
            assert_eq!(FnName::new(bad), Err(ConfigError::InvalidFunctionName));
        }
    }

    #[test]
    fn fn_list_capacity_and_lookup() {
        let list = FnList::new(&["transfer", "approve"]).unwrap();
        assert!(list.contains("approve"));
        let mut pushed = FnList::empty();
        pushed.push("transfer").unwrap();
        pushed.push("approve").unwrap();
        assert_eq!(pushed, list);
        let mut full = FnList::new(&["f"; MAX_FN_LIST_LEN]).unwrap();
        assert_eq!(full.push("g"), Err(ConfigError::TooManyFunctions));
        assert!(!list.contains("burn"));
        assert_eq!(list.len(), 2);
        assert_eq!(
            FnList::new(&["f"; MAX_FN_LIST_LEN + 1]),
            Err(ConfigError::TooManyFunctions)
        );
    }
}
