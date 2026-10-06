//! Time-window rule: calls are allowed only on configured UTC weekdays and
//! within configured UTC hour ranges.

use crate::{Call, ConfigError, Decision, Reason};

/// Maximum number of hour ranges in one time window.
pub const MAX_RANGES: usize = 4;

/// Weekday bits for [`TimeWindowConfig::day_mask`]. Monday is bit 0.
pub mod days {
    pub const MON: u8 = 1 << 0;
    pub const TUE: u8 = 1 << 1;
    pub const WED: u8 = 1 << 2;
    pub const THU: u8 = 1 << 3;
    pub const FRI: u8 = 1 << 4;
    pub const SAT: u8 = 1 << 5;
    pub const SUN: u8 = 1 << 6;
    pub const WEEKDAYS: u8 = MON | TUE | WED | THU | FRI;
    pub const ALL: u8 = 0x7f;
}

/// A half-open UTC hour range `[start, end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HourRange {
    pub start: u8,
    pub end: u8,
}

impl HourRange {
    fn is_valid(&self) -> bool {
        self.start < 24 && self.end <= 24 && self.start < self.end
    }

    fn contains(&self, hour: u8) -> bool {
        self.start <= hour && hour < self.end
    }
}

/// A validated time window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeWindowConfig {
    day_mask: u8,
    ranges: [HourRange; MAX_RANGES],
    len: u8,
}

impl TimeWindowConfig {
    /// Builds a validated window. Rejects an empty or out-of-range day mask,
    /// no ranges, more than [`MAX_RANGES`] ranges, and any range with
    /// `start >= 24`, `end > 24`, or `start >= end`.
    pub fn new(day_mask: u8, ranges: &[HourRange]) -> Result<Self, ConfigError> {
        if day_mask == 0 {
            return Err(ConfigError::EmptyDayMask);
        }
        if day_mask & !days::ALL != 0 {
            return Err(ConfigError::InvalidDayMask);
        }
        if ranges.is_empty() {
            return Err(ConfigError::NoHourRanges);
        }
        if ranges.len() > MAX_RANGES {
            return Err(ConfigError::TooManyHourRanges);
        }
        if !ranges.iter().all(HourRange::is_valid) {
            return Err(ConfigError::InvalidHourRange);
        }
        let mut stored = [HourRange { start: 0, end: 0 }; MAX_RANGES];
        stored[..ranges.len()].copy_from_slice(ranges);
        Ok(Self {
            day_mask,
            ranges: stored,
            len: ranges.len() as u8,
        })
    }

    pub fn day_mask(&self) -> u8 {
        self.day_mask
    }

    pub fn ranges(&self) -> &[HourRange] {
        &self.ranges[..self.len as usize]
    }

    /// Whether `timestamp` (UNIX seconds) falls inside the window.
    pub fn contains(&self, timestamp: u64) -> bool {
        let (weekday, hour) = weekday_and_hour(timestamp);
        self.day_mask & (1 << weekday) != 0 && self.ranges().iter().any(|r| r.contains(hour))
    }
}

/// Returns the UTC weekday (Monday = 0) and hour (0..24) of a UNIX timestamp.
pub fn weekday_and_hour(timestamp: u64) -> (u8, u8) {
    const DAY: u64 = 86_400;
    let days_since_epoch = timestamp / DAY;
    // 1970-01-01 was a Thursday, which is weekday 3 when Monday is 0.
    let weekday = ((days_since_epoch + 3) % 7) as u8;
    let hour = ((timestamp % DAY) / 3_600) as u8;
    (weekday, hour)
}

/// Allows the call only if its timestamp falls inside the window.
pub fn evaluate(config: &TimeWindowConfig, _state: &(), call: &Call) -> (Decision, ()) {
    (decide(config, call.timestamp), ())
}

/// Contract-creation contexts are subject to the same window as calls.
pub fn evaluate_create_contract(
    config: &TimeWindowConfig,
    _state: &(),
    timestamp: u64,
) -> (Decision, ()) {
    (decide(config, timestamp), ())
}

fn decide(config: &TimeWindowConfig, timestamp: u64) -> Decision {
    if config.contains(timestamp) {
        Decision::Allow
    } else {
        Decision::Deny(Reason::OutsideTimeWindow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // Tuesday 2024-01-02 00:00:00 UTC.
    const TUE_MIDNIGHT: u64 = 1_704_153_600;
    const HOUR: u64 = 3_600;
    const DAY: u64 = 86_400;

    fn at(timestamp: u64) -> Call<'static> {
        Call {
            contract: [0; 32],
            function: "transfer",
            amount: None,
            timestamp,
        }
    }

    fn office_hours() -> TimeWindowConfig {
        TimeWindowConfig::new(days::WEEKDAYS, &[HourRange { start: 8, end: 20 }]).unwrap()
    }

    #[test]
    fn computes_weekday_and_hour() {
        assert_eq!(weekday_and_hour(0), (3, 0)); // Thursday
        assert_eq!(weekday_and_hour(TUE_MIDNIGHT), (1, 0));
        assert_eq!(weekday_and_hour(TUE_MIDNIGHT + 14 * HOUR + 59), (1, 14));
        assert_eq!(weekday_and_hour(TUE_MIDNIGHT + 5 * DAY), (6, 0)); // Sunday
    }

    #[test]
    fn range_is_half_open() {
        let config = office_hours();
        let tue = TUE_MIDNIGHT;
        assert_eq!(
            evaluate(&config, &(), &at(tue + 8 * HOUR - 1)).0,
            Decision::Deny(Reason::OutsideTimeWindow)
        );
        assert_eq!(
            evaluate(&config, &(), &at(tue + 8 * HOUR)).0,
            Decision::Allow
        );
        assert_eq!(
            evaluate(&config, &(), &at(tue + 20 * HOUR - 1)).0,
            Decision::Allow
        );
        assert_eq!(
            evaluate(&config, &(), &at(tue + 20 * HOUR)).0,
            Decision::Deny(Reason::OutsideTimeWindow)
        );
    }

    #[test]
    fn denies_disallowed_days() {
        let config = office_hours();
        let saturday_noon = TUE_MIDNIGHT + 4 * DAY + 12 * HOUR;
        assert_eq!(
            evaluate(&config, &(), &at(saturday_noon)).0,
            Decision::Deny(Reason::OutsideTimeWindow)
        );
    }

    #[test]
    fn multiple_ranges_and_full_day() {
        let config = TimeWindowConfig::new(
            days::ALL,
            &[
                HourRange { start: 0, end: 2 },
                HourRange { start: 22, end: 24 },
            ],
        )
        .unwrap();
        assert!(config.contains(TUE_MIDNIGHT + HOUR));
        assert!(config.contains(TUE_MIDNIGHT + 23 * HOUR));
        assert!(!config.contains(TUE_MIDNIGHT + 12 * HOUR));
        assert_eq!(
            evaluate_create_contract(&config, &(), TUE_MIDNIGHT + 12 * HOUR).0,
            Decision::Deny(Reason::OutsideTimeWindow)
        );
    }

    #[test]
    fn rejects_invalid_configs() {
        let ok = [HourRange { start: 8, end: 20 }];
        assert_eq!(
            TimeWindowConfig::new(0, &ok),
            Err(ConfigError::EmptyDayMask)
        );
        assert_eq!(
            TimeWindowConfig::new(0x80, &ok),
            Err(ConfigError::InvalidDayMask)
        );
        assert_eq!(
            TimeWindowConfig::new(days::ALL, &[]),
            Err(ConfigError::NoHourRanges)
        );
        assert_eq!(
            TimeWindowConfig::new(days::ALL, &[ok[0]; 5]),
            Err(ConfigError::TooManyHourRanges)
        );
        for bad in [(24, 24), (24, 25), (5, 25), (10, 10), (12, 8)] {
            assert_eq!(
                TimeWindowConfig::new(
                    days::ALL,
                    &[HourRange {
                        start: bad.0,
                        end: bad.1
                    }]
                ),
                Err(ConfigError::InvalidHourRange),
                "{bad:?}"
            );
        }
    }

    proptest! {
        #[test]
        fn all_day_every_day_allows_everything(ts in any::<u64>()) {
            let config =
                TimeWindowConfig::new(days::ALL, &[HourRange { start: 0, end: 24 }]).unwrap();
            prop_assert_eq!(evaluate(&config, &(), &at(ts)).0, Decision::Allow);
        }

        #[test]
        fn decision_matches_weekday_and_hour(
            ts in any::<u64>(),
            mask in 1u8..=0x7f,
            start in 0u8..24,
            len in 1u8..=24,
        ) {
            let end = start.saturating_add(len).min(24);
            let config = TimeWindowConfig::new(mask, &[HourRange { start, end }]).unwrap();
            let (weekday, hour) = weekday_and_hour(ts);
            let expected = mask & (1 << weekday) != 0 && start <= hour && hour < end;
            prop_assert_eq!(evaluate(&config, &(), &at(ts)).0.is_allow(), expected);
        }

        #[test]
        fn weekly_periodicity(ts in 0u64..u64::MAX - 7 * DAY) {
            let config = office_hours();
            prop_assert_eq!(config.contains(ts), config.contains(ts + 7 * DAY));
        }
    }
}
