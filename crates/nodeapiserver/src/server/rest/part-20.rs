
/// Real upstream's own continuation-token contract: a client must treat
/// this as fully opaque, never construct or parse one itself. This
/// build's own encoding (base64 of `<resume-key>\0<revision>`) has no
/// compatibility requirement with real upstream's own token format,
/// since nothing outside this crate's own client/server pair ever reads
/// one.
///
/// `resume_key` must already be `list`'s own last-returned key with a
/// single `0x00` byte appended by the caller (the standard etcd idiom
/// for "the immediate lexicographic successor of this key" — exactly
/// the correct next `Range` start to exclude everything already
/// returned while including everything after it: byte-string
/// comparison guarantees any real key strictly greater than `last_key`
/// is always >= `last_key + 0x00`, since `0x00` is the smallest
/// possible byte). This function then appends *its own* `0x00` as the
/// key/revision separator — so a real encoded buffer ends up with two
/// consecutive `0x00` bytes where the successor marker meets the
/// separator, which is deliberate, not a bug: [`decode_continue_token`]
/// finds the *last* one to split on, so the successor marker correctly
/// stays part of the decoded key.
fn encode_continue_token(resume_key: &[u8], revision: i64) -> String {
    include!("body-28-1.rs");
    include!("body-28-2.rs");
}
