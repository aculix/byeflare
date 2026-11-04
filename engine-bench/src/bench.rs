//! Runs one attempt against an engine or a baseline solver and records it.

use crate::attempt::{self, PageResult};
use crate::baseline;
use crate::cdp::Cdp;
use crate::detect::is_protected;
use crate::engine::{EngineSpec, LaunchOptions, Running};
use crate::memory::Sampler;
use crate::outcome::Outcome;
use crate::replay::{self, Cookie};
use crate::report::AttemptRecord;
use anyhow::{Context, Result};
use byeflare::domain::{SolveError, Target};
use byeflare::solver::{CfsolveSolver, Solver};
use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::time::timeout;

const CDP_CALL_TIMEOUT: Duration = Duration::from_secs(30);
const REPLAY_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Settings {
    pub timeout: Duration,
    pub options: LaunchOptions,
}

pub async fn engine_attempt(
    spec: &EngineSpec,
    label: &str,
    target: &str,
    round: u32,
    settings: &Settings,
) -> AttemptRecord {
    let mut record = AttemptRecord::failed(label, target, round);
    let running = match Running::start(spec, &settings.options).await {
        Ok(running) => running,
        Err(error) => {
            record.note = Some(format!("{error:#}"));
            return record;
        }
    };
    record.startup_ms = Some(running.startup_ms);

    let sampler = Sampler::start(running.pgid);
    let result = drive(&running.ws_url, target, settings.timeout).await;
    let (peak_mb, cpu_seconds) = sampler.finish().await;
    running.stop().await;
    record.peak_mb = Some(peak_mb);
    record.cpu_seconds = Some(cpu_seconds);

    match result {
        Ok(page) => {
            record.outcome = page.outcome;
            record.solve_ms = Some(page.solve_ms);
            let proxy = settings.options.proxy.as_deref();
            apply_replay(
                &mut record,
                target,
                &page.cookies,
                page.user_agent.as_deref(),
                proxy,
            )
            .await;
        }
        Err(error) => record.note = Some(format!("{error:#}")),
    }
    record
}

/// Bounds the whole attempt, because a hung engine can stall every CDP call.
async fn drive(ws_url: &str, target: &str, limit: Duration) -> Result<PageResult> {
    let work = async {
        let cdp = Cdp::connect(ws_url, CDP_CALL_TIMEOUT).await?;
        attempt::solve(&cdp, target, limit).await
    };
    timeout(limit + Duration::from_secs(10), work)
        .await
        .context("engine stopped responding")?
}

pub async fn baseline_attempt(
    name: &str,
    solver_url: &str,
    target: &str,
    round: u32,
    settings: &Settings,
) -> AttemptRecord {
    let mut record = AttemptRecord::failed(name, target, round);

    let started = Instant::now();
    let result = baseline::solve(solver_url, target, settings.timeout).await;
    let solve_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(solve) => {
            record.outcome = solve.outcome;
            record.solve_ms = Some(solve_ms);
            record.note = solve.message;
            let proxy = settings.options.proxy.as_deref();
            apply_replay(
                &mut record,
                target,
                &solve.cookies,
                solve.user_agent.as_deref(),
                proxy,
            )
            .await;
        }
        Err(error) => record.note = Some(format!("{error:#}")),
    }
    record
}

/// Runs cfsolve inside this process; its path is a profile, not a binary.
pub async fn cfsolve_attempt(
    spec: &EngineSpec,
    label: &str,
    target: &str,
    round: u32,
    settings: &Settings,
) -> AttemptRecord {
    let mut record = AttemptRecord::failed(label, target, round);

    let started = Instant::now();
    let solver = match CfsolveSolver::load(&spec.path) {
        Ok(solver) => solver,
        Err(error) => {
            record.note = Some(format!("{error:#}"));
            return record;
        }
    };
    record.startup_ms = Some(started.elapsed().as_millis() as u64);
    let user_agent = solver.user_agent().to_string();
    let target_spec = Target {
        url: target.to_string(),
        proxy: settings.options.proxy.clone(),
        deadline: Instant::now() + settings.timeout,
    };

    // cfsolve runs in this process, so the group to watch is our own.
    // SAFETY: `getpgrp` takes no arguments and cannot fail.
    let sampler = Sampler::start(unsafe { libc::getpgrp() } as u32);
    let started = Instant::now();
    let result = timeout(settings.timeout, solver.clear(&target_spec)).await;
    let (peak_mb, cpu_seconds) = sampler.finish().await;
    record.peak_mb = Some(peak_mb);
    record.cpu_seconds = Some(cpu_seconds);
    record.solve_ms = Some(started.elapsed().as_millis() as u64);

    let proxy = settings.options.proxy.as_deref();
    match result {
        Err(_) => {
            record.note = Some(format!(
                "no clearance within {}s",
                settings.timeout.as_secs()
            ))
        }
        Ok(Ok(cleared)) => {
            record.outcome = Outcome::Cleared;
            let cookies: Vec<Cookie> = cleared
                .cookies
                .iter()
                .map(|cookie| Cookie {
                    name: cookie.name.clone(),
                    value: cookie.value.clone(),
                })
                .collect();
            apply_replay(&mut record, target, &cookies, Some(&user_agent), proxy).await;
        }
        Ok(Err(error)) => match error {
            SolveError::NotChallenged => {
                record.outcome = Outcome::NoChallenge;
                apply_replay(&mut record, target, &[], Some(&user_agent), proxy).await;
            }
            SolveError::Blocked(status) => {
                record.outcome = Outcome::Blocked;
                record.note = Some(format!("replay target answered HTTP {status}"));
            }
            SolveError::Challenged(status) => {
                record.outcome = Outcome::ChallengeTimeout;
                record.note = Some(format!("still challenged, HTTP {status}"));
            }
            SolveError::Rejected(message) => {
                record.outcome = Outcome::ChallengeTimeout;
                record.note = Some(format!("rejected: {message}"));
            }
            SolveError::Network(message) => record.note = Some(format!("network: {message}")),
            SolveError::Unsupported(message) => {
                record.note = Some(format!("unsupported: {message}"))
            }
            SolveError::Config(message) => record.note = Some(format!("config: {message}")),
        },
    }
    record
}

/// Fills in whether Prowlarr's own replay would have worked.
pub async fn apply_replay(
    record: &mut AttemptRecord,
    target: &str,
    cookies: &[Cookie],
    user_agent: Option<&str>,
    proxy: Option<&str>,
) {
    if !matches!(record.outcome, Outcome::Cleared | Outcome::NoChallenge) {
        record.replay_ok = Some(false);
        return;
    }
    match replay::fetch(target, cookies, user_agent, proxy, REPLAY_TIMEOUT).await {
        Ok(response) => {
            let passed = !is_protected(&response) && response.status < 400;
            record.replay_ok = Some(passed);
            if !passed {
                record.note = Some(format!("replay got HTTP {}", response.status));
            }
        }
        Err(error) => {
            record.replay_ok = Some(false);
            record.note = Some(format!("replay failed: {error:#}"));
        }
    }
}

pub async fn probe(
    spec: &EngineSpec,
    url: &str,
    expression: &str,
    options: &LaunchOptions,
) -> Result<Value> {
    let running = Running::start(spec, options).await?;
    let result = async {
        let cdp = Cdp::connect(&running.ws_url, CDP_CALL_TIMEOUT).await?;
        attempt::evaluate_on(&cdp, url, expression).await
    }
    .await;
    running.stop().await;
    let text = result?;
    Ok(serde_json::from_str(
        text.as_str().context("probe returned no text")?,
    )?)
}
