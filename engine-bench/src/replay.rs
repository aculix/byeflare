//! Repeats a request the way Prowlarr does after a solve: plain HTTP with the
//! solver's cookies and user agent. Passing here is the real goal of a solve.

use crate::detect::HttpResponse;
use anyhow::Result;
use reqwest::header::{CONTENT_ENCODING, COOKIE, HeaderMap, HeaderName, SERVER, USER_AGENT, VARY};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
}

/// Fetches `url` over plain HTTP and returns what came back.
pub async fn fetch(
    url: &str,
    cookies: &[Cookie],
    user_agent: Option<&str>,
    proxy: Option<&str>,
    timeout: Duration,
) -> Result<HttpResponse> {
    let mut client = reqwest::Client::builder().no_proxy().timeout(timeout);
    if let Some(proxy) = proxy {
        client = client.proxy(reqwest::Proxy::all(proxy)?);
    }

    let mut request = client.build()?.get(url);
    if let Some(user_agent) = user_agent {
        request = request.header(USER_AGENT, user_agent);
    }
    if !cookies.is_empty() {
        let pairs: Vec<String> = cookies
            .iter()
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect();
        request = request.header(COOKIE, pairs.join("; "));
    }

    let response = request.send().await?;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    Ok(HttpResponse {
        status,
        server: header_text(&headers, SERVER),
        vary: header_text(&headers, VARY),
        content_encoding: header_text(&headers, CONTENT_ENCODING),
        body: response.text().await?,
    })
}

fn header_text(headers: &HeaderMap, name: HeaderName) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?;
    Some(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::is_protected;
    use crate::mock;

    const AGENT: &str = "agent-a";
    const TIMEOUT: Duration = Duration::from_secs(5);

    fn clearance() -> Vec<Cookie> {
        vec![Cookie {
            name: "cf_clearance".into(),
            value: "mock-clearance".into(),
        }]
    }

    /// Solves the mock's challenge for `AGENT`, as a browser would.
    async fn solved_mock() -> String {
        let base = format!("http://{}", mock::start().await.unwrap());
        reqwest::Client::new()
            .post(format!("{base}/cdn-cgi/solve"))
            .header("user-agent", AGENT)
            .send()
            .await
            .unwrap();
        base
    }

    #[tokio::test]
    async fn matching_cookie_and_agent_pass() {
        let base = solved_mock().await;
        let response = fetch(
            &format!("{base}/challenge"),
            &clearance(),
            Some(AGENT),
            None,
            TIMEOUT,
        )
        .await
        .unwrap();
        assert_eq!(response.status, 200);
        assert!(!is_protected(&response));
    }

    #[tokio::test]
    async fn a_different_agent_is_challenged_again() {
        let base = solved_mock().await;
        let response = fetch(
            &format!("{base}/challenge"),
            &clearance(),
            Some("agent-b"),
            None,
            TIMEOUT,
        )
        .await
        .unwrap();
        assert!(is_protected(&response));
    }

    #[tokio::test]
    async fn no_cookies_means_a_challenge() {
        let base = solved_mock().await;
        let response = fetch(
            &format!("{base}/challenge"),
            &[],
            Some(AGENT),
            None,
            TIMEOUT,
        )
        .await
        .unwrap();
        assert!(is_protected(&response));
    }

    #[tokio::test]
    async fn a_block_page_counts_as_protected() {
        let base = solved_mock().await;
        let response = fetch(&format!("{base}/blocked"), &[], Some(AGENT), None, TIMEOUT)
            .await
            .unwrap();
        assert!(is_protected(&response));
    }

    #[tokio::test]
    async fn the_server_header_is_captured() {
        let base = solved_mock().await;
        let response = fetch(&format!("{base}/open"), &[], None, None, TIMEOUT)
            .await
            .unwrap();
        assert_eq!(response.server.as_deref(), Some("cloudflare"));
    }

    #[tokio::test]
    async fn an_unreachable_host_is_an_error() {
        let result = fetch(
            "http://127.0.0.1:1/",
            &[],
            None,
            None,
            Duration::from_secs(1),
        )
        .await;
        assert!(result.is_err());
    }
}
