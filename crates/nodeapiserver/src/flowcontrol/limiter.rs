//! Bounded request concurrency for API Priority and Fairness.
//!
//! This is the request-side enforcement half that was missing after
//! FlowSchema matching and response labeling landed. It uses Tokio's fair
//! semaphore queue for the finite request budget, honors an `Exempt`
//! PriorityLevelConfiguration, and leaves long-running streams out of the
//! budget so one watch or upgrade cannot consume all ordinary request seats.
//! The full upstream shuffle-sharded per-flow queue is a separate refinement;
//! this gate still enforces the important safety property that ordinary
//! requests cannot grow without bound.

use crate::server::path::RequestInfo;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

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
}

#[derive(Clone)]
pub struct ConcurrencyLimiter {
    requests: Arc<Semaphore>,
    mutating_requests: Arc<Semaphore>,
    queued: Arc<AtomicUsize>,
    queue_length_limit: usize,
}

impl ConcurrencyLimiter {
    pub fn new(max_requests: usize, max_mutating_requests: usize, queue_length_limit: usize) -> Self {
        Self {
            requests: Arc::new(Semaphore::new(max_requests)),
            mutating_requests: Arc::new(Semaphore::new(max_mutating_requests)),
            queued: Arc::new(AtomicUsize::new(0)),
            queue_length_limit,
        }
    }

    /// Acquire the request seats for one request. `exempt` is taken from the
    /// selected PriorityLevelConfiguration. Watches and connection-upgrade
    /// proxy requests are intentionally unbounded, matching the upstream
    /// long-running-request exemption from the normal request budget.
    pub async fn acquire(
        &self,
        info: &RequestInfo,
        query: &str,
        exempt: bool,
    ) -> Result<Option<Permit>, Error> {
        if exempt || is_long_running(info, query) {
            return Ok(None);
        }
        let previous = self.queued.fetch_add(1, Ordering::AcqRel);
        if previous >= self.queue_length_limit {
            self.queued.fetch_sub(1, Ordering::Release);
            return Err(Error::QueueFull);
        }
        let requests = match self.requests.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                self.queued.fetch_sub(1, Ordering::Release);
                return Err(Error::Closed);
            }
        };
        let mutating_requests = if is_mutating(info) {
            match self.mutating_requests.clone().acquire_owned().await {
                Ok(permit) => Some(permit),
                Err(_) => {
                    drop(requests);
                    self.queued.fetch_sub(1, Ordering::Release);
                    return Err(Error::Closed);
                }
            }
        } else {
            None
        };
        self.queued.fetch_sub(1, Ordering::Release);
        Ok(Some(Permit {
            _requests: requests,
            _mutating_requests: mutating_requests.take(),
        }))
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
        let first = limiter.acquire(&request("get"), "", false).await.unwrap().unwrap();
        let waiter = {
            let limiter = limiter.clone();
            tokio::spawn(async move { limiter.acquire(&request("create"), "", false).await })
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
        let error = limiter.acquire(&request("get"), "", false).await.unwrap_err();
        assert_eq!(error, Error::QueueFull);
    }

    #[tokio::test]
    async fn exempt_and_long_running_requests_do_not_consume_seats() {
        let limiter = ConcurrencyLimiter::new(1, 1, 0);
        assert!(limiter.acquire(&request("get"), "", true).await.unwrap().is_none());
        let mut watch = request("watch");
        watch.is_resource_request = true;
        assert!(limiter.acquire(&watch, "", false).await.unwrap().is_none());
        let mut log = request("get");
        log.subresource = "log".to_string();
        assert!(limiter.acquire(&log, "follow=true", false).await.unwrap().is_none());
    }
}
