//! token_needs_refresh(): issue #554's gate on re-minting a projected
//! serviceAccountToken via a fresh TokenRequest call, instead of leaving an
//! already-fresh-enough token file alone.
use super::*;
use std::time::{Duration, SystemTime};

#[test]
fn a_missing_file_always_needs_a_mint() {
    assert!(token_needs_refresh(None, SystemTime::now(), None));
}

#[test]
fn a_freshly_written_token_does_not_need_a_mint() {
    let now = SystemTime::now();
    assert!(!token_needs_refresh(Some(now), now, None));
}

#[test]
fn a_token_well_within_its_default_ttl_does_not_need_a_mint() {
    let now = SystemTime::now();
    let mtime = now - Duration::from_secs(60); // 1 minute old, default TTL 3600s
    assert!(!token_needs_refresh(Some(mtime), now, None));
}

#[test]
fn a_token_past_eighty_percent_of_its_default_ttl_needs_a_mint() {
    let now = SystemTime::now();
    let mtime = now - Duration::from_secs(2881); // just past 80% of 3600s
    assert!(token_needs_refresh(Some(mtime), now, None));
}

#[test]
fn expiration_seconds_overrides_the_default_ttl() {
    let now = SystemTime::now();
    let mtime = now - Duration::from_secs(9); // 90% of a 10s TTL
    assert!(token_needs_refresh(Some(mtime), now, Some(10)));
    let mtime = now - Duration::from_secs(1); // 10% of a 10s TTL
    assert!(!token_needs_refresh(Some(mtime), now, Some(10)));
}

#[test]
fn a_non_positive_expiration_seconds_falls_back_to_the_default_ttl() {
    // A malformed/zero expirationSeconds must not be read as "expires
    // instantly" (which would mean every reconcile mints a fresh token,
    // the exact waste this fix removes) or as "never expires" either.
    let now = SystemTime::now();
    let mtime = now - Duration::from_secs(60);
    assert!(!token_needs_refresh(Some(mtime), now, Some(0)));
    assert!(!token_needs_refresh(Some(mtime), now, Some(-1)));
}

#[test]
fn a_clock_that_went_backwards_is_treated_as_stale() {
    let now = SystemTime::now();
    let mtime = now + Duration::from_secs(60); // mtime "in the future" relative to now
    assert!(token_needs_refresh(Some(mtime), now, None));
}
