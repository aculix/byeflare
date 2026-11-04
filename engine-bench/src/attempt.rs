//! One solve: open the target in an engine and watch the page until it settles.
//! The harness only observes. It never clicks a widget or changes the page.

use crate::cdp::Cdp;
use crate::outcome::{Outcome, PageSnapshot, Verdict, at_deadline, classify};
use crate::replay::Cookie;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::time::sleep;

const POLL_EVERY: Duration = Duration::from_millis(200);

const SNAPSHOT: &str = r#"JSON.stringify({
  title: document.title,
  href: location.href,
  readyState: document.readyState,
  body: document.body ? document.body.innerText.slice(0, 200) : '',
  chlOpt: typeof window._cf_chl_opt !== 'undefined',
  turnstile: !!document.querySelector(
    'iframe[src*="challenges.cloudflare.com"], input[name="cf-turnstile-response"]'
  )
})"#;

#[derive(Debug)]
pub struct PageResult {
    pub outcome: Outcome,
    pub solve_ms: u64,
    pub cookies: Vec<Cookie>,
    pub user_agent: Option<String>,
}

pub async fn solve(cdp: &Cdp, url: &str, timeout: Duration) -> Result<PageResult> {
    let session = open_page(cdp).await?;
    let started = Instant::now();
    navigate(cdp, &session, url).await?;

    let mut last = None;
    let mut finished_at = None;
    while started.elapsed() < timeout {
        sleep(POLL_EVERY).await;

        // Evaluating fails while the page is between navigations.
        let Some(page) = snapshot(cdp, &session).await else {
            continue;
        };
        let cookies = read_cookies(cdp, &session, &[url, &page.href]).await?;
        let has_clearance = cookies.iter().any(|cookie| cookie.name == "cf_clearance");

        let settled = if page.looks_finished() {
            finished_at.get_or_insert_with(Instant::now).elapsed()
        } else {
            finished_at = None;
            Duration::ZERO
        };

        if let Verdict::Done(outcome) = classify(&page, has_clearance, settled) {
            return Ok(PageResult {
                outcome,
                solve_ms: started.elapsed().as_millis() as u64,
                cookies,
                user_agent: user_agent(cdp, &session).await,
            });
        }
        last = Some(page);
    }

    Ok(PageResult {
        outcome: at_deadline(last.as_ref()),
        solve_ms: started.elapsed().as_millis() as u64,
        cookies: Vec::new(),
        user_agent: None,
    })
}

/// Evaluates `expression` in a fresh page once `url` has loaded.
pub async fn evaluate_on(cdp: &Cdp, url: &str, expression: &str) -> Result<Value> {
    let session = open_page(cdp).await?;
    navigate(cdp, &session, url).await?;
    sleep(Duration::from_millis(500)).await;
    evaluate(cdp, &session, expression).await
}

async fn open_page(cdp: &Cdp) -> Result<String> {
    let target = cdp
        .call("Target.createTarget", json!({"url": "about:blank"}), None)
        .await?;
    let target_id = target["targetId"].as_str().context("no targetId")?;
    let attached = cdp
        .call(
            "Target.attachToTarget",
            json!({"targetId": target_id, "flatten": true}),
            None,
        )
        .await?;
    let session = attached["sessionId"].as_str().context("no sessionId")?;
    Ok(session.to_string())
}

async fn navigate(cdp: &Cdp, session: &str, url: &str) -> Result<()> {
    cdp.call("Page.enable", json!({}), Some(session)).await?;
    let reply = cdp
        .call("Page.navigate", json!({"url": url}), Some(session))
        .await?;
    if let Some(error) = reply["errorText"].as_str().filter(|text| !text.is_empty()) {
        bail!("navigation failed: {error}");
    }
    Ok(())
}

async fn evaluate(cdp: &Cdp, session: &str, expression: &str) -> Result<Value> {
    let reply = cdp
        .call(
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
            Some(session),
        )
        .await?;
    if let Some(error) = reply.get("exceptionDetails") {
        bail!("page script failed: {error}");
    }
    Ok(reply["result"]["value"].clone())
}

async fn snapshot(cdp: &Cdp, session: &str) -> Option<PageSnapshot> {
    let value = evaluate(cdp, session, SNAPSHOT).await.ok()?;
    serde_json::from_str(value.as_str()?).ok()
}

async fn user_agent(cdp: &Cdp, session: &str) -> Option<String> {
    let value = evaluate(cdp, session, "navigator.userAgent").await.ok()?;
    value.as_str().map(String::from)
}

async fn read_cookies(cdp: &Cdp, session: &str, urls: &[&str]) -> Result<Vec<Cookie>> {
    let reply = match cdp
        .call("Network.getCookies", json!({"urls": urls}), Some(session))
        .await
    {
        Ok(reply) => reply,
        Err(_) => cdp.call("Storage.getCookies", json!({}), None).await?,
    };
    Ok(reply["cookies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|cookie| {
            Some(Cookie {
                name: cookie["name"].as_str()?.to_string(),
                value: cookie["value"].as_str()?.to_string(),
            })
        })
        .collect())
}
