//! Bounded request concurrency for API Priority and Fairness.
//!
//! This is the request-side enforcement half that was missing after
//! FlowSchema matching and response labeling landed. It uses Tokio's fair
//! semaphore queue for the finite request budget, honors an `Exempt`
//! PriorityLevelConfiguration, and leaves long-running streams out of the
//! budget so one watch or upgrade cannot consume all ordinary request seats.
//! Limited levels use the configured queue count and hand size to assign each
//! flow to the shortest queue in a deterministic shuffle-shard hand. The
//! seat-borrowing adjustment is a separate refinement; this gate still
//! enforces the important safety property that ordinary requests cannot grow
//! without bound.

use crate::flowcontrol::resolve::PriorityLevelConfig;
use crate::server::path::RequestInfo;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use thiserror::Error;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, TryAcquireError};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("API request queue is full")]
    QueueFull,
    #[error("API concurrency limiter is closed")]
    Closed,
}

#[derive(Debug)]
pub struct Permit {
    _requests: OwnedSemaphorePermit,
    _mutating_requests: Option<OwnedSemaphorePermit>,
    _priority: Option<PriorityLease>,
}

#[derive(Debug)]
struct PriorityState {
    active: AtomicUsize,
    max_concurrency: AtomicUsize,
    queue_length_limit: AtomicUsize,
    hand_size: AtomicUsize,
    queues: Box<[Arc<PriorityQueue>]>,
    notify: Notify,
}

#[derive(Debug)]
struct PriorityQueue {
    active: AtomicUsize,
    queued: AtomicUsize,
}

#[derive(Debug)]
struct PriorityLease {
    state: Arc<PriorityState>,
    queue: Arc<PriorityQueue>,
}

impl Drop for PriorityLease {
    fn drop(&mut self) {
        self.queue.active.fetch_sub(1, Ordering::Release);
        self.state.active.fetch_sub(1, Ordering::Release);
        self.state.notify.notify_one();
    }
}

struct QueueGuard {
    queue: Arc<PriorityQueue>,
    armed: bool,
}

impl QueueGuard {
    fn new(queue: Arc<PriorityQueue>) -> Self {
        Self { queue, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for QueueGuard {
    fn drop(&mut self) {
        if self.armed {
            self.queue.queued.fetch_sub(1, Ordering::Release);
        }
    }
}

#[derive(Clone)]
pub struct ConcurrencyLimiter {
    requests: Arc<Semaphore>,
    mutating_requests: Arc<Semaphore>,
    queued: Arc<AtomicUsize>,
    queue_length_limit: usize,
    max_requests: usize,
    priority_states: Arc<Mutex<HashMap<String, Arc<PriorityState>>>>,
}

impl ConcurrencyLimiter {
    pub fn new(max_requests: usize, max_mutating_requests: usize, queue_length_limit: usize) -> Self {
        Self {
            requests: Arc::new(Semaphore::new(max_requests)),
            mutating_requests: Arc::new(Semaphore::new(max_mutating_requests)),
            queued: Arc::new(AtomicUsize::new(0)),
            queue_length_limit,
            max_requests,
            priority_states: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Acquire the request seats for one request. `priority` is the selected
    /// PriorityLevelConfiguration, when APF resolution succeeded. Watches
    /// and connection-upgrade
    /// proxy requests are intentionally unbounded, matching the upstream
    /// long-running-request exemption from the normal request budget.
    pub async fn acquire(
        &self,
        info: &RequestInfo,
        query: &str,
        priority: Option<&PriorityLevelConfig>,
        flow_distinguisher: &str,
    ) -> Result<Option<Permit>, Error> {
        if priority.is_some_and(|level| level.exempt) || is_long_running(info, query) {
            return Ok(None);
        }
        if priority.is_some_and(|level| level.nominal_concurrency_shares == 0) {
            return Err(Error::QueueFull);
        }
        let requests = acquire_seat(self.requests.clone(), &self.queued, self.queue_length_limit).await?;
        let mut mutating_requests = if is_mutating(info) {
            match acquire_seat(self.mutating_requests.clone(), &self.queued, self.queue_length_limit).await {
                Ok(permit) => Some(permit),
                Err(error) => {
                    drop(requests);
                    return Err(error);
                }
            }
        } else {
            None
        };
        let priority = match priority {
            Some(level) => Some(acquire_priority(&self.priority_states, level, flow_distinguisher, self.max_requests).await?),
            None => None,
        };
        Ok(Some(Permit {
            _requests: requests,
            _mutating_requests: mutating_requests.take(),
            _priority: priority,
        }))
    }
}

async fn acquire_priority(
    states: &Arc<Mutex<HashMap<String, Arc<PriorityState>>>>,
    config: &PriorityLevelConfig,
    flow_distinguisher: &str,
    max_global_seats: usize,
) -> Result<PriorityLease, Error> {
    let limit = config
        .nominal_concurrency_shares
        .saturating_mul(max_global_seats)
        .div_ceil(config.total_nominal_concurrency_shares)
        .max(1);
    let state = {
        let mut states = states.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let queue_count = config.queues.max(1);
        let replace = states.get(&config.uid).is_some_and(|state| {
            state.queues.len() != queue_count
                && state.active.load(Ordering::Acquire) == 0
                && state.queues.iter().all(|queue| queue.queued.load(Ordering::Acquire) == 0)
        });
        if replace {
            states.insert(config.uid.clone(), Arc::new(new_priority_state(config, limit)));
        }
        states
            .entry(config.uid.clone())
            .or_insert_with(|| {
                Arc::new(new_priority_state(config, limit))
            })
            .clone()
    };
    state.max_concurrency.store(limit, Ordering::Release);
    state.queue_length_limit.store(if config.reject { 0 } else { config.queue_length_limit }, Ordering::Release);
    state.hand_size.store(config.hand_size.max(1).min(state.queues.len()), Ordering::Release);
    let queue = select_queue(&state, flow_distinguisher);

    loop {
        if try_claim(&state, &queue) {
            return Ok(PriorityLease { state, queue });
        }
        let notified = state.notify.notified();
        let mut guard = QueueGuard::new(queue.clone());
        let previous = queue.queued.fetch_add(1, Ordering::AcqRel);
        if previous >= state.queue_length_limit.load(Ordering::Acquire) {
            return Err(Error::QueueFull);
        }
        if try_claim(&state, &queue) {
            guard.disarm();
            drop(notified);
            return Ok(PriorityLease { state, queue });
        }
        notified.await;
        drop(guard);
    }
}

fn new_priority_state(config: &PriorityLevelConfig, limit: usize) -> PriorityState {
    let queue_count = config.queues.max(1);
    PriorityState {
        active: AtomicUsize::new(0),
        max_concurrency: AtomicUsize::new(limit),
        queue_length_limit: AtomicUsize::new(if config.reject { 0 } else { config.queue_length_limit }),
        hand_size: AtomicUsize::new(config.hand_size.max(1).min(queue_count)),
        queues: (0..queue_count)
            .map(|_| Arc::new(PriorityQueue { active: AtomicUsize::new(0), queued: AtomicUsize::new(0) }))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
        notify: Notify::new(),
    }
}

fn select_queue(state: &PriorityState, flow_distinguisher: &str) -> Arc<PriorityQueue> {
    let hand = shuffle_hand(state.queues.len(), state.hand_size.load(Ordering::Acquire), flow_distinguisher);
    let queue_index = hand
        .into_iter()
        .min_by_key(|index| {
            let queue = &state.queues[*index];
            (
                queue.active.load(Ordering::Acquire).saturating_add(queue.queued.load(Ordering::Acquire)),
                *index,
            )
        })
        .unwrap_or(0);
    state.queues[queue_index].clone()
}

/// Produce a stable, process-independent queue hand from the flow
/// distinguisher. Upstream uses a deterministic hash-to-permutation step;
/// this compact FNV-1a plus splitmix sequence has the same required
/// properties here: identical flows keep the same hand, distinct flows are
/// spread across the configured queues, and no process-random hash seed can
/// make behavior change between requests handled by one apiserver.
fn shuffle_hand(queue_count: usize, hand_size: usize, flow_distinguisher: &str) -> Vec<usize> {
    if queue_count == 0 {
        return Vec::new();
    }
    let target = hand_size.max(1).min(queue_count);
    let mut seed = stable_hash(flow_distinguisher);
    let mut hand = Vec::with_capacity(target);
    let mut attempts = 0;
    while hand.len() < target && attempts < queue_count.saturating_mul(16) {
        attempts += 1;
        seed = splitmix64(seed);
        let index = (seed as usize) % queue_count;
        if !hand.contains(&index) {
            hand.push(index);
        }
    }
    for index in 0..queue_count {
        if hand.len() == target {
            break;
        }
        if !hand.contains(&index) {
            hand.push(index);
        }
    }
    hand
}

fn stable_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3))
}

fn splitmix64(value: u64) -> u64 {
    let mut value = value.wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

fn try_claim(state: &PriorityState, queue: &PriorityQueue) -> bool {
    let limit = state.max_concurrency.load(Ordering::Acquire);
    let mut active = state.active.load(Ordering::Acquire);
    loop {
        if active >= limit {
            return false;
        }
        match state.active.compare_exchange_weak(active, active + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                queue.active.fetch_add(1, Ordering::AcqRel);
                return true;
            }
            Err(observed) => active = observed,
        }
    }
}

async fn acquire_seat(semaphore: Arc<Semaphore>, queued: &AtomicUsize, queue_length_limit: usize) -> Result<OwnedSemaphorePermit, Error> {
    match semaphore.clone().try_acquire_owned() {
        Ok(permit) => Ok(permit),
        Err(TryAcquireError::Closed) => Err(Error::Closed),
        Err(TryAcquireError::NoPermits) => {
            let previous = queued.fetch_add(1, Ordering::AcqRel);
            if previous >= queue_length_limit {
                queued.fetch_sub(1, Ordering::Release);
                return Err(Error::QueueFull);
            }
            let result = semaphore.acquire_owned().await.map_err(|_| Error::Closed);
            queued.fetch_sub(1, Ordering::Release);
            result
        }
    }
}

fn is_mutating(info: &RequestInfo) -> bool {
    matches!(info.verb.as_str(), "create" | "update" | "patch" | "delete" | "deletecollection")
}

fn is_long_running(info: &RequestInfo, query: &str) -> bool {
    if matches!(info.verb.as_str(), "watch" | "proxy")
        || matches!(info.subresource.as_str(), "exec" | "attach" | "portforward")
    {
        return true;
    }
    info.subresource == "log"
        && query.split('&').any(|part| {
            let Some((key, value)) = part.split_once('=') else {
                return false;
            };
            key == "follow" && !matches!(value, "" | "0" | "false")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(verb: &str) -> RequestInfo {
        RequestInfo {
            verb: verb.to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn ordinary_requests_are_bounded_and_mutations_use_both_budgets() {
        let limiter = ConcurrencyLimiter::new(1, 1, 2);
        let first = limiter.acquire(&request("get"), "", None, "").await.unwrap().unwrap();
        let waiter = {
            let limiter = limiter.clone();
            tokio::spawn(async move { limiter.acquire(&request("create"), "", None, "").await })
        };
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(first);
        let second = waiter.await.unwrap().unwrap().unwrap();
        drop(second);
    }

    #[tokio::test]
    async fn queue_limit_rejects_without_waiting_forever() {
        let limiter = ConcurrencyLimiter::new(1, 1, 0);
        let first = limiter.acquire(&request("get"), "", None, "").await.unwrap().unwrap();
        let error = limiter.acquire(&request("get"), "", None, "").await.unwrap_err();
        assert_eq!(error, Error::QueueFull);
        drop(first);
    }

    #[tokio::test]
    async fn zero_queue_length_still_allows_an_immediately_available_request() {
        let limiter = ConcurrencyLimiter::new(1, 1, 0);
        assert!(limiter.acquire(&request("get"), "", None, "").await.unwrap().is_some());
        assert!(limiter.acquire(&request("create"), "", None, "").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn exempt_and_long_running_requests_do_not_consume_seats() {
        let limiter = ConcurrencyLimiter::new(1, 1, 0);
        let exempt = PriorityLevelConfig {
            uid: "exempt".to_string(),
            exempt: true,
            nominal_concurrency_shares: 0,
            total_nominal_concurrency_shares: 1,
            queues: 1,
            hand_size: 1,
            queue_length_limit: 0,
            lendable_percent: 0,
            borrowing_limit_percent: None,
            reject: false,
        };
        assert!(limiter.acquire(&request("get"), "", Some(&exempt), "").await.unwrap().is_none());
        let mut watch = request("watch");
        watch.is_resource_request = true;
        assert!(limiter.acquire(&watch, "", None, "").await.unwrap().is_none());
        let mut log = request("get");
        log.subresource = "log".to_string();
        assert!(limiter.acquire(&log, "follow=true", None, "").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn limited_priority_level_has_its_own_queue_limit() {
        let limiter = ConcurrencyLimiter::new(2, 2, 2);
        let level = PriorityLevelConfig {
            uid: "limited".to_string(),
            exempt: false,
            nominal_concurrency_shares: 1,
            total_nominal_concurrency_shares: 2,
            queues: 2,
            hand_size: 1,
            queue_length_limit: 0,
            lendable_percent: 0,
            borrowing_limit_percent: None,
            reject: false,
        };
        let first = limiter.acquire(&request("get"), "", Some(&level), "alice").await.unwrap().unwrap();
        assert_eq!(limiter.acquire(&request("get"), "", Some(&level), "alice").await.unwrap_err(), Error::QueueFull);
        drop(first);
    }

    #[tokio::test]
    async fn priority_waiter_is_released_when_the_level_seat_is_dropped() {
        let limiter = ConcurrencyLimiter::new(2, 2, 2);
        let level = PriorityLevelConfig {
            uid: "limited".to_string(),
            exempt: false,
            nominal_concurrency_shares: 1,
            total_nominal_concurrency_shares: 2,
            queues: 2,
            hand_size: 1,
            queue_length_limit: 1,
            lendable_percent: 0,
            borrowing_limit_percent: None,
            reject: false,
        };
        let first = limiter.acquire(&request("get"), "", Some(&level), "alice").await.unwrap().unwrap();
        let waiter = {
            let limiter = limiter.clone();
            let level = level.clone();
            tokio::spawn(async move { limiter.acquire(&request("get"), "", Some(&level), "alice").await })
        };
        tokio::task::yield_now().await;
        drop(first);
        assert!(waiter.await.unwrap().unwrap().is_some());
    }

    #[tokio::test]
    async fn priority_level_state_is_shared_across_flow_distinguishers() {
        let limiter = ConcurrencyLimiter::new(2, 2, 0);
        let level = PriorityLevelConfig {
            uid: "limited".to_string(),
            exempt: false,
            nominal_concurrency_shares: 1,
            total_nominal_concurrency_shares: 2,
            queues: 2,
            hand_size: 1,
            queue_length_limit: 0,
            lendable_percent: 0,
            borrowing_limit_percent: None,
            reject: false,
        };

        let alice = limiter.acquire(&request("get"), "", Some(&level), "alice").await.unwrap().unwrap();
        assert_eq!(limiter.acquire(&request("get"), "", Some(&level), "bob").await.unwrap_err(), Error::QueueFull);
        assert_eq!(limiter.priority_states.lock().unwrap().len(), 1);
        drop(alice);
    }

    #[test]
    fn shuffle_hand_is_stable_and_unique() {
        let first = shuffle_hand(16, 4, "alice");
        assert_eq!(first, shuffle_hand(16, 4, "alice"));
        assert_eq!(first.len(), 4);
        assert_eq!(first.iter().collect::<std::collections::HashSet<_>>().len(), 4);
    }

    #[tokio::test]
    async fn distinct_flows_wait_in_the_shared_priority_queue() {
        let limiter = ConcurrencyLimiter::new(2, 2, 2);
        let level = PriorityLevelConfig {
            uid: "limited".to_string(),
            exempt: false,
            nominal_concurrency_shares: 1,
            total_nominal_concurrency_shares: 2,
            queues: 8,
            hand_size: 2,
            queue_length_limit: 2,
            lendable_percent: 0,
            borrowing_limit_percent: None,
            reject: false,
        };
        let first = limiter.acquire(&request("get"), "", Some(&level), "alice").await.unwrap().unwrap();
        let waiter = {
            let limiter = limiter.clone();
            let level = level.clone();
            tokio::spawn(async move { limiter.acquire(&request("get"), "", Some(&level), "bob").await })
        };
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        assert_eq!(limiter.priority_states.lock().unwrap().len(), 1);
        drop(first);
        assert!(waiter.await.unwrap().unwrap().is_some());
    }
}
