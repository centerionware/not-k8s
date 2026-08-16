//! Conversion between k8s-openapi's `Time` (a `jiff::Timestamp` newtype in
//! the `k8s-openapi` version this workspace pins — no `chrono` Cargo
//! feature is enabled for it) and `chrono::DateTime<Utc>`, which
//! `cron_schedule.rs`'s calendar/weekday arithmetic is written against
//! because chrono makes that easy. Unix-second precision only —
//! sub-second precision is never load-bearing anywhere in this crate
//! (cron is minute-granularity, TTL/deadline math is second-granularity).

use chrono::{DateTime, TimeZone, Utc};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

pub fn to_chrono(t: &Time) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(t.0.as_second(), 0).single()
}

pub fn from_chrono(dt: DateTime<Utc>) -> Time {
    Time(k8s_openapi::jiff::Timestamp::from_second(dt.timestamp()).unwrap_or(k8s_openapi::jiff::Timestamp::UNIX_EPOCH))
}

pub fn now() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_k8s_time() {
        let dt = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let t = from_chrono(dt);
        assert_eq!(to_chrono(&t), Some(dt));
    }
}
