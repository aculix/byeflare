use crate::domain::NamedCookie;
use std::time::Instant;

pub struct Fetcher {
    client: wreq::Client,
    user_agent: String,
    header_order: Vec<String>,
}

#[derive(Debug)]
pub struct Snapshot {
    pub url: String,
    pub status: u16,
    pub body: String,
}

impl Fetcher {
    pub fn new(
        user_agent: String,
        header_order: Vec<String>,
        proxy: Option<&str>,
    ) -> anyhow::Result<Self> {
        let mut builder = wreq::Client::builder()
            .redirect(wreq::redirect::Policy::limited(5))
            .timeout(std::time::Duration::from_secs(60));
        if let Some(proxy) = proxy {
            builder = builder.proxy(wreq::Proxy::all(proxy)?);
        }
        Ok(Self {
            client: builder.build()?,
            user_agent,
            header_order,
        })
    }

    fn request(
        &self,
        method: wreq::Method,
        url: &str,
        cookies: &[NamedCookie],
        deadline: Instant,
    ) -> anyhow::Result<wreq::RequestBuilder> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut builder = self.client.request(method, url).timeout(remaining);
        builder = builder.header(wreq::header::USER_AGENT, &self.user_agent);
        for entry in &self.header_order {
            let (name, value) = match entry.split_once(": ") {
                Some((name, value)) => (name, Some(value)),
                None => (entry.as_str(), None),
            };
            let skip = ["user-agent", "cookie", "accept-encoding", "referer"]
                .iter()
                .any(|skipped| name.eq_ignore_ascii_case(skipped));
            if skip {
                continue;
            }
            let Some(value) = value else { continue };
            builder = builder.header(name, value);
        }
        if !cookies.is_empty() {
            let joined = cookies
                .iter()
                .map(|cookie| format!("{}={}", cookie.name, cookie.value))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(wreq::header::COOKIE, joined);
        }
        Ok(builder)
    }

    pub async fn get(
        &self,
        url: &str,
        cookies: &[NamedCookie],
        deadline: Instant,
    ) -> anyhow::Result<Snapshot> {
        let response = self
            .request(wreq::Method::GET, url, cookies, deadline)?
            .send()
            .await?;
        snapshot(response, url).await
    }

    pub async fn post(
        &self,
        url: &str,
        body: &str,
        content_type: &str,
        cookies: &[NamedCookie],
        deadline: Instant,
    ) -> anyhow::Result<Snapshot> {
        let response = self
            .request(wreq::Method::POST, url, cookies, deadline)?
            .header(wreq::header::CONTENT_TYPE, content_type)
            .body(body.to_string())
            .send()
            .await?;
        snapshot(response, url).await
    }
}

async fn snapshot(response: wreq::Response, url: &str) -> anyhow::Result<Snapshot> {
    let status = response.status().as_u16();
    let body = response.text().await?;
    Ok(Snapshot {
        url: url.to_string(),
        status,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::NamedCookie;
    use axum::extract::State;
    use axum::routing::any;
    use std::sync::{Arc, Mutex};

    #[derive(Default, Clone)]
    struct Seen {
        method: String,
        user_agent: String,
        cookie: String,
        body: String,
    }

    async fn echo(State(seen): State<Arc<Mutex<Seen>>>, request: axum::extract::Request) -> String {
        let method = request.method().to_string();
        let user_agent = request
            .headers()
            .get("user-agent")
            .map(|v| v.to_str().unwrap_or(""))
            .unwrap_or("")
            .to_string();
        let cookie = request
            .headers()
            .get("cookie")
            .map(|v| v.to_str().unwrap_or(""))
            .unwrap_or("")
            .to_string();
        let body = axum::body::to_bytes(request.into_body(), 1 << 20)
            .await
            .unwrap();
        let mut seen = seen.lock().unwrap();
        seen.method = method;
        seen.user_agent = user_agent;
        seen.cookie = cookie;
        seen.body = String::from_utf8_lossy(&body).to_string();
        "hello".to_string()
    }

    async fn serve(seen: Arc<Mutex<Seen>>) -> String {
        let app = axum::Router::new().fallback(any(echo)).with_state(seen);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}/")
    }

    fn cookies() -> Vec<NamedCookie> {
        vec![NamedCookie {
            name: "cf_clearance".into(),
            value: "v".into(),
            domain: "host".into(),
            path: "/".into(),
            expires: None,
            http_only: true,
            secure: true,
        }]
    }

    fn deadline() -> Instant {
        Instant::now() + std::time::Duration::from_secs(10)
    }

    #[tokio::test]
    async fn a_get_carries_the_agent_and_cookies() {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let url = serve(seen.clone()).await;
        let fetcher = Fetcher::new("agent/1".into(), vec![], None).unwrap();
        let snapshot = fetcher.get(&url, &cookies(), deadline()).await.unwrap();
        assert_eq!(snapshot.status, 200);
        assert_eq!(snapshot.body, "hello");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.method, "GET");
        assert_eq!(seen.user_agent, "agent/1");
        assert_eq!(seen.cookie, "cf_clearance=v");
    }

    #[tokio::test]
    async fn a_post_sends_its_body_and_type() {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let url = serve(seen.clone()).await;
        let fetcher = Fetcher::new("agent/1".into(), vec![], None).unwrap();
        fetcher
            .post(
                &url,
                "q=x",
                "application/x-www-form-urlencoded",
                &cookies(),
                deadline(),
            )
            .await
            .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.method, "POST");
        assert_eq!(seen.body, "q=x");
    }
}
