//! Pure evaluation logic for Mandate spending policies.
//!
//! This crate is the single source of truth for every policy decision. The
//! Soroban contracts, the CLI, and the browser simulator all convert their
//! inputs into a [`Call`] and delegate to the `evaluate` functions here, so
//! they cannot disagree.
//!
//! The crate is `no_std`, allocation-free, and has no dependency on
//! `soroban-sdk`. Every rule exposes
//! `evaluate(&config, &state, &call) -> (Decision, NewState)` with no I/O.
//! Stateless rules use `()` as their state.
#![no_std]
#![forbid(unsafe_code)]

pub mod allowlist;
pub mod period_cap;
pub mod time_window;

mod fn_name;

pub use fn_name::{FnList, FnName, MAX_FN_LIST_LEN, MAX_FN_NAME_LEN};

/// A single contract invocation being authorized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Call<'a> {
    /// The 32-byte contract ID of the contract being called.
    pub contract: [u8; 32],
    /// The name of the function being called.
    pub function: &'a str,
    /// The amount moved by the call, when the caller could determine one
    /// (for example the third argument of `transfer(from, to, amount)`).
    pub amount: Option<i128>,
    /// UNIX timestamp, in seconds, at which the call is evaluated.
    pub timestamp: u64,
}

/// The outcome of evaluating a call against a rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(Reason),
}

impl Decision {
    pub fn is_allow(&self) -> bool {
        matches!(self, Decision::Allow)
    }
}

/// Why a call was denied.
///
/// The discriminants are stable: contracts and tooling may expose them as
/// numeric codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Reason {
    /// The called contract, or the function on it, is not on the allowlist.
    NotAllowlisted = 1,
    /// The context creates a contract, which the allowlist always denies.
    ContractCreation = 2,
    /// The call happens outside the configured days or hours.
    OutsideTimeWindow = 3,
    /// The transfer would push spending in the current period over the limit.
    CapExceeded = 4,
    /// The call targets the capped token with a function other than
    /// `transfer` that has not been explicitly allowed.
    FunctionNotAllowed = 5,
    /// A transfer amount is missing or negative.
    InvalidAmount = 6,
    /// An arithmetic operation would overflow.
    Overflow = 7,
}

impl Reason {
    /// A stable, human-readable name for the reason.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Reason::NotAllowlisted => "NotAllowlisted",
            Reason::ContractCreation => "ContractCreation",
            Reason::OutsideTimeWindow => "OutsideTimeWindow",
            Reason::CapExceeded => "CapExceeded",
            Reason::FunctionNotAllowed => "FunctionNotAllowed",
            Reason::InvalidAmount => "InvalidAmount",
            Reason::Overflow => "Overflow",
        }
    }

    pub const fn code(&self) -> u32 {
        *self as u32
    }
}

/// Why a configuration was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ConfigError {
    /// The allowlist has more than [`allowlist::MAX_ENTRIES`] entries.
    TooManyEntries = 1,
    /// A function list holds more than
    /// [`MAX_FN_LIST_LEN`] functions.
    TooManyFunctions = 2,
    /// The same contract appears in more than one allowlist entry.
    DuplicateEntry = 3,
    /// A function name is empty, longer than 32 bytes, or contains characters
    /// outside `[A-Za-z0-9_]`.
    InvalidFunctionName = 4,
    /// The weekday mask allows no days.
    EmptyDayMask = 5,
    /// The weekday mask sets bits above bit 6.
    InvalidDayMask = 6,
    /// No hour ranges were given.
    NoHourRanges = 7,
    /// More than [`time_window::MAX_RANGES`] hour ranges were given.
    TooManyHourRanges = 8,
    /// An hour range has `start >= 24`, `end > 24`, or `start >= end`.
    InvalidHourRange = 9,
    /// The spending limit is zero or negative.
    NonPositiveLimit = 10,
    /// The period length is zero.
    ZeroPeriod = 11,
}

impl ConfigError {
    pub const fn as_str(&self) -> &'static str {
        match self {
            ConfigError::TooManyEntries => "TooManyEntries",
            ConfigError::TooManyFunctions => "TooManyFunctions",
            ConfigError::DuplicateEntry => "DuplicateEntry",
            ConfigError::InvalidFunctionName => "InvalidFunctionName",
            ConfigError::EmptyDayMask => "EmptyDayMask",
            ConfigError::InvalidDayMask => "InvalidDayMask",
            ConfigError::NoHourRanges => "NoHourRanges",
            ConfigError::TooManyHourRanges => "TooManyHourRanges",
            ConfigError::InvalidHourRange => "InvalidHourRange",
            ConfigError::NonPositiveLimit => "NonPositiveLimit",
            ConfigError::ZeroPeriod => "ZeroPeriod",
        }
    }

    pub const fn code(&self) -> u32 {
        *self as u32
    }
}
