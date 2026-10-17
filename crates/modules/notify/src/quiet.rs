//! Quiet hours (ADR 0032 §4): `"22:00-08:00"`, every day or per day, in local time. Pure: the
//! caller turns now into local wall-clock time.

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDateTime, NaiveTime, Weekday};

/// One range, possibly across midnight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    start: NaiveTime,
    end: NaiveTime,
}

impl Range {
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = || format!("'{text}' isn't a range of times, like \"22:00-08:00\"");
        let (a, b) = text.split_once('-').ok_or_else(bad)?;
        let time = |t: &str| NaiveTime::parse_from_str(t.trim(), "%H:%M").map_err(|_| bad());
        let (start, end) = (time(a)?, time(b)?);
        if start == end {
            return Err(format!("'{text}' starts and ends at the same time"));
        }
        Ok(Self { start, end })
    }

    fn crosses_midnight(&self) -> bool {
        self.start > self.end
    }
}

/// Which day words a quiet-hours key may be: `every_day`, `weekdays`, `weekends`, or one day.
pub const KEYS: [&str; 10] = ["every_day", "weekdays", "weekends", "mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// The `[quiet]` settings: day word → range.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Quiet(BTreeMap<String, Range>);

impl Quiet {
    /// From `[quiet]`'s text values, every problem named.
    pub fn parse(entries: &BTreeMap<String, String>) -> Result<Self, String> {
        let mut out = BTreeMap::new();
        for (key, text) in entries {
            if !KEYS.contains(&key.as_str()) {
                return Err(format!("quiet: '{key}' isn't a day word (every_day, weekdays, weekends, mon…sun)"));
            }
            out.insert(key.clone(), Range::parse(text).map_err(|e| format!("quiet.{key}: {e}"))?);
        }
        Ok(Self(out))
    }

    /// The range for one day: the day's own, then weekdays/weekends, then every_day.
    fn for_day(&self, day: Weekday) -> Option<Range> {
        let own = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"][day.num_days_from_monday() as usize];
        let group = if matches!(day, Weekday::Sat | Weekday::Sun) { "weekends" } else { "weekdays" };
        [own, group, "every_day"].iter().find_map(|k| self.0.get(*k).copied())
    }

    /// Whether `at` (local) is quiet. A range belongs to the day it starts on: Friday's
    /// `01:00-10:00` is Friday morning; Friday's `22:00-08:00` runs into Saturday.
    pub fn contains(&self, at: NaiveDateTime) -> bool {
        let (day, t) = (at.weekday(), at.time());
        let today = self.for_day(day).is_some_and(|r| r.start <= t && (r.crosses_midnight() || t < r.end));
        let from_yesterday = self.for_day(day.pred()).is_some_and(|r| r.crosses_midnight() && t < r.end);
        today || from_yesterday
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet(pairs: &[(&str, &str)]) -> Quiet {
        Quiet::parse(&pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()).unwrap()
    }

    /// 2026-10-16 is a Friday.
    fn at(day: u32, time: &str) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 10, day)
            .unwrap()
            .and_time(NaiveTime::parse_from_str(time, "%H:%M").unwrap())
    }

    #[test]
    fn every_day_across_midnight() {
        let q = quiet(&[("every_day", "22:00-08:00")]);
        for (d, t, want) in
            [(16, "21:59", false), (16, "22:00", true), (17, "03:00", true), (17, "07:59", true), (17, "08:00", false)]
        {
            assert_eq!(q.contains(at(d, t)), want, "{d} {t}");
        }
        let lunch = quiet(&[("every_day", "12:00 - 13:30")]);
        assert!(lunch.contains(at(16, "12:45")) && !lunch.contains(at(16, "13:30")));
    }

    #[test]
    fn a_days_own_hours_win_and_belong_to_the_day_they_start() {
        let q = quiet(&[("every_day", "22:00-07:00"), ("weekends", "00:00-10:00"), ("fri", "23:30-09:00")]);
        assert!(!q.contains(at(16, "22:30")), "Friday's own starts at 23:30");
        assert!(q.contains(at(16, "23:45")));
        assert!(q.contains(at(17, "08:30")), "Friday night's range runs into Saturday morning");
        assert!(q.contains(at(17, "09:30")), "and Saturday's own (weekends) until 10:00");
        assert!(!q.contains(at(17, "10:00")));
        assert!(
            q.contains(at(15, "06:30")) && !q.contains(at(15, "07:00")),
            "Thursday morning, from Wednesday's every_day"
        );
    }

    #[test]
    fn a_bad_setting_says_what_it_should_be() {
        let bad = |k: &str, v: &str| Quiet::parse(&[(k.to_string(), v.to_string())].into_iter().collect()).unwrap_err();
        assert!(bad("every_day", "10pm-8am").contains("like \"22:00-08:00\""));
        assert!(bad("every_day", "08:00-08:00").contains("same time"));
        assert!(bad("someday", "22:00-08:00").contains("isn't a day word"));
    }
}
