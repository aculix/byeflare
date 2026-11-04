use async_trait::async_trait;
use axum::extract::State;
use axum::routing::any;
use byeflare::api::{AppState, Limits, app};
use byeflare::cache::Cache;
use byeflare::domain::{Cleared, NamedCookie, SolveError, Target};
use byeflare::solver::Solver;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Clone)]
enum Behaviour {
    Solve,
    Fail(SolveError),
    /// Fails the first solve only, then succeeds.
    FailFirst(SolveError),
}

struct Fake {
    behaviour: Behaviour,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Solver for Fake {
    async fn clear(&self, _target: &Target) -> Result<Cleared, SolveError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.behaviour {
            Behaviour::FailFirst(error) if call == 0 => Err(error.clone()),
            Behaviour::Solve | Behaviour::FailFirst(_) => Ok(Cleared {
                url: "https://example/".into(),
                status: 200,
                body: String::new(),
                cookies: vec![cookie()],
            }),
            Behaviour::Fail(error) => Err(error.clone()),
        }
    }
}

fn cookie() -> NamedCookie {
    NamedCookie {
        name: "cf_clearance".into(),
        value: "v".into(),
        domain: "host".into(),
        path: "/".into(),
        expires: None,
        http_only: true,
        secure: true,
    }
}

fn state(solver: Arc<dyn Solver>, cache: Cache) -> Arc<AppState> {
    Arc::new(AppState {
        solver,
        cache,
        user_agent: "agent/1".into(),
        header_order: vec![],
        limits: Limits {
            default_timeout: Duration::from_secs(5),
            max_timeout: Duration::from_secs(10),
            max_solve: Duration::from_secs(2),
        },
        version: "test".into(),
        inflight: Arc::new(tokio::sync::Semaphore::new(4)),
    })
}

/// An origin that serves a challenge until `cf_clearance` arrives.
async fn origin() -> String {
    async fn handler(State(echo): State<bool>, request: axum::extract::Request) -> String {
        let cleared = request
            .headers()
            .get("cookie")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("cf_clearance"));
        let body = axum::body::to_bytes(request.into_body(), 1 << 20)
            .await
            .unwrap();
        if echo {
            return format!("echo:{}", String::from_utf8_lossy(&body));
        }
        if cleared {
            "results".into()
        } else {
            "<title>Just a moment...</title>".into()
        }
    }

    async fn serve(echo: bool) -> String {
        let app = axum::Router::new().fallback(any(handler)).with_state(echo);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}/")
    }

    serve(false).await
}

async fn origin_echo() -> String {
    async fn handler(State(_): State<bool>, request: axum::extract::Request) -> String {
        let body = axum::body::to_bytes(request.into_body(), 1 << 20)
            .await
            .unwrap();
        format!("echo:{}", String::from_utf8_lossy(&body))
    }
    let app = axum::Router::new().fallback(any(handler)).with_state(true);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}/")
}

async fn start(solver: Arc<dyn Solver>, cache: Cache) -> String {
    let app = app(state(solver, cache));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

fn fake(behaviour: Behaviour) -> (Arc<dyn Solver>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let solver = Arc::new(Fake {
        behaviour,
        calls: calls.clone(),
    });
    (solver, calls)
}

async fn call(url: &str, body: serde_json::Value) -> serde_json::Value {
    let client = wreq::Client::new();
    let response = client
        .post(format!("{url}/v1"))
        .json(&body)
        .send()
        .await
        .unwrap();
    response.json().await.unwrap()
}

#[tokio::test]
async fn ping_answers_get_and_post() {
    let (solver, _) = fake(Behaviour::Solve);
    let url = start(solver, Cache::new(8)).await;
    let client = wreq::Client::new();
    let get = client
        .get(format!("{url}/ping"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(get["status"], "ok");
    let post = client
        .post(format!("{url}/ping"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(post["status"], "ok");
}

#[tokio::test]
async fn a_solve_returns_the_page_and_cookies_and_then_caches() {
    let target = origin().await;
    let (solver, calls) = fake(Behaviour::Solve);
    let url = start(solver, Cache::new(8)).await;
    let body = serde_json::json!({"cmd": "request.get", "url": target, "maxTimeout": 5000});
    let first = call(&url, body.clone()).await;
    assert_eq!(first["status"], "ok");
    assert_eq!(first["solution"]["response"], "results");
    assert_eq!(first["solution"]["userAgent"], "agent/1");
    assert_eq!(first["solution"]["cookies"][0]["name"], "cf_clearance");

    let second = call(&url, body).await;
    assert_eq!(second["status"], "ok");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_stale_cookie_is_detected_and_replaced() {
    let target = origin().await;
    let (solver, calls) = fake(Behaviour::Solve);
    let cache = Cache::new(8);
    let host = target.trim_start_matches("http://").trim_end_matches('/');
    cache
        .insert(
            format!("{host}|-"),
            vec![NamedCookie {
                name: "stale".into(),
                value: "x".into(),
                domain: "host".into(),
                path: "/".into(),
                expires: None,
                http_only: false,
                secure: false,
            }],
            std::time::Instant::now() + Duration::from_secs(60),
        )
        .await;
    let url = start(solver, cache).await;
    let response = call(
        &url,
        serde_json::json!({"cmd": "request.get", "url": target, "maxTimeout": 5000}),
    )
    .await;
    assert_eq!(response["status"], "ok");
    assert_eq!(response["solution"]["response"], "results");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn not_challenged_returns_the_plain_page() {
    let target = origin().await;
    let (solver, _) = fake(Behaviour::FailFirst(SolveError::NotChallenged));
    let url = start(solver, Cache::new(8)).await;
    let response = call(
        &url,
        serde_json::json!({"cmd": "request.get", "url": target, "maxTimeout": 5000}),
    )
    .await;
    assert_eq!(response["status"], "ok");
    // The origin challenges without the cookie, so this proves the refetch path ran.
    assert_eq!(response["solution"]["response"], "results");
}

#[tokio::test]
async fn unsupported_becomes_an_error_envelope() {
    let target = origin().await;
    let (solver, _) = fake(Behaviour::Fail(SolveError::Unsupported("new build".into())));
    let url = start(solver, Cache::new(8)).await;
    let response = call(
        &url,
        serde_json::json!({"cmd": "request.get", "url": target, "maxTimeout": 5000}),
    )
    .await;
    assert_eq!(response["status"], "error");
    assert!(response["message"].as_str().unwrap().contains("new build"));
}

#[tokio::test]
async fn a_post_returns_the_posted_echo() {
    let target = origin_echo().await;
    let (solver, _) = fake(Behaviour::Solve);
    let url = start(solver, Cache::new(8)).await;
    let response = call(
        &url,
        serde_json::json!({
            "cmd": "request.post",
            "url": target,
            "postData": "q=x",
            "headers": {"content-type": "application/x-www-form-urlencoded"},
            "maxTimeout": 5000
        }),
    )
    .await;
    assert_eq!(response["status"], "ok");
    assert_eq!(response["solution"]["response"], "echo:q=x");
}

#[tokio::test]
async fn unknown_commands_and_sessions() {
    let (solver, _) = fake(Behaviour::Solve);
    let url = start(solver, Cache::new(8)).await;
    let unknown = call(&url, serde_json::json!({"cmd": "nope"})).await;
    assert_eq!(unknown["status"], "error");

    let created = call(&url, serde_json::json!({"cmd": "sessions.create"})).await;
    assert_eq!(created["status"], "ok");
    assert!(created["session"].as_str().is_some());
}
