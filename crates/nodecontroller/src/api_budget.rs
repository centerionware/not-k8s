use http::{Request, Response};
use kube::client::Body;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::Semaphore;
use tower::{Layer, Service};

#[derive(Clone)]
pub(crate) struct ApiBudgetLayer {
    general: Arc<Semaphore>,
    reserved: Arc<Semaphore>,
}

impl ApiBudgetLayer {
    pub(crate) fn new(general_permits: usize, reserved_permits: usize) -> Self {
        Self {
            general: Arc::new(Semaphore::new(general_permits.max(1))),
            reserved: Arc::new(Semaphore::new(reserved_permits.max(1))),
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
        }
    }
}

#[derive(Clone)]
pub(crate) struct ApiBudgetService<S> {
    inner: S,
    general: Arc<Semaphore>,
    reserved: Arc<Semaphore>,
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
        let priority = request.uri().path().contains("/leases")
            || request
                .uri()
                .query()
                .is_some_and(|query| query.contains("watch=true"));
        let permit = if priority {
            self.reserved.clone().acquire_owned()
        } else {
            self.general.clone().acquire_owned()
        };
        let future = self.inner.call(request);
        Box::pin(async move {
            let _permit = permit
                .await
                .expect("nodecontroller API budget semaphore closed");
            future.await
        })
    }
}
