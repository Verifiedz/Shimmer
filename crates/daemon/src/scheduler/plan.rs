//! When does a trigger fire? Pure functions over plain data, no runtime, no I/O (§12 rule 10).
//!
//! All times are UTC, except a `CronLocal` expression, which is read in the configured local
//! timezone (ADR 0031 §2). Cron expressions are standard 5-field (`min hour dom month dow`,
//! Sunday = 0 or 7, Monday = 1) or 6-field with leading seconds.

use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, LocalResult, NaiveDateTime, TimeZone, Utc};
use shimmer_core::{CatchUp, Error, LocalTimezone, Result, Schedule};

/// A firing later than this after its scheduled time counts as *missed*, not on time. Wide
/// enough that a busy poll never turns a normal firing into a "catch-up".
pub const ON_TIME_GRACE: Duration = Duration::from_secs(60);
/// Most occurrences examined in one catch-up. Beyond it the rest are counted as missed.
pub const MAX_SCAN: usize = 10_000;
/// Most firings a single `Backfill` may produce in one go.
pub const MAX_BACKFILL: usize = 1_000;

/// A validated schedule, ready to ask "what comes next?".
pub struct Parsed(Kind);

enum Kind {
    Cron(Box<cron::Schedule>),
    CronLocal(Box<cron::Schedule>, LocalTimezone),
    Every(chrono::Duration),
    Once(DateTime<Utc>),
}

impl Parsed {
    /// `local` is the configured timezone, used only by a `CronLocal` schedule.
    pub fn parse(schedule: &Schedule, local: LocalTimezone) -> Result<Self> {
        Ok(Self(match schedule {
            Schedule::Cron(expr) => Kind::Cron(Box::new(parse_cron(expr)?)),
            Schedule::CronLocal(expr) => Kind::CronLocal(Box::new(parse_cron(expr)?), local),
            Schedule::Every(d) => {
                if d.is_zero() {
                    return Err(Error::invalid_params("'every' must be at least one second"));
                }
                let d = chrono::Duration::from_std(*d).map_err(|_| Error::invalid_params("'every' is too large"))?;
                Kind::Every(d)
            }
            Schedule::Once(t) => Kind::Once(*t),
        }))
    }

    /// First occurrence strictly after `after`, if any. A `Once` has exactly one.
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match &self.0 {
            Kind::Cron(c) => c.after(&after).next(),
            Kind::CronLocal(c, tz) => next_local(c, *tz, after),
            Kind::Every(d) => after.checked_add_signed(*d),
            Kind::Once(t) => (*t > after).then_some(*t),
        }
    }

    /// Read in the local timezone, so its next run moves when that changes.
    pub fn is_local(&self) -> bool {
        matches!(self.0, Kind::CronLocal(..))
    }

    /// Where a local trigger's next run goes when it was computed in zone `was` and the
    /// configured zone is now another (ADR 0031 §2): the same wall-clock time in the new zone, so a
    /// run that fell due while the daemon was down stays due and catch-up still sees it. Never at or
    /// before `last_run` (then the next occurrence after it). Anything but a `CronLocal`, or the
    /// same zone, keeps `next_due` as it is: recomputing from now would hide missed runs.
    pub fn rezoned(
        &self,
        next_due: DateTime<Utc>,
        was: LocalTimezone,
        last_run: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let Kind::CronLocal(_, now_tz) = &self.0 else { return Some(next_due) };
        if *now_tz == was {
            return Some(next_due);
        }
        let wall = next_due.with_timezone(&was.tz()).naive_local();
        let tz = now_tz.tz();
        let at = match tz.from_local_datetime(&wall) {
            LocalResult::Single(t) => t,
            LocalResult::Ambiguous(first, _) => first,
            LocalResult::None => first_existing_after(&tz, wall)?,
        }
        .with_timezone(&Utc);
        match last_run {
            Some(last) if at <= last => self.next_after(last),
            _ => Some(at),
        }
    }

    /// Where a trigger created at `now` first becomes due. `Every` counts from creation; a
    /// `Once` is due at its own time even if that is already past (`catch_up` then decides).
    pub fn first_due(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match &self.0 {
            Kind::Once(t) => Some(*t),
            _ => self.next_after(now),
        }
    }
}

/// The first local-time occurrence of `c` strictly after `after` (ADR 0031 §2). The expression is
/// matched against wall-clock times, then each is placed on the real timeline:
/// - a time the clocks skip (spring forward) runs at the next minute that exists, once: the
///   occurrences inside the gap all land there, and the next search starts after it;
/// - a time the clocks repeat (fall back) runs at the first of the two, once: the wall clock is
///   walked forward, so the repeated hour is never matched again.
fn next_local(c: &cron::Schedule, tz: LocalTimezone, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let tz = tz.tz();
    let mut wall = after.with_timezone(&tz).naive_local();
    for _ in 0..MAX_SCAN {
        // The `cron` crate walks UTC, which has no clock changes: use it as a plain wall clock.
        let next = c.after(&Utc.from_utc_datetime(&wall)).next()?.naive_utc();
        let at = match tz.from_local_datetime(&next) {
            LocalResult::Single(t) => t,
            LocalResult::Ambiguous(first, _) => first,
            LocalResult::None => first_existing_after(&tz, next)?,
        }
        .with_timezone(&Utc);
        if at > after {
            return Some(at);
        }
        wall = next;
    }
    None
}

/// The first wall-clock minute after `gap` that exists in `tz` (a clock change skips at most a
/// couple of hours).
fn first_existing_after<Z: TimeZone>(tz: &Z, gap: NaiveDateTime) -> Option<DateTime<Z>> {
    (1..=180).find_map(|m| tz.from_local_datetime(&(gap + chrono::Duration::minutes(m))).earliest())
}

/// Standard cron -> the `cron` crate. That crate numbers weekdays 1-7 from Sunday and rejects
/// 0, so a literal `1` would run on Sundays. Translate numeric weekdays to names.
fn parse_cron(expr: &str) -> Result<cron::Schedule> {
    let mut fields: Vec<String> = expr.split_whitespace().map(str::to_owned).collect();
    match fields.len() {
        5 => fields.insert(0, "0".into()),
        6 => {}
        n => return Err(Error::invalid_params(format!("cron needs 5 or 6 fields, got {n}: '{expr}'"))),
    }
    fields[5] = weekday_names(&fields[5]);
    cron::Schedule::from_str(&fields.join(" "))
        .map_err(|e| Error::invalid_params(format!("invalid cron '{expr}': {e}")))
}

fn weekday_names(field: &str) -> String {
    const NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let mut out = String::new();
    let mut digits = String::new();
    let mut after_step = false;
    let flush = |digits: &mut String, out: &mut String, after_step: bool| {
        if digits.is_empty() {
            return;
        }
        match digits.parse::<usize>() {
            // A step size ("1-5/2") is a count, not a weekday.
            Ok(n) if !after_step && n <= 7 => out.push_str(NAMES[n % 7]),
            _ => out.push_str(digits),
        }
        digits.clear();
    };
    for c in field.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else {
            flush(&mut digits, &mut out, after_step);
            after_step = c == '/';
            out.push(c);
        }
    }
    flush(&mut digits, &mut out, after_step);
    out
}

#[derive(Debug, PartialEq, Eq)]
pub struct Decision {
    /// Scheduled times to fire now, oldest first.
    pub fire: Vec<DateTime<Utc>>,
    /// Due occurrences dropped by the catch-up policy.
    pub missed: usize,
    /// The new `next_due`; `None` means the trigger is finished (a `Once`).
    pub next_due: Option<DateTime<Utc>>,
}

/// Given a trigger whose `next_due` has passed, decide what fires (§6.3).
///
/// * `Skip`: only an on-time firing runs; anything missed is dropped.
/// * `RunOnce`: one firing, however many were missed, stamped with the latest occurrence.
/// * `Backfill`: one firing per occurrence, each stamped with its own historical time.
pub fn decide(schedule: &Parsed, catch_up: CatchUp, next_due: DateTime<Utc>, now: DateTime<Utc>) -> Decision {
    if next_due > now {
        return Decision { fire: vec![], missed: 0, next_due: Some(next_due) };
    }
    let mut occurrences = Vec::new();
    let mut cursor = Some(next_due);
    let mut truncated = false;
    while let Some(t) = cursor.filter(|t| *t <= now) {
        if occurrences.len() == MAX_SCAN {
            truncated = true;
            break;
        }
        occurrences.push(t);
        cursor = schedule.next_after(t);
    }
    // After a truncated scan `cursor` is still in the past; jump to the first future one.
    let next_due = if truncated { schedule.next_after(now) } else { cursor };
    let dropped_by_scan = if truncated { 1 } else { 0 }; // "at least one more": exact count is unknowable cheaply

    let grace = chrono::Duration::from_std(ON_TIME_GRACE).unwrap_or(chrono::Duration::MAX);
    let total = occurrences.len();
    let fire: Vec<_> = match catch_up {
        CatchUp::Skip => occurrences.iter().copied().filter(|t| now - *t <= grace).collect(),
        CatchUp::RunOnce => occurrences.last().copied().into_iter().collect(),
        CatchUp::Backfill => occurrences.iter().copied().take(MAX_BACKFILL).collect(),
    };
    Decision { missed: total - fire.len() + dropped_by_scan, fire, next_due }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    impl Parsed {
        fn parse_utc(schedule: &Schedule) -> Result<Self> {
            Self::parse(schedule, LocalTimezone::UTC)
        }
    }

    fn new_york(expr: &str) -> Parsed {
        Parsed::parse(&Schedule::CronLocal(expr.into()), LocalTimezone::parse("America/New_York").unwrap()).unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn a_new_zone_keeps_the_wall_clock_time_so_a_missed_run_stays_missed() {
        let ny = LocalTimezone::parse("America/New_York").unwrap();
        let london =
            Parsed::parse(&Schedule::CronLocal("0 9 * * *".into()), LocalTimezone::parse("Europe/London").unwrap())
                .unwrap();
        // Due Monday 09:00 in New York (13:00 UTC); in London that's Monday 09:00 there (08:00 UTC),
        // earlier on the timeline: still in the past for a daemon starting later on Monday.
        let due = utc("2026-10-12T13:00:00Z");
        assert_eq!(london.rezoned(due, ny, None), Some(utc("2026-10-12T08:00:00Z")));
        // Never at or before the last run: then the next one after it.
        assert_eq!(london.rezoned(due, ny, Some(utc("2026-10-12T09:00:00Z"))), Some(utc("2026-10-13T08:00:00Z")));
        // The same zone keeps it exactly; so does anything that isn't local.
        assert_eq!(new_york("0 9 * * *").rezoned(due, ny, None), Some(due));
        let utc_cron = Parsed::parse_utc(&Schedule::Cron("0 9 * * *".into())).unwrap();
        assert_eq!(utc_cron.rezoned(due, ny, None), Some(due));
    }

    #[test]
    fn a_new_zone_moves_a_skipped_time_to_the_next_minute_that_exists() {
        // 02:30 every day, last computed in UTC; New York skips 02:00–03:00 on 2027-03-14.
        let utc_zone = LocalTimezone::UTC;
        let s = new_york("30 2 * * *");
        assert_eq!(
            s.rezoned(utc("2027-03-14T02:30:00Z"), utc_zone, None),
            Some(utc("2027-03-14T07:00:00Z")),
            "03:00 EDT"
        );
    }

    #[test]
    fn local_cron_stays_at_the_same_local_time_across_a_clock_change() {
        // 9am weekdays in New York: 14:00 UTC in winter (EST), 13:00 UTC in summer (EDT).
        let s = new_york("0 9 * * 1-5");
        assert_eq!(
            s.next_after(utc("2026-03-06T15:00:00Z")),
            Some(utc("2026-03-09T13:00:00Z")),
            "Monday after the change"
        );
        assert_eq!(s.next_after(utc("2026-03-05T15:00:00Z")), Some(utc("2026-03-06T14:00:00Z")), "Friday before it");
        assert!(s.is_local());
    }

    #[test]
    fn a_time_the_clocks_skip_runs_once_at_the_next_minute_that_exists() {
        // 2026-03-08 in New York: 02:00 EST jumps to 03:00 EDT (07:00 UTC), so 02:30 never happens.
        let s = new_york("30 2 * * *");
        assert_eq!(s.next_after(utc("2026-03-08T05:00:00Z")), Some(utc("2026-03-08T07:00:00Z")));
        assert_eq!(
            s.next_after(utc("2026-03-08T07:00:00Z")),
            Some(utc("2026-03-09T06:30:00Z")),
            "02:30 EDT the next day"
        );
        // Every quarter-hour inside the gap: once at 03:00, not four times.
        let q = new_york("*/15 2 * * *");
        assert_eq!(q.next_after(utc("2026-03-08T06:59:00Z")), Some(utc("2026-03-08T07:00:00Z")));
        assert_eq!(
            q.next_after(utc("2026-03-08T07:00:00Z")),
            Some(utc("2026-03-09T06:00:00Z")),
            "then 02:00 the next day"
        );
    }

    #[test]
    fn a_time_the_clocks_repeat_runs_once_at_the_first() {
        // 2026-11-01 in New York: 01:00-02:00 happens twice, first EDT (05:00-06:00 UTC), then EST.
        let s = new_york("30 1 * * *");
        assert_eq!(s.next_after(utc("2026-11-01T04:00:00Z")), Some(utc("2026-11-01T05:30:00Z")), "the first 01:30");
        assert_eq!(
            s.next_after(utc("2026-11-01T05:30:00Z")),
            Some(utc("2026-11-02T06:30:00Z")),
            "not the second 01:30"
        );
        // Asked from inside the repeated hour (EST), it doesn't go back to the first one either.
        assert_eq!(s.next_after(utc("2026-11-01T06:10:00Z")), Some(utc("2026-11-02T06:30:00Z")));
    }

    #[test]
    fn utc_cron_is_unchanged() {
        let s = Parsed::parse(&Schedule::Cron("0 9 * * 1-5".into()), LocalTimezone::parse("America/New_York").unwrap())
            .unwrap();
        assert_eq!(s.next_after(utc("2026-03-08T00:00:00Z")), Some(utc("2026-03-09T09:00:00Z")), "the zone is ignored");
        assert!(!s.is_local());
    }

    fn at(d: u32, h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, d, h, m, s).unwrap()
    }

    fn daily() -> Parsed {
        Parsed::parse_utc(&Schedule::Every(Duration::from_secs(86_400))).unwrap()
    }

    #[test]
    fn asleep_three_days_then_each_policy_does_what_it_says() {
        // Due Fri 09:00, machine wakes Mon 12:00: Fri, Sat, Sun and Mon 09:00 all passed.
        let due = at(18, 9, 0, 0);
        let now = at(21, 12, 0, 0);
        let s = daily();

        let skip = decide(&s, CatchUp::Skip, due, now);
        assert_eq!((skip.fire.len(), skip.missed), (0, 4));

        let once = decide(&s, CatchUp::RunOnce, due, now);
        assert_eq!(once.fire, [at(21, 9, 0, 0)], "one firing, stamped with the latest occurrence");
        assert_eq!(once.missed, 3);

        let fill = decide(&s, CatchUp::Backfill, due, now);
        assert_eq!(fill.fire, [at(18, 9, 0, 0), at(19, 9, 0, 0), at(20, 9, 0, 0), at(21, 9, 0, 0)]);
        assert_eq!(fill.missed, 0);

        for d in [&skip, &once, &fill] {
            assert_eq!(d.next_due, Some(at(22, 9, 0, 0)), "next_due always lands in the future");
        }
    }

    #[test]
    fn a_firing_within_grace_is_on_time_even_for_skip() {
        let d = decide(&daily(), CatchUp::Skip, at(21, 9, 0, 0), at(21, 9, 0, 30));
        assert_eq!(d.fire, [at(21, 9, 0, 0)]);
        assert_eq!(d.missed, 0);
        let late = decide(&daily(), CatchUp::Skip, at(21, 9, 0, 0), at(21, 9, 2, 0));
        assert!(late.fire.is_empty() && late.missed == 1);
    }

    #[test]
    fn not_yet_due_changes_nothing() {
        let d = decide(&daily(), CatchUp::Backfill, at(22, 9, 0, 0), at(21, 9, 0, 0));
        assert_eq!(d, Decision { fire: vec![], missed: 0, next_due: Some(at(22, 9, 0, 0)) });
    }

    #[test]
    fn every_does_not_drift_with_late_wakeups() {
        let d = decide(&daily(), CatchUp::RunOnce, at(21, 9, 0, 0), at(21, 17, 45, 0));
        assert_eq!(d.next_due, Some(at(22, 9, 0, 0)), "anchored to the schedule, not to when we woke");
    }

    #[test]
    fn once_fires_then_finishes() {
        let s = Parsed::parse_utc(&Schedule::Once(at(24, 9, 0, 0))).unwrap();
        assert_eq!(s.first_due(at(21, 0, 0, 0)), Some(at(24, 9, 0, 0)));
        let d = decide(&s, CatchUp::Skip, at(24, 9, 0, 0), at(24, 9, 0, 5));
        assert_eq!((d.fire, d.next_due), (vec![at(24, 9, 0, 0)], None));
        // Already past by days when first seen: skip drops it, and it is finished either way.
        let d = decide(&s, CatchUp::Skip, at(24, 9, 0, 0), at(27, 9, 0, 0));
        assert_eq!((d.fire.len(), d.missed, d.next_due), (0, 1, None));
        let d = decide(&s, CatchUp::RunOnce, at(24, 9, 0, 0), at(27, 9, 0, 0));
        assert_eq!((d.fire, d.next_due), (vec![at(24, 9, 0, 0)], None));
    }

    #[test]
    fn runaway_catch_up_is_bounded() {
        let s = Parsed::parse_utc(&Schedule::Every(Duration::from_secs(1))).unwrap();
        let now = at(21, 0, 0, 0);
        let due = now - chrono::Duration::days(30);
        let d = decide(&s, CatchUp::Backfill, due, now);
        assert_eq!(d.fire.len(), MAX_BACKFILL);
        assert!(d.missed > 0);
        assert!(d.next_due.unwrap() > now);
    }

    #[test]
    fn cron_weekdays_mean_what_standard_cron_means() {
        let monday_9 = |expr: &str| {
            let s = Parsed::parse_utc(&Schedule::Cron(expr.into())).unwrap();
            s.next_after(at(21, 12, 0, 0)).unwrap() // Mon 21 Sep 2026, after 09:00
        };
        // 1 = Monday. The `cron` crate alone would say Sunday.
        assert_eq!(monday_9("0 9 * * 1"), at(28, 9, 0, 0));
        assert_eq!(monday_9("0 9 * * Mon"), at(28, 9, 0, 0));
        assert_eq!(monday_9("0 9 * * 0"), at(27, 9, 0, 0), "0 = Sunday");
        assert_eq!(monday_9("0 9 * * 7"), at(27, 9, 0, 0), "7 = Sunday");
        assert_eq!(monday_9("0 9 * * 1-5"), at(22, 9, 0, 0), "Tuesday is the next weekday");
        assert_eq!(monday_9("0 9 * * 6,0"), at(26, 9, 0, 0), "Saturday");
        assert_eq!(monday_9("0 0 9 * * 1"), at(28, 9, 0, 0), "6-field form too");
        assert_eq!(monday_9("*/30 * * * *"), at(21, 12, 30, 0));
    }

    #[test]
    fn weekday_translation_leaves_step_sizes_alone() {
        assert_eq!(weekday_names("1-5/2"), "Mon-Fri/2");
        assert_eq!(weekday_names("*/2"), "*/2");
        assert_eq!(weekday_names("*"), "*");
        assert_eq!(weekday_names("1,3,5"), "Mon,Wed,Fri");
    }

    #[test]
    fn bad_schedules_are_rejected() {
        for bad in ["", "* * *", "61 * * * *", "0 9 * * banana", "0 0 0 0 0 0 0 0"] {
            assert!(Parsed::parse_utc(&Schedule::Cron(bad.into())).is_err(), "{bad:?}");
        }
        assert!(Parsed::parse_utc(&Schedule::Every(Duration::ZERO)).is_err());
    }
}
