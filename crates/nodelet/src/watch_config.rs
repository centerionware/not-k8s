//! Watch request timing shared by nodelet's independent informers.
//!
//! kube-rs otherwise sends the same five-minute (290s) timeout for every
//! informer. The control-plane processes start their informers together, so
//! that shared deadline turns one harmless reconnect into a synchronized LIST
//! storm. Keep each timeout within the API server's 295s limit, but spread
//! starts across a 56s window. The process id prevents the independently
//! compiled components from choosing the same sequence at startup.

use kube::runtime::watcher;
use std::sync::atomic::{AtomicU32, Ordering};

const WATCH_TIMEOUT_MIN_SECS: u32 = 240;
const WATCH_TIMEOUT_MAX_SECS: u32 = 295;
const WATCH_TIMEOUT_SPAN: u32 = WATCH_TIMEOUT_MAX_SECS - WATCH_TIMEOUT_MIN_SECS + 1;
static WATCH_CONFIG_SEQUENCE: AtomicU32 = AtomicU32::new(0);

pub(crate) fn config() -> watcher::Config {
    let sequence = WATCH_CONFIG_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let process_seed = std::process::id().wrapping_mul(0x9E37_79B9);
    let timeout = WATCH_TIMEOUT_MIN_SECS + process_seed.wrapping_add(sequence) % WATCH_TIMEOUT_SPAN;
    watcher::Config::default().timeout(timeout)
}
