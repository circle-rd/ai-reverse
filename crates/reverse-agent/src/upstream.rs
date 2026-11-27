//! HTTP client for the local inference engine.

use std::time::Duration;

use http::uri::Authority;
use http::{HeaderMap, Method, Request, Response, StatusCode, Uri, header};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use reverse_core::body::{ProxyBody, empty, full};
use reverse_core::secret::SecretString;

use crate::config::Config;
use crate::error::UpstreamError;

const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// The engine behind the agent. It alone knows the engine credential.
pub struct Upstream {
    client: Client<HttpConnector, ProxyBody>,
    authority: Authority,
    api_key: Option<SecretString>,
    headers_timeout: Duration,
}

impl Upstream {
    /// Builds a client for the configured engine.
    #[must_use]
    pub fn new(config: &Config) -> Self {
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(config.connect_timeout));
        connector.set_nodelay(true);
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .build(connector);
        Self {
            client,
            authority: config.upstream.authority().clone(),
            api_key: config.upstream_api_key.clone(),
            headers_timeout: config.upstream_headers_timeout,
        }
    }

    /// Sends a request; `headers` must already be filtered, the engine key is added here.
    pub async fn send(
        &self,
        method: Method,
        path: &str,
        headers: HeaderMap,
        body: ProxyBody,
    ) -> Result<Response<Incoming>, UpstreamError> {
        let uri = Uri::builder()
            .scheme("http")
            .authority(self.authority.clone())
            .path_and_query(path)
            .build()?;
        let mut request = Request::builder().method(method).uri(uri).body(body)?;
        *request.headers_mut() = headers;
        if let Some(key) = &self.api_key {
            request
                .headers_mut()
                .insert(header::AUTHORIZATION, key.bearer_header()?);
        }
        match tokio::time::timeout(self.headers_timeout, self.client.request(request)).await {
            Err(_) => Err(UpstreamError::Timeout(self.headers_timeout)),
            Ok(result) => Ok(result?),
        }
    }

    /// Reads a small text answer within `budget`; the status is returned, not interpreted.
    pub async fn get_text(
        &self,
        path: &str,
        max_bytes: usize,
        budget: Duration,
    ) -> Result<(StatusCode, String), UpstreamError> {
        self.exchange(
            Method::GET,
            path,
            HeaderMap::new(),
            empty(),
            max_bytes,
            budget,
        )
        .await
    }

    /// Posts a JSON body and reads a small text answer within `budget`.
    pub async fn post_json_text(
        &self,
        path: &str,
        body: Vec<u8>,
        max_bytes: usize,
        budget: Duration,
    ) -> Result<(StatusCode, String), UpstreamError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        self.exchange(Method::POST, path, headers, full(body), max_bytes, budget)
            .await
    }

    async fn exchange(
        &self,
        method: Method,
        path: &str,
        headers: HeaderMap,
        body: ProxyBody,
        max_bytes: usize,
        budget: Duration,
    ) -> Result<(StatusCode, String), UpstreamError> {
        let read = async {
            let response = self.send(method, path, headers, body).await?;
            let status = response.status();
            let bytes = Limited::new(response.into_body(), max_bytes)
                .collect()
                .await
                .map_err(|e| UpstreamError::Body(e.to_string()))?
                .to_bytes();
            Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
        };
        tokio::time::timeout(budget, read)
            .await
            .map_err(|_| UpstreamError::Timeout(budget))?
    }
}
