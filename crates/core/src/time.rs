//! Deriving a local calendar date from a UTC instant, without ever storing local time
//! (ADR 0009). Event and file timestamps stay `DateTime<Utc>`, unchanged; this only answers
//! "what day is it, for the human" for stamping (`records.complete`) and, later, the
//! scheduler's daily jobs and the calendar module.
//!
//! Deliberately not a `Clock` method: `Clock` (`clock.rs`) is a load-bearing, pervasively
//! used injectable time source with its own narrow contract ("testable by advancing a fake
//! clock"). Timezone lookup is a config-shaped concern with its own dependency; keeping it
//! here means `Clock` and its many `Clock::fake` call sites are untouched (ADR 0009).

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;

use crate::error::{Error, Result};

/// A resolved IANA timezone. Parsing/validating the name happens once, at load, not per call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalTimezone(Tz);

impl LocalTimezone {
    pub const UTC: LocalTimezone = LocalTimezone(Tz::UTC);

    /// Parses an IANA zone name, e.g. `"America/New_York"`. Errors name the bad value so a
    /// typo in `config.toml` is diagnosable.
    pub fn parse(iana_name: &str) -> Result<Self> {
        iana_name
            .parse::<Tz>()
            .map(LocalTimezone)
            .map_err(|_| Error::invalid_params(format!("unknown IANA timezone '{iana_name}'")))
    }

    /// The zone itself, for working out local times (ADR 0031 §2).
    pub fn tz(&self) -> Tz {
        self.0
    }

    pub fn name(&self) -> &'static str {
        self.0.name()
    }
}

impl Default for LocalTimezone {
    fn default() -> Self {
        Self::UTC
    }
}

/// The calendar date `at` falls on in `tz`. Pure: no I/O, no `Utc::now()`. An instant always
/// maps to exactly one local date, so this has no ambiguous or missing case — unlike going
/// the other direction, local wall-clock to instant, which this deliberately does not do
/// (that is cron scheduling's problem, ADR 0004, still deferred).
pub fn local_date(at: DateTime<Utc>, tz: &LocalTimezone) -> NaiveDate {
    at.with_timezone(&tz.0).date_naive()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn utc_timezone_matches_the_utc_date() {
        let at = Utc.with_ymd_and_hms(2026, 3, 15, 12, 0, 0).unwrap();
        assert_eq!(local_date(at, &LocalTimezone::UTC), at.date_naive());
    }

    #[test]
    fn late_evening_utc_is_still_today_west_of_utc() {
        // 2026-03-16 02:30 UTC is 2026-03-15 22:30 in New York (EDT, UTC-4 in March).
        let at = Utc.with_ymd_and_hms(2026, 3, 16, 2, 30, 0).unwrap();
        let ny = LocalTimezone::parse("America/New_York").unwrap();
        assert_eq!(local_date(at, &ny), NaiveDate::from_ymd_opt(2026, 3, 15).unwrap());
    }

    #[test]
    fn early_morning_utc_is_already_tomorrow_east_of_utc() {
        // 2026-03-15 20:00 UTC is 2026-03-16 05:00 in Tokyo (UTC+9).
        let at = Utc.with_ymd_and_hms(2026, 3, 15, 20, 0, 0).unwrap();
        let tokyo = LocalTimezone::parse("Asia/Tokyo").unwrap();
        assert_eq!(local_date(at, &tokyo), NaiveDate::from_ymd_opt(2026, 3, 16).unwrap());
    }

    #[test]
    fn rejects_unknown_timezone_name() {
        assert!(LocalTimezone::parse("Not/AZone").is_err());
    }
}
