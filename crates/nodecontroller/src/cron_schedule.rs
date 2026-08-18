//! A minimal, from-scratch standard 5-field cron expression parser and
//! next-run calculator — `cronjob-controller`'s only real new primitive.
//! Written independently rather than pulled in as a dependency, the same
//! call this crate already made for FNV template hashing and taint/
//! toleration matching: the surface needed here (parse once per CronJob,
//! ask "what's the next run after this instant") is small and worth
//! keeping dependency-free and unit-testable in isolation.
//!
//! # Scope
//!
//! Standard 5-field format only (`minute hour day-of-month month
//! day-of-week`) — no seconds field, no `@hourly`/`@daily`-style macros,
//! no named months/weekdays (`JAN`, `MON`). Supports `*`, a bare number,
//! `*/N`, `N/step`, and `a-b/N` step syntax, `a-b` ranges, and comma-separated lists —
//! covers the overwhelming majority of real CronJob schedules. Matches
//! upstream's own "day-of-month OR day-of-week" quirk when both fields are
//! restricted (not `*`): a real, well-known cron semantic, not a bug.
//!
//! `next_after` searches minute-by-minute up to one year ahead and returns
//! `None` if nothing matches in that window (a schedule that can genuinely
//! never fire, e.g. `0 0 30 2 *` — Feb 30th) rather than looping forever.

use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    minute: Vec<u32>,
    hour: Vec<u32>,
    day_of_month: Vec<u32>,
    month: Vec<u32>,
    day_of_week: Vec<u32>,
    dom_restricted: bool,
    dow_restricted: bool,
}

fn parse_field(field: &str, min: u32, max: u32) -> Result<(Vec<u32>, bool), String> {
    if field == "*" {
        return Ok(((min..=max).collect(), false));
    }
    let mut values = Vec::new();
    for part in field.split(',') {
        let (range_part, step, explicit_step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<u32>()
                    .map_err(|_| format!("bad step in {part:?}"))?,
                true,
            ),
            None => (part, 1, false),
        };
        let (lo, hi) = if range_part == "*" {
            (min, max)
        } else if let Some((a, b)) = range_part.split_once('-') {
            let a: u32 = a
                .parse()
                .map_err(|_| format!("bad range start in {part:?}"))?;
            let b: u32 = b
                .parse()
                .map_err(|_| format!("bad range end in {part:?}"))?;
            (a, b)
        } else if explicit_step {
            let v: u32 = range_part
                .parse()
                .map_err(|_| format!("bad value {part:?}"))?;
            (v, max)
        } else {
            let v: u32 = range_part
                .parse()
                .map_err(|_| format!("bad value {part:?}"))?;
            (v, v)
        };
        if lo < min || hi > max || lo > hi {
            return Err(format!("field {field:?} out of range {min}-{max}"));
        }
        let mut v = lo;
        while v <= hi {
            values.push(v);
            v += step.max(1);
        }
    }
    values.sort_unstable();
    values.dedup();
    Ok((values, true))
}

impl Schedule {
    pub fn parse(expr: &str) -> Result<Schedule, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "expected 5 fields, got {} in {expr:?}",
                fields.len()
            ));
        }
        let (minute, _) = parse_field(fields[0], 0, 59)?;
        let (hour, _) = parse_field(fields[1], 0, 23)?;
        let (day_of_month, dom_restricted) = parse_field(fields[2], 1, 31)?;
        let (month, _) = parse_field(fields[3], 1, 12)?;
        let (day_of_week, dow_restricted) = parse_field(fields[4], 0, 6)?;
        if minute.is_empty()
            || hour.is_empty()
            || day_of_month.is_empty()
            || month.is_empty()
            || day_of_week.is_empty()
        {
            return Err(format!("a field in {expr:?} matched no values"));
        }
        Ok(Schedule {
            minute,
            hour,
            day_of_month,
            month,
            day_of_week,
            dom_restricted,
            dow_restricted,
        })
    }

    fn matches(&self, t: &DateTime<Utc>) -> bool {
        if !self.minute.contains(&t.minute())
            || !self.hour.contains(&t.hour())
            || !self.month.contains(&t.month())
        {
            return false;
        }
        let dom_ok = self.day_of_month.contains(&t.day());
        let dow = t.weekday().num_days_from_sunday();
        let dow_ok = self.day_of_week.contains(&dow);
        // Upstream cron quirk: when BOTH day-of-month and day-of-week are
        // restricted, either matching is enough (OR, not AND). When only
        // one (or neither) is restricted, the restricted one alone decides
        // (an unrestricted field is trivially "everything", contributing
        // nothing to the decision).
        if self.dom_restricted && self.dow_restricted {
            dom_ok || dow_ok
        } else {
            dom_ok && dow_ok
        }
    }

    /// The next matching minute strictly after `from`, searching up to one
    /// year ahead. `from`'s own seconds/sub-second are dropped (cron has
    /// minute granularity); the search itself always starts at least one
    /// minute after `from`, so a `from` that already matches never returns
    /// itself.
    pub fn next_after(&self, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let start = Utc
            .with_ymd_and_hms(
                from.year(),
                from.month(),
                from.day(),
                from.hour(),
                from.minute(),
                0,
            )
            .single()?
            + Duration::minutes(1);
        let limit = start + Duration::days(366);
        let mut t = start;
        while t < limit {
            if self.matches(&t) {
                return Some(t);
            }
            t += Duration::minutes(1);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn every_minute() {
        let s = Schedule::parse("* * * * *").unwrap();
        assert_eq!(
            s.next_after(dt(2026, 1, 1, 0, 0)),
            Some(dt(2026, 1, 1, 0, 1))
        );
    }

    #[test]
    fn specific_time_next_day() {
        let s = Schedule::parse("30 4 * * *").unwrap();
        assert_eq!(
            s.next_after(dt(2026, 1, 1, 5, 0)),
            Some(dt(2026, 1, 2, 4, 30))
        );
        assert_eq!(
            s.next_after(dt(2026, 1, 1, 4, 0)),
            Some(dt(2026, 1, 1, 4, 30))
        );
    }

    #[test]
    fn step_syntax() {
        let s = Schedule::parse("*/15 * * * *").unwrap();
        assert_eq!(
            s.next_after(dt(2026, 1, 1, 0, 1)),
            Some(dt(2026, 1, 1, 0, 15))
        );
    }

    #[test]
    fn explicit_start_step_runs_through_the_field_maximum() {
        let s = Schedule::parse("0 0/6 * * *").unwrap();
        assert_eq!(
            s.next_after(dt(2026, 1, 1, 0, 30)),
            Some(dt(2026, 1, 1, 6, 0))
        );
    }

    #[test]
    fn list_and_range() {
        let s = Schedule::parse("0 9-17 * * 1-5").unwrap();
        // 2026-01-01 is a Thursday (weekday 4).
        assert_eq!(
            s.next_after(dt(2026, 1, 1, 8, 59)),
            Some(dt(2026, 1, 1, 9, 0))
        );
    }

    #[test]
    fn dom_and_dow_both_restricted_is_an_or() {
        // 13th-of-the-month OR Friday, at midnight.
        let s = Schedule::parse("0 0 13 * 5").unwrap();
        // 2026-01-02 is a Friday.
        assert_eq!(
            s.next_after(dt(2026, 1, 1, 0, 0)),
            Some(dt(2026, 1, 2, 0, 0))
        );
    }

    #[test]
    fn rejects_malformed_expressions() {
        assert!(Schedule::parse("* * * *").is_err()); // only 4 fields
        assert!(Schedule::parse("60 * * * *").is_err()); // minute out of range
    }

    #[test]
    fn a_never_matching_schedule_returns_none_within_the_search_window() {
        let s = Schedule::parse("0 0 30 2 *").unwrap(); // Feb 30th never exists
        assert_eq!(s.next_after(dt(2026, 1, 1, 0, 0)), None);
    }
}
