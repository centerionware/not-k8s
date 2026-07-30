//! read_trim(): reads a sysfs/procfs-style file and falls back cleanly when
//! it's missing, empty, or whitespace-only — used for kernel_version,
//! machine_id, boot_id, none of which should ever crash node registration
//! just because a container/chroot doesn't expose them.
use super::*;

fn tmp_file(tag: &str, contents: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "nodelet-test-read-trim-{tag}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::write(&p, contents).unwrap();
    p
}

#[test]
fn reads_and_trims_trailing_newline() {
    let p = tmp_file("newline", "6.12.89-android16\n");
    assert_eq!(read_trim(p.to_str().unwrap(), "fallback"), "6.12.89-android16");
    std::fs::remove_file(&p).ok();
}

#[test]
fn reads_and_trims_surrounding_whitespace() {
    let p = tmp_file("whitespace", "  value-with-spaces  \n");
    assert_eq!(read_trim(p.to_str().unwrap(), "fallback"), "value-with-spaces");
    std::fs::remove_file(&p).ok();
}

#[test]
fn missing_file_returns_fallback() {
    assert_eq!(read_trim("/definitely/does/not/exist/xyz", "fallback"), "fallback");
}

#[test]
fn empty_file_returns_fallback_not_empty_string() {
    // An empty machine-id/boot-id is worse than a clearly-fake placeholder
    // — several downstream tools treat an empty string as "unset" but a
    // corrupt/garbage value as real data.
    let p = tmp_file("empty", "");
    assert_eq!(read_trim(p.to_str().unwrap(), "fallback"), "fallback");
    std::fs::remove_file(&p).ok();
}

#[test]
fn whitespace_only_file_returns_fallback() {
    let p = tmp_file("whitespace-only", "   \n\t  \n");
    assert_eq!(read_trim(p.to_str().unwrap(), "fallback"), "fallback");
    std::fs::remove_file(&p).ok();
}
