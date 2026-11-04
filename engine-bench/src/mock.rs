//! A local stand-in for a protected indexer. It proves the harness logic
//! without touching a real site, and it only imitates the shape of a challenge.

use crate::proxy::{basic_credentials, read_head};
use anyhow::Result;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};

const CLEARANCE: &str = "cf_clearance=mock-clearance";

/// Only the mock proxy can resolve this name, so an engine that ignores its
/// proxy can't load a page from it.
pub const PROXIED_HOST: &str = "indexer.test";

const INDEXER: &str = "<!doctype html><title>Indexer</title><p>Search results</p>";
const BLOCKED: &str = "<!doctype html><title>Access denied</title><p>Blocked</p>";
const PROBE: &str = "<!doctype html><title>Probe</title><p>Probe page</p>";

const SOLVES_ITSELF: &str = r#"<!doctype html><title>Just a moment...</title><p>Checking</p>
<script>
setTimeout(async () => {
  await fetch('/cdn-cgi/solve', { method: 'POST' });
  location.reload();
}, 400);
</script>"#;

const NEVER_SOLVES: &str = "<!doctype html><title>Just a moment...</title><p>Checking</p>";

const ASKS_FOR_A_CLICK: &str = r#"<!doctype html><title>Just a moment...</title>
<form><input type="hidden" name="cf-turnstile-response"></form>"#;

#[derive(Clone, Default)]
struct Mock {
    solved_for: Arc<Mutex<Option<String>>>,
}

pub async fn start() -> Result<SocketAddr> {
    let app = Router::new()
        .route("/open", get(|| async { page(StatusCode::OK, INDEXER) }))
        .route("/challenge", get(challenge))
        .route("/cdn-cgi/solve", post(solve))
        .route(
            "/never",
            get(|| async { page(StatusCode::FORBIDDEN, NEVER_SOLVES) }),
        )
        .route(
            "/interactive",
            get(|| async { page(StatusCode::FORBIDDEN, ASKS_FOR_A_CLICK) }),
        )
        .route(
            "/blocked",
            get(|| async { page(StatusCode::FORBIDDEN, BLOCKED) }),
        )
        .route("/probe", get(|| async { page(StatusCode::OK, PROBE) }))
        .with_state(Mock::default());

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok(address)
}

fn page(status: StatusCode, html: &'static str) -> Response {
    let mut response = (status, Html(html)).into_response();
    response
        .headers_mut()
        .insert(header::SERVER, HeaderValue::from_static("cloudflare"));
    response
}

fn user_agent(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::USER_AGENT)?.to_str().ok()?;
    Some(value.to_string())
}

async fn challenge(State(mock): State<Mock>, headers: HeaderMap) -> Response {
    let has_cookie = headers
        .get(header::COOKIE)
        .and_then(|cookies| cookies.to_str().ok())
        .is_some_and(|cookies| cookies.split("; ").any(|pair| pair == CLEARANCE));
    let same_agent = *mock.solved_for.lock().unwrap() == user_agent(&headers);

    if has_cookie && same_agent {
        page(StatusCode::OK, INDEXER)
    } else {
        page(StatusCode::FORBIDDEN, SOLVES_ITSELF)
    }
}

async fn solve(State(mock): State<Mock>, headers: HeaderMap) -> Response {
    *mock.solved_for.lock().unwrap() = user_agent(&headers);
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("cf_clearance=mock-clearance; Path=/; HttpOnly; Max-Age=1800"),
    );
    response
}

/// Stands in for a paid proxy: it checks the login, then tunnels to the target.
pub struct MockProxy {
    pub address: SocketAddr,
    #[cfg_attr(not(test), allow(dead_code))]
    tunnels: Arc<AtomicUsize>,
    #[cfg_attr(not(test), allow(dead_code))]
    auth_seen: Arc<Mutex<Option<String>>>,
}

impl MockProxy {
    #[cfg(test)]
    pub fn tunnels(&self) -> usize {
        self.tunnels.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub fn auth_seen(&self) -> Option<String> {
        self.auth_seen.lock().unwrap().clone()
    }
}

/// `login` is `user:pass`. Without one, the proxy accepts anybody.
pub async fn start_proxy(login: Option<&str>) -> Result<MockProxy> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let tunnels = Arc::new(AtomicUsize::new(0));
    let auth_seen = Arc::new(Mutex::new(None));
    let expected = login.map(basic_credentials);

    let (counter, seen) = (tunnels.clone(), auth_seen.clone());
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            tokio::spawn(serve_tunnel(
                client,
                expected.clone(),
                counter.clone(),
                seen.clone(),
            ));
        }
    });

    Ok(MockProxy {
        address,
        tunnels,
        auth_seen,
    })
}

async fn serve_tunnel(
    mut client: TcpStream,
    expected: Option<String>,
    tunnels: Arc<AtomicUsize>,
    auth_seen: Arc<Mutex<Option<String>>>,
) -> Option<()> {
    let (head, _) = read_head(&mut client).await?;
    let head = String::from_utf8(head).ok()?;
    let target = head
        .strip_prefix("CONNECT ")?
        .split(' ')
        .next()?
        .to_string();
    let auth = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("proxy-authorization")
            .then(|| value.trim().to_string())
    });
    *auth_seen.lock().unwrap() = auth.clone();

    if expected.is_some() && auth != expected {
        client
            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
            .await
            .ok()?;
        return None;
    }

    let target = target.replace(PROXIED_HOST, "127.0.0.1");
    let mut origin = TcpStream::connect(&target).await.ok()?;
    tunnels.fetch_add(1, Ordering::SeqCst);
    client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await
        .ok()?;
    copy_bidirectional(&mut client, &mut origin).await.ok()?;
    Some(())
}
