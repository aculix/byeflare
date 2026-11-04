use crate::cache::Cache;
use crate::detect::is_challenged;
use crate::domain::{Cleared, NamedCookie, SolveError, Target};
use crate::fetch::Snapshot;
use crate::proxy;
use crate::solver::Solver;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub struct Limits {
    pub default_timeout: Duration,
    pub max_timeout: Duration,
    pub max_solve: Duration,
}

pub struct AppState {
    pub solver: Arc<dyn Solver>,
    pub cache: Cache,
    pub user_agent: String,
    pub header_order: Vec<String>,
    pub limits: Limits,
    pub version: String,
}

pub fn app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/ping", get(ping).post(ping))
        .route("/v1", post(v1))
        .with_state(state)
}

#[derive(Deserialize)]
struct V1Request {
    cmd: String,
    url: Option<String>,
    #[serde(rename = "maxTimeout")]
    max_timeout: Option<u64>,
    #[serde(rename = "postData")]
    post_data: Option<String>,
    headers: Option<serde_json::Value>,
    proxy: Option<ProxyInput>,
    session: Option<String>,
}

#[derive(Deserialize)]
struct ProxyInput {
    url: String,
    username: Option<String>,
    password: Option<String>,
}

#[derive(Serialize)]
struct Envelope {
    status: &'static str,
    message: String,
    version: String,
    #[serde(rename = "startTimestamp")]
    start_timestamp: u128,
    #[serde(rename = "endTimestamp")]
    end_timestamp: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    solution: Option<Solution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sessions: Option<Vec<String>>,
}

#[derive(Serialize)]
struct Solution {
    url: String,
    status: String,
    headers: serde_json::Value,
    response: String,
    cookies: Vec<CookieOut>,
    #[serde(rename = "userAgent")]
    user_agent: String,
}

#[derive(Serialize)]
struct CookieOut {
    name: String,
    value: String,
    domain: String,
    path: String,
    expires: f64,
    size: usize,
    #[serde(rename = "httpOnly")]
    http_only: bool,
    secure: bool,
    session: bool,
    #[serde(rename = "sameSite")]
    same_site: String,
}

impl From<NamedCookie> for CookieOut {
    fn from(cookie: NamedCookie) -> Self {
        Self {
            size: cookie.name.len() + cookie.value.len(),
            expires: cookie.expires.unwrap_or(0.0),
            session: cookie.expires.is_none(),
            http_only: cookie.http_only,
            secure: cookie.secure,
            name: cookie.name,
            value: cookie.value,
            domain: cookie.domain,
            path: cookie.path,
            same_site: "Lax".into(),
        }
    }
}

struct Reply {
    start: u128,
}

impl Reply {
    fn new() -> Self {
        Self { start: millis() }
    }

    fn ok(self, state: &AppState, solution: Option<Solution>) -> Json<Envelope> {
        Json(Envelope {
            status: "ok",
            message: "ok".into(),
            version: state.version.clone(),
            start_timestamp: self.start,
            end_timestamp: millis(),
            solution,
            session: None,
            sessions: None,
        })
    }

    fn error(self, state: &AppState, message: impl Into<String>) -> Json<Envelope> {
        Json(Envelope {
            status: "error",
            message: message.into(),
            version: state.version.clone(),
            start_timestamp: self.start,
            end_timestamp: millis(),
            solution: None,
            session: None,
            sessions: None,
        })
    }
}

fn millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or(0)
}

async fn ping(State(state): State<Arc<AppState>>) -> Json<Envelope> {
    Reply::new().ok(&state, None)
}

async fn v1(State(state): State<Arc<AppState>>, Json(request): Json<V1Request>) -> Json<Envelope> {
    let reply = Reply::new();
    match request.cmd.as_str() {
        "request.get" | "request.post" => request_page(&state, request, reply).await,
        "sessions.create" => {
            let mut envelope = reply.ok(&state, None).0;
            let id = Uuid::new_v4().to_string();
            envelope.message = "Session created successfully.".into();
            envelope.session = Some(id);
            Json(envelope)
        }
        "sessions.list" => {
            let mut envelope = reply.ok(&state, None).0;
            envelope.sessions = Some(Vec::new());
            Json(envelope)
        }
        "sessions.destroy" => Json(reply.ok(&state, None).0),
        other => reply.error(&state, format!("Unknown command: {other}")),
    }
}

async fn request_page(
    state: &AppState,
    request: V1Request,
    reply: Reply,
) -> Json<Envelope> {
    let Some(url) = request.url.clone() else {
        return reply.error(state, "Missing url");
    };
    let timeout = state
        .limits
        .default_timeout
        .min(Duration::from_millis(request.max_timeout.unwrap_or_else(|| {
            state.limits.default_timeout.as_millis() as u64
        })))
        .min(state.limits.max_timeout);

    let proxy = match request.proxy.as_ref().map(|proxy| proxy::Input {
        url: proxy.url.clone(),
        username: proxy.username.clone(),
        password: proxy.password.clone(),
    }) {
        Some(input) => match proxy::resolve(Some(&input)) {
            Ok(proxy) => proxy,
            Err(error) => return reply.error(state, error.to_string()),
        },
        None => None,
    };

    let host = cfsolve::Url::parse(&url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_default();
    let key = format!("{host}|{}", proxy.as_deref().unwrap_or("-"));
    let target = Target {
        url: url.clone(),
        proxy: proxy.clone(),
        deadline: Instant::now() + timeout,
    };

    let (status, body, cookies) = match obtain(state, &target, &key).await {
        Ok(page) => (page.status, page.body, page.cookies),
        Err(error) => return reply.error(state, describe(&error)),
    };

    if request.cmd == "request.post" {
        let body_text = request.post_data.clone().unwrap_or_default();
        let content_type = request
            .headers
            .as_ref()
            .and_then(|headers| headers.get("content-type").or_else(|| headers.get("Content-Type")))
            .and_then(|value| value.as_str())
            .unwrap_or("application/x-www-form-urlencoded")
            .to_string();
        let fetcher = match state.fetcher(proxy.as_deref()) {
            Ok(fetcher) => fetcher,
            Err(error) => return reply.error(state, error.to_string()),
        };
        let page = match fetcher.post(&url, &body_text, &content_type, &cookies, target.deadline).await {
            Ok(page) => page,
            Err(error) => return reply.error(state, error.to_string()),
        };
        return reply.ok(state, Some(solution(state, page.url, page.status, page.body, cookies)));
    }

    reply.ok(state, Some(solution(state, url, status, body, cookies)))
}

struct Page {
    url: String,
    status: u16,
    body: String,
    cookies: Vec<NamedCookie>,
}

async fn obtain(state: &AppState, target: &Target, key: &str) -> Result<Page, SolveError> {
    if let Some(cookies) = state.cache.get(key).await {
        let page = fetch(state, target, &cookies).await?;
        if !is_challenged(&page.body) {
            return Ok(Page { cookies, ..page });
        }
        state.cache.invalidate(key).await;
    }

    match solve(state, target, key).await {
        Ok(cookies) => {
            let page = fetch(state, target, &cookies).await?;
            if !is_challenged(&page.body) {
                return Ok(Page { cookies, ..page });
            }
            state.cache.invalidate(key).await;
            let cookies = solve(state, target, key).await?;
            let page = fetch(state, target, &cookies).await?;
            if is_challenged(&page.body) {
                return Err(SolveError::Challenged(page.status));
            }
            Ok(Page { cookies, ..page })
        }
        Err(SolveError::NotChallenged) | Err(SolveError::Blocked(_)) => {
            let page = fetch(state, target, &[]).await?;
            if !is_challenged(&page.body) {
                return Ok(page);
            }
            let cookies = solve(state, target, key).await?;
            let page = fetch(state, target, &cookies).await?;
            if is_challenged(&page.body) {
                return Err(SolveError::Challenged(page.status));
            }
            Ok(Page { cookies, ..page })
        }
        Err(error) => Err(error),
    }
}

async fn solve(state: &AppState, target: &Target, key: &str) -> Result<Vec<NamedCookie>, SolveError> {
    let expires = Instant::now() + Duration::from_secs(30 * 60);
    state
        .cache
        .get_or_solve(state.solver.as_ref(), key, clone_target(target), expires, state.limits.max_solve)
        .await
}

fn clone_target(target: &Target) -> Target {
    Target {
        url: target.url.clone(),
        proxy: target.proxy.clone(),
        deadline: target.deadline,
    }
}

async fn fetch(state: &AppState, target: &Target, cookies: &[NamedCookie]) -> Result<Page, SolveError> {
    let fetcher = state
        .fetcher(target.proxy.as_deref())
        .map_err(|error| SolveError::Network(error.to_string()))?;
    let page: Snapshot = fetcher
        .get(&target.url, cookies, target.deadline)
        .await
        .map_err(|error| SolveError::Network(error.to_string()))?;
    Ok(Page {
        url: page.url,
        status: page.status,
        body: page.body,
        cookies: Vec::new(),
    })
}

impl AppState {
    fn fetcher(&self, proxy: Option<&str>) -> anyhow::Result<crate::fetch::Fetcher> {
        crate::fetch::Fetcher::new(self.user_agent.clone(), self.header_order.clone(), proxy)
    }
}

fn solution(state: &AppState, url: String, status: u16, body: String, cookies: Vec<NamedCookie>) -> Solution {
    Solution {
        url,
        status: status.to_string(),
        headers: serde_json::json!({}),
        response: body,
        cookies: cookies.into_iter().map(CookieOut::from).collect(),
        user_agent: state.user_agent.clone(),
    }
}

fn describe(error: &SolveError) -> String {
    match error {
        SolveError::NotChallenged => "the page answered without a challenge".into(),
        SolveError::Blocked(status) => format!("the page answered HTTP {status}"),
        SolveError::Network(message) => format!("request failed: {message}"),
        SolveError::Rejected(message) => format!("Cloudflare rejected the solve: {message}"),
        SolveError::Challenged(status) => format!("still challenged after solving (HTTP {status})"),
        SolveError::Unsupported(message) => format!("unsupported: {message}"),
        SolveError::Config(message) => format!("config: {message}"),
    }
}

pub fn expired_cleared(cleared: Cleared) -> Cleared {
    cleared
}
