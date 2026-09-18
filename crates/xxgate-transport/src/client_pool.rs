use super::deadlines::{ConnectDeadlineExceeded, LiveConnectLayer};
use async_trait::async_trait;
use futures::StreamExt;
use std::{collections::HashMap, sync::Mutex};
use tokio_util::sync::CancellationToken;
use xxgate_core::{
    Error, Result,
    protocol::{PreparedRequest, UpstreamResponse, UpstreamTransport},
    settings::LiveSettings,
};

pub struct HttpTransport {
    clients: Mutex<HashMap<String, (i64, reqwest::Client)>>,
    settings: LiveSettings,
    allow_http: bool,
}
impl HttpTransport {
    pub fn new(settings: LiveSettings, allow_http: bool) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        Self {
            clients: Mutex::new(HashMap::new()),
            settings,
            allow_http,
        }
    }
    fn client(&self, request: &PreparedRequest) -> Result<reqwest::Client> {
        let url =
            url::Url::parse(&request.url).map_err(|_| Error::invalid("Invalid upstream URL"))?;
        if url.username() != ""
            || url.password().is_some()
            || !(url.scheme() == "https" || self.allow_http && url.scheme() == "http")
        {
            return Err(Error::invalid("Unsupported upstream URL"));
        }
        let key = format!(
            "{}:{}:{}",
            request
                .account_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "control".into()),
            url.origin().ascii_serialization(),
            request.tls_backend
        );
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((version, client)) = clients.get(&key)
            && *version == request.profile_version
        {
            return Ok(client.clone());
        }
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .pool_max_idle_per_host(4)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .cookie_store(request.account_id.is_some())
            .connector_layer(LiveConnectLayer(self.settings.clone()));
        if request.tls_backend == "rustls" {
            builder = builder.use_rustls_tls();
        }
        let client = builder.build().map_err(|_| {
            Error::new(
                500,
                "client_initialization_failed",
                "Unable to initialize upstream HTTP client",
            )
        })?;
        clients.insert(key, (request.profile_version, client.clone()));
        Ok(client)
    }
}

fn transport_error(error: reqwest::Error) -> Error {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(err) = source {
        if err.downcast_ref::<ConnectDeadlineExceeded>().is_some() {
            return Error::new(504, "connect_timeout", "Upstream connection timed out");
        }
        source = err.source();
    }
    if error.is_connect() {
        Error::new(502, "connect_failed", "Unable to connect to the upstream")
    } else if error.is_timeout() {
        Error::new(504, "upstream_timeout", "Upstream transport timed out")
    } else {
        Error::new(502, "transport_failed", "Upstream HTTP transport failed")
    }
}

#[async_trait]
impl UpstreamTransport for HttpTransport {
    async fn send_once(
        &self,
        request: PreparedRequest,
        cancel: CancellationToken,
    ) -> Result<UpstreamResponse> {
        let client = self.client(&request)?;
        let future = client
            .request(request.method, &request.url)
            .headers(request.headers)
            .body(request.body)
            .send();
        let response = tokio::select! { biased; _=cancel.cancelled()=>return Err(Error::cancelled()), r=future=>r.map_err(transport_error)? };
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let stream = response
            .bytes_stream()
            .map(|r| r.map_err(transport_error))
            .take_until(cancel.cancelled_owned());
        Ok(UpstreamResponse {
            status,
            headers,
            bytes: Box::pin(stream),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    #[tokio::test]
    async fn server_errors_are_not_retried_or_redirected() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move || {
                let count = counted.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    (axum::http::StatusCode::SERVICE_UNAVAILABLE, "busy")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let transport = HttpTransport::new(LiveSettings::new(Default::default()), true);
        let r = transport
            .send_once(
                PreparedRequest {
                    method: http::Method::POST,
                    url: format!("http://{addr}/"),
                    headers: http::HeaderMap::new(),
                    body: bytes::Bytes::from_static(b"{}"),
                    account_id: None,
                    profile_version: 1,
                    tls_backend: "native".into(),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(r.status, 503);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        task.abort();
    }
}
