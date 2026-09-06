use http::{Request, Response};
use kube::client::Body;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;
use tokio::sync::Semaphore;
use tower::{Layer, Service};

#[derive(Clone)]
pub(crate) struct ApiBudgetLayer {
    general: Arc<Semaphore>,
    reserved: Arc<Semaphore>,
    general_active: Arc<AtomicUsize>,
    reserved_active: Arc<AtomicUsize>,
}

impl ApiBudgetLayer {
    pub(crate) fn new(general_permits: usize, reserved_permits: usize) -> Self {
        Self {
            general: Arc::new(Semaphore::new(general_permits.max(1))),
            reserved: Arc::new(Semaphore::new(reserved_permits.max(1))),
            general_active: Arc::new(AtomicUsize::new(0)),
            reserved_active: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl<S> Layer<S> for ApiBudgetLayer {
    type Service = ApiBudgetService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ApiBudgetService {
            inner,
            general: self.general.clone(),
            reserved: self.reserved.clone(),
            general_active: self.general_active.clone(),
            reserved_active: self.reserved_active.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ApiBudgetService<S> {
    inner: S,
    general: Arc<Semaphore>,
    reserved: Arc<Semaphore>,
    general_active: Arc<AtomicUsize>,
    reserved_active: Arc<AtomicUsize>,
}

impl<S, B> Service<Request<Body>> for ApiBudgetService<S>
where
    S: Service<Request<Body>, Response = Response<B>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    B: Send + 'static,
{
    type Response = Response<B>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let priority = is_reserved_request(&request);
        let permit = if priority {
            self.reserved.clone().acquire_owned()
        } else {
            self.general.clone().acquire_owned()
        };
        let active = if priority {
            self.reserved_active.clone()
        } else {
            self.general_active.clone()
        };
        let future = self.inner.call(request);
        Box::pin(async move {
            let started = Instant::now();
            let _permit = permit
                .await
                .expect("nodecontroller API budget semaphore closed");
            let active_count = active.fetch_add(1, Ordering::Relaxed) + 1;
            let wait = started.elapsed();
            if wait >= std::time::Duration::from_millis(100) {
                tracing::debug!(
                    reserved = priority,
                    wait_ms = wait.as_millis() as u64,
                    active = active_count,
                    "nodecontroller API request waited for concurrency budget"
                );
            }
            let result = future.await;
            active.fetch_sub(1, Ordering::Relaxed);
            result
        })
    }
}

fn is_reserved_request(request: &Request<Body>) -> bool {
    request.uri().path().contains("/leases")
        || request.uri().query().is_some_and(|query| {
            query.split('&').any(|part| {
                let mut fields = part.splitn(2, '=');
                fields.next() == Some("watch") && fields.next() == Some("true")
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use tokio::time::{Duration, sleep};
    use tower::service_fn;

    fn request(uri: &str) -> Request<Body> {
        Request::get(uri).body(Body::empty()).unwrap()
    }

    #[test]
    fn only_exact_watch_query_values_are_reserved() {
        assert!(is_reserved_request(&request("/api/v1/pods?watch=true")));
        assert!(is_reserved_request(&request(
            "/api/v1/pods?resourceVersion=1&watch=true"
        )));
        assert!(!is_reserved_request(&request("/api/v1/pods?watch=trueish")));
        assert!(!is_reserved_request(&request("/api/v1/pods?watch=false")));
    }

    #[tokio::test]
    async fn reserved_requests_progress_when_general_capacity_is_full() {
        let layer = ApiBudgetLayer::new(1, 1);
        let service = service_fn(|_: Request<Body>| async {
            sleep(Duration::from_millis(20)).await;
            Ok::<_, Infallible>(Response::new(()))
        });
        let mut service = layer.layer(service);

        let _general = service.call(request("/api/v1/pods"));
        tokio::task::yield_now().await;
        let reserved = service.call(request(
            "/apis/coordination.k8s.io/v1/namespaces/kube-system/leases?watch=true",
        ));
        tokio::time::timeout(Duration::from_millis(100), reserved)
            .await
            .expect("reserved request was blocked by general capacity")
            .unwrap();
    }
}
