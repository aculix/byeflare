//! Checks the harness and each engine against the local mock. It touches no
//! real site, so it says nothing about whether an engine passes Cloudflare.

use crate::bench::{self, Settings};
use crate::engine::{EngineSpec, Kind, LaunchOptions};
use crate::mock;
use crate::outcome::Outcome;
use crate::proxy::{self, Upstream};
use crate::report::AttemptRecord;
use anyhow::{Result, bail};
use serde_json::Value;
use std::time::Duration;

const SCENARIOS: [(&str, Outcome, bool); 5] = [
    ("/open", Outcome::NoChallenge, true),
    ("/challenge", Outcome::Cleared, true),
    ("/never", Outcome::ChallengeTimeout, false),
    ("/interactive", Outcome::InteractiveTimeout, false),
    ("/blocked", Outcome::Blocked, false),
];

const CAPABILITIES: &str = r#"(() => {
  const report = { userAgent: navigator.userAgent, webdriver: navigator.webdriver };
  const check = (name, test) => {
    try { report[name] = !!test(); } catch { report[name] = false; }
  };
  const canvas = () => document.createElement('canvas');
  check('canvas2d', () => {
    const element = canvas();
    element.width = element.height = 16;
    const context = element.getContext('2d');
    context.fillRect(0, 0, 8, 8);
    return element.toDataURL().length > 100;
  });
  check('webgl', () => canvas().getContext('webgl'));
  check('webassembly', () => typeof WebAssembly === 'object');
  check('cryptoSubtle', () => crypto.subtle);
  check('worker', () => typeof Worker === 'function');
  check('resourceTiming', () => performance.getEntriesByType('navigation').length > 0);
  check('userAgentData', () => navigator.userAgentData);
  check('plugins', () => navigator.plugins.length > 0);
  check('audioContext', () => window.OfflineAudioContext);
  check('webrtc', () => window.RTCPeerConnection);
  return JSON.stringify(report);
})()"#;

pub async fn run(engines: &[EngineSpec], no_sandbox: bool, timeout: Duration) -> Result<()> {
    let base = format!("http://{}", mock::start().await?);
    let settings = Settings {
        timeout,
        options: LaunchOptions {
            no_sandbox,
            local_targets: true,
            ..Default::default()
        },
    };

    let mut mismatches = 0;
    for spec in engines {
        println!("\n## {}\n", spec.name);
        if spec.kind == Kind::Cfsolve {
            println!("cfsolve runs no browser, so there is nothing to probe. Check it with `run`.");
            continue;
        }
        println!("| Scenario | Expected | Actual | Replay | Solve | Peak memory | CPU | Startup |");
        println!("|---|---|---|---|---|---|---|---|");
        for (path, expected, replay) in SCENARIOS {
            let target = format!("{base}{path}");
            let record = bench::engine_attempt(spec, &spec.name, &target, 1, &settings).await;
            let matched = record.outcome == expected && record.replay_ok == Some(replay);
            mismatches += usize::from(!matched);
            println!("{}", scenario_row(path, expected, &record, matched));
            if let Some(note) = &record.note {
                println!("| | note: {note} | | | | | | |");
            }
        }

        let record = proxied_attempt(spec, &base, &settings).await?;
        let matched = record.outcome == Outcome::Cleared && record.replay_ok == Some(true);
        mismatches += usize::from(!matched);
        println!(
            "{}",
            scenario_row("/challenge via proxy", Outcome::Cleared, &record, matched)
        );
        if let Some(note) = &record.note {
            println!("| | note: {note} | | | | | | |");
        }

        match bench::probe(
            spec,
            &format!("{base}/probe"),
            CAPABILITIES,
            &settings.options,
        )
        .await
        {
            Ok(report) => println!("\nCapabilities: {}", capability_line(&report)),
            Err(error) => println!("\nCapabilities: probe failed: {error:#}"),
        }
    }

    println!("\n{mismatches} scenario(s) did not match.");
    if mismatches > 0 {
        bail!("self-test found {mismatches} mismatch(es)");
    }
    Ok(())
}

/// Runs a challenge through a proxy that demands a login, on a hostname only
/// that proxy can resolve. An engine that ignores its proxy fails to load it.
async fn proxied_attempt(
    spec: &EngineSpec,
    base: &str,
    settings: &Settings,
) -> Result<AttemptRecord> {
    let proxy = mock::start_proxy(Some("user:pass")).await?;
    let upstream = Upstream::parse(&format!("http://user:pass@{}", proxy.address))?;
    let proxied = Settings {
        timeout: settings.timeout,
        options: LaunchOptions {
            proxy: Some(proxy::start(upstream).await?),
            ..settings.options.clone()
        },
    };
    let target = format!("{base}/challenge").replace("127.0.0.1", mock::PROXIED_HOST);
    Ok(bench::engine_attempt(spec, &spec.name, &target, 1, &proxied).await)
}

fn scenario_row(path: &str, expected: Outcome, record: &AttemptRecord, matched: bool) -> String {
    let mark = if matched { "ok" } else { "MISMATCH" };
    format!(
        "| {path} | {expected:?} | {:?} {mark} | {:?} | {:?} ms | {:.0} MB | {:.2} s | {:?} ms |",
        record.outcome,
        record.replay_ok,
        record.solve_ms.unwrap_or(0),
        record.peak_mb.unwrap_or(0.0),
        record.cpu_seconds.unwrap_or(0.0),
        record.startup_ms.unwrap_or(0),
    )
}

fn capability_line(report: &Value) -> String {
    let Some(fields) = report.as_object() else {
        return report.to_string();
    };
    let missing: Vec<&str> = fields
        .iter()
        .filter(|(_, value)| **value == Value::Bool(false))
        .filter(|(name, _)| name.as_str() != "webdriver")
        .map(|(name, _)| name.as_str())
        .collect();
    format!(
        "missing [{}], webdriver={}, userAgent={}",
        missing.join(", "),
        report["webdriver"],
        report["userAgent"]
    )
}
