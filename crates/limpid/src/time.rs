//! Typed wall-clock values used by duration telemetry.
//!
//! Queue records persist an absolute wall-clock boundary so delivery latency
//! can span a restart. Keep that representation separate from elapsed
//! durations and convert to floating-point seconds only at the metrics facade.

use chrono::{DateTime, Utc};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct UnixNanos(i64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DurationNanos(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ElapsedNanos {
    pub(crate) duration: DurationNanos,
    pub(crate) reversed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ClockSample {
    pub(crate) utc: DateTime<Utc>,
    pub(crate) unix_nanos: UnixNanos,
}

impl UnixNanos {
    pub(crate) const fn new(value: i64) -> Self {
        Self(value)
    }

    pub(crate) fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    pub(crate) fn from_system_time(value: SystemTime) -> Self {
        let nanos = match value.duration_since(UNIX_EPOCH) {
            Ok(duration) => i128::try_from(duration.as_nanos()).unwrap_or(i128::MAX),
            Err(error) => -i128::try_from(error.duration().as_nanos()).unwrap_or(i128::MAX),
        };
        Self(nanos.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
    }

    pub(crate) fn from_datetime(value: DateTime<Utc>) -> Self {
        Self(value.timestamp_nanos_opt().unwrap_or_else(|| {
            if value.timestamp() < 0 {
                i64::MIN
            } else {
                i64::MAX
            }
        }))
    }

    pub(crate) const fn get(self) -> i64 {
        self.0
    }

    pub(crate) const fn to_datetime(self) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp_nanos(self.0)
    }

    pub(crate) fn to_wire_u64(self) -> u64 {
        u64::try_from(self.0).unwrap_or(0)
    }

    pub(crate) fn elapsed_since(self, earlier: Self) -> ElapsedNanos {
        let delta = self.0 as i128 - earlier.0 as i128;
        if delta < 0 {
            ElapsedNanos {
                duration: DurationNanos(0),
                reversed: true,
            }
        } else {
            ElapsedNanos {
                duration: DurationNanos(delta as u64),
                reversed: false,
            }
        }
    }
}

impl DurationNanos {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) fn as_seconds_f64(self) -> f64 {
        self.0 as f64 / 1_000_000_000.0
    }
}

impl ElapsedNanos {
    pub(crate) fn between_u64(later: u64, earlier: u64) -> Self {
        match later.checked_sub(earlier) {
            Some(duration) => Self {
                duration: DurationNanos(duration),
                reversed: false,
            },
            None => Self {
                duration: DurationNanos(0),
                reversed: true,
            },
        }
    }
}

impl ClockSample {
    pub(crate) fn now() -> Self {
        let unix_nanos = UnixNanos::now();
        Self {
            utc: unix_nanos.to_datetime(),
            unix_nanos,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_datetime(utc: DateTime<Utc>) -> Self {
        Self {
            unix_nanos: UnixNanos::from_datetime(utc),
            utc,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_time_is_total_and_elapsed_clamps_reversed_clocks() {
        let before_epoch = UNIX_EPOCH - std::time::Duration::from_nanos(7);
        let sample = UnixNanos::from_system_time(before_epoch);
        assert_eq!(sample.get(), -7);
        assert_eq!(sample.to_wire_u64(), 0);

        let reversed = UnixNanos::new(4).elapsed_since(UnixNanos::new(9));
        assert_eq!(reversed.duration, DurationNanos::new(0));
        assert!(reversed.reversed);

        let wire_reversed = ElapsedNanos::between_u64(i64::MAX as u64, u64::MAX);
        assert_eq!(wire_reversed.duration, DurationNanos::new(0));
        assert!(wire_reversed.reversed);
    }
}
