use reqwest::{Client, RequestBuilder, Response};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tracing::debug;

use crate::error::{HonchoError, Result};

const DEFAULT_BASE_URL: &str = "https://api.honcho.dev";

/// Honcho REST API client.
///
/// Cheaply clone-able — the inner `reqwest::Client` holds an `Arc`.
#[derive(Debug, Clone)]
pub struct HonchoClient {
    pub(crate) http: Client,
    pub(crate) base_url: String,
    pub(crate) token: String,
}

impl HonchoClient {
    /// Create a client pointing at the production SaaS endpoint.
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_base_url(DEFAULT_BASE_URL, token)
    }

    /// Create a client pointing at a custom base URL (e.g. `http://localhost:8000`).
    pub fn with_base_url(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            http: Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            token: token.into(),
        }
    }

    // ── internal helpers ──────────────────────────────────────────────────

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Build a URL with optional query params (key-value pairs).
    pub(crate) fn url_with_query(&self, path: &str, params: &[(&str, String)]) -> String {
        if params.is_empty() {
            return self.url(path);
        }
        let qs: String = params
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding(k), urlencoding(v)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{}{}?{}", self.base_url, path, qs)
    }

    fn auth(&self, rb: RequestBuilder) -> RequestBuilder {
        rb.bearer_auth(&self.token)
    }

    pub(crate) async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = self.url(path);
        debug!("GET {url}");
        let resp = self.auth(self.http.get(&url)).send().await?;
        parse_response(resp).await
    }

    pub(crate) async fn get_with_query<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T> {
        let url = self.url_with_query(path, query);
        debug!("GET {url}");
        let resp = self.auth(self.http.get(&url)).send().await?;
        parse_response(resp).await
    }

    pub(crate) async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let url = self.url(path);
        debug!("POST {url}");
        let resp = self.auth(self.http.post(&url)).json(body).send().await?;
        parse_response(resp).await
    }

    pub(crate) async fn post_with_query<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
        body: &B,
    ) -> Result<T> {
        let url = self.url_with_query(path, query);
        debug!("POST {url}");
        let resp = self.auth(self.http.post(&url)).json(body).send().await?;
        parse_response(resp).await
    }

    pub(crate) async fn put<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let url = self.url(path);
        debug!("PUT {url}");
        let resp = self.auth(self.http.put(&url)).json(body).send().await?;
        parse_response(resp).await
    }

    pub(crate) async fn put_with_query<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
        body: &B,
    ) -> Result<T> {
        let url = self.url_with_query(path, query);
        debug!("PUT {url}");
        let resp = self.auth(self.http.put(&url)).json(body).send().await?;
        parse_response(resp).await
    }

    pub(crate) async fn delete_ok(&self, path: &str) -> Result<()> {
        let url = self.url(path);
        debug!("DELETE {url}");
        let resp = self.auth(self.http.delete(&url)).send().await?;
        parse_no_content(resp).await
    }

    pub(crate) async fn delete_json<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        let url = self.url(path);
        debug!("DELETE {url} (with body)");
        let resp = self
            .auth(self.http.delete(&url))
            .json(body)
            .send()
            .await?;
        parse_no_content(resp).await
    }
}

async fn parse_response<T: DeserializeOwned>(resp: Response) -> Result<T> {
    let status = resp.status();
    if status.is_success() {
        Ok(resp.json::<T>().await?)
    } else {
        let code = status.as_u16();
        let body = resp.text().await.unwrap_or_default();
        Err(HonchoError::Http { status: code, body })
    }
}

pub(crate) async fn parse_no_content(resp: Response) -> Result<()> {
    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Err(HonchoError::Http { status, body })
    }
}

/// Minimal percent-encoding for query param keys and values.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
            | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
