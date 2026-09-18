use std::{
    error::Error as StdError,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time::{Duration, Instant};
use tower::{Layer, Service};
use xxgate_core::settings::LiveSettings;

type BoxError = Box<dyn StdError + Send + Sync>;

#[derive(Debug)]
pub struct ConnectDeadlineExceeded;
impl std::fmt::Display for ConnectDeadlineExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("xxgate_connect_timeout")
    }
}
impl StdError for ConnectDeadlineExceeded {}

#[derive(Clone)]
pub struct LiveConnectLayer(pub LiveSettings);
impl<S> Layer<S> for LiveConnectLayer {
    type Service = LiveConnector<S>;
    fn layer(&self, inner: S) -> Self::Service {
        LiveConnector {
            inner,
            settings: self.0.clone(),
        }
    }
}

#[derive(Clone)]
pub struct LiveConnector<S> {
    inner: S,
    settings: LiveSettings,
}

impl<S, R> Service<R> for LiveConnector<S>
where
    S: Service<R, Error = BoxError> + Send + Sync + 'static,
    S::Future: Send + 'static,
    S::Response: Send + 'static,
    R: Send + 'static,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, BoxError>> + Send>>;
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }
    fn call(&mut self, request: R) -> Self::Future {
        let connecting = self.inner.call(request);
        let mut settings = self.settings.subscribe();
        Box::pin(async move {
            let started = Instant::now();
            tokio::pin!(connecting);
            loop {
                let deadline = started
                    + Duration::from_millis(settings.borrow_and_update().connect_timeout_ms);
                tokio::select! {
                    biased;
                    _ = settings.changed() => {},
                    _ = tokio::time::sleep_until(deadline) => return Err(Box::new(ConnectDeadlineExceeded) as BoxError),
                    result = &mut connecting => return result,
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xxgate_core::settings::RuntimeSettings;
    #[tokio::test(start_paused = true)]
    async fn updating_timeout_interrupts_an_existing_connection() {
        let settings = LiveSettings::new(RuntimeSettings::default());
        let service = tower::service_fn(|_: ()| async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok::<(), BoxError>(())
        });
        let mut connector = LiveConnectLayer(settings.clone()).layer(service);
        let future = connector.call(());
        let task = tokio::spawn(future);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        let mut next = settings.current();
        next.connect_timeout_ms = 1000;
        settings.publish(next);
        assert!(
            task.await
                .unwrap()
                .unwrap_err()
                .downcast_ref::<ConnectDeadlineExceeded>()
                .is_some()
        );
    }
}
