//! Asks a running FlareSolverr-compatible solver, such as Byparr, for a solve
//! using the same request Prowlarr sends.

use crate::outcome::Outcome;
use crate::replay::Cookie;
use anyhow::Result;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Solve {
    pub outcome: Outcome,
    pub cookies: Vec<Cookie>,
    pub user_agent: Option<String>,
    pub message: Option<String>,
}

pub async fn solve(solver_url: &str, target: &str, timeout: Duration) -> Result<Solve> {
    let request = json!({
        "cmd": "request.get",
        "url": target,
        "maxTimeout": timeout.as_millis() as u64,
    });
    // Prowlarr gives the solver five seconds more than the solve itself.
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(timeout + Duration::from_secs(5))
        .build()?;
    let reply: Value = client
        .post(format!("{}/v1", solver_url.trim_end_matches('/')))
        .json(&request)
        .send()
        .await?
        .json()
        .await?;

    let solution = &reply["solution"];
    let cookies: Vec<Cookie> = solution["cookies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|cookie| {
            Some(Cookie {
                name: cookie["name"].as_str()?.to_string(),
                value: cookie["value"].as_str()?.to_string(),
            })
        })
        .collect();

    let outcome = if reply["status"] != "ok" {
        Outcome::ChallengeTimeout
    } else if cookies.iter().any(|cookie| cookie.name == "cf_clearance") {
        Outcome::Cleared
    } else {
        Outcome::NoChallenge
    };

    Ok(Solve {
        outcome,
        cookies,
        user_agent: solution["userAgent"].as_str().map(String::from),
        message: reply["message"]
            .as_str()
            .filter(|message| !message.is_empty())
            .map(String::from),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::post;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    const TIMEOUT: Duration = Duration::from_secs(5);

    /// Serves one fixed reply on `/v1` and records the requests it receives.
    async fn fake_solver(status: StatusCode, reply: Value) -> (String, Arc<Mutex<Vec<Value>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let app = Router::new().route(
            "/v1",
            post(move |body: String| {
                let recorded = recorded.clone();
                let reply = reply.clone();
                async move {
                    recorded
                        .lock()
                        .unwrap()
                        .push(serde_json::from_str::<Value>(&body).unwrap());
                    (status, reply.to_string())
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await });
        (format!("http://127.0.0.1:{port}"), seen)
    }

    fn solved(cookies: Value) -> Value {
        json!({
            "status": "ok",
            "message": "",
            "solution": {"cookies": cookies, "userAgent": "agent-a", "status": 200}
        })
    }

    #[tokio::test]
    async fn sends_the_request_prowlarr_sends() {
        let (url, seen) = fake_solver(StatusCode::OK, solved(json!([]))).await;

        solve(&url, "https://indexer.test/", Duration::from_secs(60))
            .await
            .unwrap();

        assert_eq!(
            seen.lock().unwrap()[0],
            json!({"cmd": "request.get", "url": "https://indexer.test/", "maxTimeout": 60000})
        );
    }

    #[tokio::test]
    async fn a_clearance_cookie_means_cleared() {
        let cookies = json!([{"name": "cf_clearance", "value": "abc"}]);
        let (url, _) = fake_solver(StatusCode::OK, solved(cookies)).await;

        let result = solve(&url, "https://indexer.test/", TIMEOUT).await.unwrap();

        assert_eq!(result.outcome, Outcome::Cleared);
        assert_eq!(result.user_agent.as_deref(), Some("agent-a"));
        assert_eq!(result.cookies[0].value, "abc");
    }

    #[tokio::test]
    async fn success_without_clearance_means_no_challenge() {
        let cookies = json!([{"name": "session", "value": "abc"}]);
        let (url, _) = fake_solver(StatusCode::OK, solved(cookies)).await;

        let result = solve(&url, "https://indexer.test/", TIMEOUT).await.unwrap();

        assert_eq!(result.outcome, Outcome::NoChallenge);
    }

    #[tokio::test]
    async fn a_500_reply_is_a_failed_challenge_with_its_message() {
        let reply = json!({"status": "error", "message": "Challenge not solved"});
        let (url, _) = fake_solver(StatusCode::INTERNAL_SERVER_ERROR, reply).await;

        let result = solve(&url, "https://indexer.test/", TIMEOUT).await.unwrap();

        assert_eq!(result.outcome, Outcome::ChallengeTimeout);
        assert_eq!(result.message.as_deref(), Some("Challenge not solved"));
    }

    #[tokio::test]
    async fn an_unreachable_solver_is_an_error() {
        let result = solve("http://127.0.0.1:1", "https://indexer.test/", TIMEOUT).await;
        assert!(result.is_err());
    }
}
