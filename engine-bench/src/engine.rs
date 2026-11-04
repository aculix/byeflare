//! Launches each engine with stock settings and finds its CDP endpoint.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::sleep;

const STARTUP_LIMIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Chrome,
    ChromeXvfb,
    HeadlessShell,
    Lightpanda,
    Obscura,
    ObscuraStealth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineSpec {
    pub name: String,
    pub kind: Kind,
    pub path: PathBuf,
}

impl FromStr for EngineSpec {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let Some((name, path)) = text.split_once(':') else {
            bail!("expected name:/path/to/engine, got {text}");
        };
        let kind = match name {
            "chrome" => Kind::Chrome,
            "chrome-xvfb" => Kind::ChromeXvfb,
            "headless-shell" => Kind::HeadlessShell,
            "lightpanda" => Kind::Lightpanda,
            "obscura" => Kind::Obscura,
            "obscura-stealth" => Kind::ObscuraStealth,
            _ => bail!("unknown engine {name}"),
        };
        Ok(Self {
            name: name.to_string(),
            kind,
            path: path.into(),
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    pub proxy: Option<String>,
    /// Needed when running as root inside a container.
    pub no_sandbox: bool,
    /// Lets engines that block private addresses reach the local mock.
    pub local_targets: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

pub fn launch_plan(
    spec: &EngineSpec,
    port: u16,
    profile: &Path,
    options: &LaunchOptions,
) -> LaunchPlan {
    let path = spec.path.display().to_string();
    match spec.kind {
        Kind::Chrome => LaunchPlan {
            program: path,
            args: chrome_args(port, profile, options, true),
            env: Vec::new(),
        },
        Kind::HeadlessShell => LaunchPlan {
            program: path,
            args: chrome_args(port, profile, options, false),
            env: Vec::new(),
        },
        Kind::ChromeXvfb => {
            let mut args = vec!["-a".to_string(), path];
            args.extend(chrome_args(port, profile, options, false));
            LaunchPlan {
                program: "xvfb-run".to_string(),
                args,
                env: Vec::new(),
            }
        }
        Kind::Lightpanda => LaunchPlan {
            program: path,
            args: lightpanda_args(port, options),
            env: vec![("LIGHTPANDA_DISABLE_TELEMETRY".into(), "true".into())],
        },
        Kind::Obscura | Kind::ObscuraStealth => LaunchPlan {
            program: path,
            args: obscura_args(spec.kind, port, options),
            env: Vec::new(),
        },
    }
}

fn chrome_args(port: u16, profile: &Path, options: &LaunchOptions, headless: bool) -> Vec<String> {
    let mut args = vec![
        format!("--remote-debugging-port={port}"),
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-dev-shm-usage".to_string(),
    ];
    if headless {
        args.push("--headless=new".to_string());
    }
    if options.no_sandbox {
        args.push("--no-sandbox".to_string());
    }
    if let Some(proxy) = &options.proxy {
        args.push(format!("--proxy-server={proxy}"));
    }
    args.push("about:blank".to_string());
    args
}

fn lightpanda_args(port: u16, options: &LaunchOptions) -> Vec<String> {
    let mut args: Vec<String> = [
        "serve",
        "--host",
        "127.0.0.1",
        "--log-level",
        "warn",
        "--port",
    ]
    .map(String::from)
    .to_vec();
    args.push(port.to_string());
    if let Some(proxy) = &options.proxy {
        args.extend(["--http-proxy".to_string(), proxy.clone()]);
    }
    args
}

fn obscura_args(kind: Kind, port: u16, options: &LaunchOptions) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(proxy) = &options.proxy {
        args.extend(["--proxy".to_string(), proxy.clone()]);
    }
    args.extend(["serve".to_string(), "--port".to_string(), port.to_string()]);
    if kind == Kind::ObscuraStealth {
        args.push("--stealth".to_string());
    }
    if options.local_targets {
        args.push("--allow-private-network".to_string());
    }
    args
}

/// Reads the WebSocket URL from a `/json/version` body. Engines without that
/// endpoint serve CDP from the root path.
pub fn websocket_url(version_body: Option<&str>, port: u16) -> String {
    version_body
        .and_then(|body| serde_json::from_str::<Value>(body).ok())
        .and_then(|version| version["webSocketDebuggerUrl"].as_str().map(String::from))
        .unwrap_or_else(|| format!("ws://127.0.0.1:{port}/"))
}

/// An engine process running in its own process group.
pub struct Running {
    child: Child,
    pub pgid: u32,
    pub ws_url: String,
    pub startup_ms: u64,
    profile: PathBuf,
}

impl Running {
    pub async fn start(spec: &EngineSpec, options: &LaunchOptions) -> Result<Self> {
        let port = free_port()?;
        let profile =
            std::env::temp_dir().join(format!("engine-bench-{}-{port}", std::process::id()));
        std::fs::create_dir_all(&profile)?;

        let plan = launch_plan(spec, port, &profile, options);
        let started = Instant::now();
        let mut child = Command::new(&plan.program)
            .args(&plan.args)
            .envs(plan.env)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("launching {}", spec.name))?;
        let pgid = child.id().context("engine exited during launch")?;

        while started.elapsed() < STARTUP_LIMIT {
            if let Some(status) = child.try_wait()? {
                bail!("{} exited during startup: {status}", spec.name);
            }
            if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                let body = version_body(port).await;
                return Ok(Self {
                    child,
                    pgid,
                    ws_url: websocket_url(body.as_deref(), port),
                    startup_ms: started.elapsed().as_millis() as u64,
                    profile,
                });
            }
            sleep(Duration::from_millis(20)).await;
        }

        kill_group(pgid);
        let _ = std::fs::remove_dir_all(&profile);
        bail!("{} did not open its CDP port", spec.name)
    }

    pub async fn stop(mut self) {
        kill_group(self.pgid);
        let _ = self.child.wait().await;
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

async fn version_body(port: u16) -> Option<String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .ok()?;
    let response = client
        .get(format!("http://127.0.0.1:{port}/json/version"))
        .send()
        .await
        .ok()?;
    response.text().await.ok()
}

fn kill_group(pgid: u32) {
    // SAFETY: `kill` takes no pointers, and the negative pid only reaches the
    // process group this harness created for the engine.
    unsafe {
        libc::kill(-(pgid as i32), libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: Kind) -> EngineSpec {
        EngineSpec {
            name: "test".into(),
            kind,
            path: "/bin/engine".into(),
        }
    }

    fn plan(kind: Kind, options: &LaunchOptions) -> LaunchPlan {
        launch_plan(&spec(kind), 9333, Path::new("/tmp/profile"), options)
    }

    fn has(plan: &LaunchPlan, arg: &str) -> bool {
        plan.args.iter().any(|candidate| candidate == arg)
    }

    #[test]
    fn parses_engine_specs() {
        let parsed: EngineSpec = "headless-shell:/opt/chs".parse().unwrap();
        assert_eq!(parsed.name, "headless-shell");
        assert_eq!(parsed.kind, Kind::HeadlessShell);
        assert_eq!(parsed.path, PathBuf::from("/opt/chs"));
    }

    #[test]
    fn engine_paths_may_contain_colons() {
        let parsed: EngineSpec = "chrome:/opt/a:b/chrome".parse().unwrap();
        assert_eq!(parsed.path, PathBuf::from("/opt/a:b/chrome"));
    }

    #[test]
    fn rejects_unknown_engines() {
        assert!("netscape:/opt/nav".parse::<EngineSpec>().is_err());
    }

    #[test]
    fn rejects_specs_without_a_path() {
        assert!("chrome".parse::<EngineSpec>().is_err());
    }

    #[test]
    fn chrome_runs_new_headless_with_its_own_profile() {
        let chrome = plan(Kind::Chrome, &LaunchOptions::default());
        assert_eq!(chrome.program, "/bin/engine");
        assert!(has(&chrome, "--headless=new"));
        assert!(has(&chrome, "--remote-debugging-port=9333"));
        assert!(has(&chrome, "--user-data-dir=/tmp/profile"));
    }

    #[test]
    fn chrome_never_gets_stealth_flags() {
        let chrome = plan(Kind::Chrome, &LaunchOptions::default());
        assert!(
            !chrome
                .args
                .iter()
                .any(|arg| arg.contains("AutomationControlled"))
        );
    }

    #[test]
    fn xvfb_wraps_chrome_and_runs_headful() {
        let chrome = plan(Kind::ChromeXvfb, &LaunchOptions::default());
        assert_eq!(chrome.program, "xvfb-run");
        assert_eq!(chrome.args[..2], ["-a", "/bin/engine"]);
        assert!(!has(&chrome, "--headless=new"));
    }

    #[test]
    fn headless_shell_needs_no_headless_flag() {
        let shell = plan(Kind::HeadlessShell, &LaunchOptions::default());
        assert!(!has(&shell, "--headless=new"));
        assert!(has(&shell, "--remote-debugging-port=9333"));
    }

    #[test]
    fn sandbox_stays_on_unless_asked() {
        assert!(!has(
            &plan(Kind::Chrome, &LaunchOptions::default()),
            "--no-sandbox"
        ));
        let options = LaunchOptions {
            no_sandbox: true,
            ..Default::default()
        };
        assert!(has(&plan(Kind::Chrome, &options), "--no-sandbox"));
    }

    #[test]
    fn chrome_takes_the_proxy_as_a_flag() {
        let options = LaunchOptions {
            proxy: Some("socks5://10.0.0.2:1080".into()),
            ..Default::default()
        };
        assert!(has(
            &plan(Kind::Chrome, &options),
            "--proxy-server=socks5://10.0.0.2:1080"
        ));
    }

    #[test]
    fn lightpanda_serves_cdp_with_telemetry_off() {
        let panda = plan(Kind::Lightpanda, &LaunchOptions::default());
        assert_eq!(panda.args[0], "serve");
        assert!(has(&panda, "9333"));
        assert!(
            panda
                .env
                .contains(&("LIGHTPANDA_DISABLE_TELEMETRY".into(), "true".into()))
        );
    }

    #[test]
    fn lightpanda_takes_the_proxy_as_a_flag() {
        let options = LaunchOptions {
            proxy: Some("http://10.0.0.2:3128".into()),
            ..Default::default()
        };
        let panda = plan(Kind::Lightpanda, &options);
        assert!(has(&panda, "--http-proxy"));
        assert!(has(&panda, "http://10.0.0.2:3128"));
    }

    #[test]
    fn obscura_stealth_is_a_separate_engine() {
        assert!(!has(
            &plan(Kind::Obscura, &LaunchOptions::default()),
            "--stealth"
        ));
        assert!(has(
            &plan(Kind::ObscuraStealth, &LaunchOptions::default()),
            "--stealth"
        ));
    }

    #[test]
    fn obscura_reaches_private_addresses_only_for_the_mock() {
        assert!(!has(
            &plan(Kind::Obscura, &LaunchOptions::default()),
            "--allow-private-network"
        ));
        let options = LaunchOptions {
            local_targets: true,
            ..Default::default()
        };
        assert!(has(
            &plan(Kind::Obscura, &options),
            "--allow-private-network"
        ));
    }

    #[test]
    fn obscura_proxy_goes_before_the_subcommand() {
        let options = LaunchOptions {
            proxy: Some("socks5://10.0.0.2:1080".into()),
            ..Default::default()
        };
        let obscura = plan(Kind::Obscura, &options);
        assert_eq!(
            obscura.args[..3],
            ["--proxy", "socks5://10.0.0.2:1080", "serve"]
        );
    }

    #[test]
    fn reads_the_websocket_url_from_json_version() {
        let body = r#"{"webSocketDebuggerUrl":"ws://127.0.0.1:9333/devtools/browser/abc"}"#;
        assert_eq!(
            websocket_url(Some(body), 9333),
            "ws://127.0.0.1:9333/devtools/browser/abc"
        );
    }

    #[test]
    fn falls_back_to_the_root_path_without_json_version() {
        assert_eq!(websocket_url(None, 9333), "ws://127.0.0.1:9333/");
        assert_eq!(
            websocket_url(Some("not json"), 9333),
            "ws://127.0.0.1:9333/"
        );
    }
}
