
/// The inverse of [`encode_continue_token`]. `None` for anything
/// malformed (not valid base64, no `0x00` separator, a non-numeric
/// revision) — surfaced by `list` as a real `ListOutcome::
/// InvalidContinueToken`, not a panic or a silently-wrong resume point.
/// Splits on the *last* `0x00` byte rather than the first, defensively:
/// a resume key built from real object names should never itself
/// contain one (`DNS-1123` names have no room for a null byte), but
/// searching from the end costs nothing and removes even that
/// assumption.
fn decode_continue_token(token: &str) -> Option<(Vec<u8>, i64)> {
    include!("body-29-1.rs");
    include!("body-29-2.rs");
}
