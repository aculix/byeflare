mod attempt;
mod baseline;
mod bench;
mod cdp;
mod detect;
mod engine;
mod exits;
mod memory;
mod mock;
mod outcome;
mod proxy;
mod replay;
mod report;
mod selftest;

use anyhow::{Context, Result, bail};
use bench::Settings;
use clap::{Args, Parser, Subcommand};
use detect::is_protected;
use engine::{EngineSpec, LaunchOptions};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::sleep;

/// A desktop browser's user agent, so the first check isn't refused for lacking one.
const CHECK_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36";

#[derive(Parser)]
#[command(about = "Compares browser engines on the job Prowlarr needs from a solver")]
struct Cli {
    /// With no command it waits idle, because hosts restart anything that exits.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Test engines against real indexer URLs. Run it on the server Prowlarr uses.
    Run(RunArgs),
    /// Check the harness and engines against a local mock. Touches no real site.
    Selftest(SelftestArgs),
    /// Serve the local mock, for trying `run` without touching a real site.
    Mock,
    /// Wait without doing anything, so a hosted container stays up for a shell.
    Idle,
}

#[derive(Args)]
struct RunArgs {
    /// File with one indexer URL per line. Blank lines and # comments are skipped.
    #[arg(long)]
    targets: PathBuf,
    /// Engine as name:/path, for example chrome:/usr/bin/chromium. Repeatable.
    #[arg(long = "engine")]
    engines: Vec<EngineSpec>,
    /// URL of a running FlareSolverr-compatible solver to compare against.
    #[arg(long)]
    baseline: Option<String>,
    #[arg(long, default_value_t = 3)]
    rounds: u32,
    /// Seconds allowed per attempt. Match Prowlarr's request timeout.
    #[arg(long, default_value_t = 60)]
    timeout: u64,
    /// Seconds to wait between attempts, to keep the load on each site small.
    #[arg(long, default_value_t = 10)]
    pause: u64,
    /// Route to test: `direct`, or the name of an environment variable holding
    /// a proxy URL. Repeat it to compare routes in one table.
    #[arg(long = "exit", default_value = exits::DIRECT)]
    exits: Vec<String>,
    /// Needed when running as root inside a container.
    #[arg(long)]
    no_sandbox: bool,
    #[arg(long, default_value = "results")]
    out: PathBuf,
}

#[derive(Args)]
struct SelftestArgs {
    #[arg(long = "engine")]
    engines: Vec<EngineSpec>,
    #[arg(long)]
    no_sandbox: bool,
    #[arg(long, default_value_t = 4)]
    timeout: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Some(Command::Run(args)) => run(args).await,
        Some(Command::Selftest(args)) => {
            selftest::run(
                &args.engines,
                args.no_sandbox,
                Duration::from_secs(args.timeout),
            )
            .await
        }
        Some(Command::Mock) => {
            let address = mock::start().await?;
            let proxy = mock::start_proxy(Some("user:pass")).await?;
            println!("Mock listening on http://{address}");
            println!("Mock proxy on http://user:pass@{}", proxy.address);
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
        Some(Command::Idle) | None => idle().await,
    }
}

async fn idle() -> Result<()> {
    println!("Waiting idle. Open a shell and run engine-bench --help to see the commands.");
    // As PID 1 the process only stops on signals it handles itself.
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = terminate.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    Ok(())
}

async fn run(args: RunArgs) -> Result<()> {
    if args.engines.is_empty() && args.baseline.is_none() {
        bail!("give at least one --engine or a --baseline");
    }
    let targets = read_targets(&args.targets)?;
    let routes = usable_exits(&args.exits).await?;
    if args.baseline.is_some() && routes.iter().all(|exit| exit.proxy.is_some()) {
        println!("--baseline only runs on the direct exit, so it will be skipped");
    }

    let mut checks = Vec::new();
    for exit in &routes {
        for target in &targets {
            let protected = first_check(target, exit.proxy.as_deref()).await;
            println!("{target} via {}: {protected}", exit.name);
            checks.push(json!({"target": target, "exit": exit.name, "first_request": protected}));
        }
    }

    // Rounds run across every engine, so a site's mood at one moment can't
    // favour whichever engine happened to run then.
    let mut records = Vec::new();
    for round in 1..=args.rounds {
        for target in &targets {
            for exit in &routes {
                let settings = Settings {
                    timeout: Duration::from_secs(args.timeout),
                    options: LaunchOptions {
                        proxy: exit.proxy.clone(),
                        no_sandbox: args.no_sandbox,
                        local_targets: false,
                    },
                };
                for spec in &args.engines {
                    let label = exits::label(&spec.name, &exit.name);
                    let record =
                        bench::engine_attempt(spec, &label, target, round, &settings).await;
                    println!(
                        "round {round} {label} {target}: {:?} {:?}",
                        record.outcome, record.note
                    );
                    records.push(record);
                    sleep(Duration::from_secs(args.pause)).await;
                }
                // Byparr sets its own proxy, so only the direct route compares fairly.
                if let (Some(solver), None) = (&args.baseline, &exit.proxy) {
                    let record =
                        bench::baseline_attempt("baseline", solver, target, round, &settings).await;
                    println!(
                        "round {round} baseline {target}: {:?} {:?}",
                        record.outcome, record.note
                    );
                    records.push(record);
                    sleep(Duration::from_secs(args.pause)).await;
                }
            }
        }
    }

    let summaries = report::summarize(&records);
    println!("\n{}", report::render_table(&summaries));
    write_results(
        &args.out,
        &json!({"checks": checks, "records": records, "summaries": summaries}),
    )
}

/// Prints where each exit really leaves from, and drops any that don't work.
async fn usable_exits(names: &[String]) -> Result<Vec<exits::Exit>> {
    let mut usable = Vec::new();
    for exit in exits::resolve(names).await? {
        match exits::describe(&exit).await {
            Ok(address) => {
                println!("exit {}: {address}", exit.name);
                usable.push(exit);
            }
            Err(error) => println!("exit {}: skipped, {error:#}", exit.name),
        }
    }
    if usable.is_empty() {
        bail!("no exit could reach the internet");
    }
    Ok(usable)
}

fn read_targets(path: &Path) -> Result<Vec<String>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let targets: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(String::from)
        .collect();
    if targets.is_empty() {
        bail!("{} has no targets", path.display());
    }
    Ok(targets)
}

/// Prowlarr only calls a solver after its own request is challenged, so this
/// shows whether a target tests anything from this IP.
async fn first_check(target: &str, proxy: Option<&str>) -> String {
    match replay::fetch(
        target,
        &[],
        Some(CHECK_AGENT),
        proxy,
        Duration::from_secs(30),
    )
    .await
    {
        Ok(response) if is_protected(&response) => format!("protected (HTTP {})", response.status),
        Ok(response) => format!("not protected (HTTP {})", response.status),
        Err(error) => format!("failed: {error:#}"),
    }
}

fn write_results(directory: &Path, results: &serde_json::Value) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let path = directory.join(format!("run-{stamp}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(results)?)?;
    println!("Results saved to {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_uses_the_direct_exit_by_default() {
        let cli = Cli::try_parse_from(["engine-bench", "run", "--targets", "t"]).unwrap();
        let Some(Command::Run(args)) = cli.command else {
            panic!("expected the run command");
        };
        assert_eq!(args.exits, [exits::DIRECT]);
    }

    #[test]
    fn exits_can_be_repeated() {
        let cli = Cli::try_parse_from([
            "engine-bench",
            "run",
            "--targets",
            "t",
            "--exit",
            "direct",
            "--exit",
            "PROXY_A",
        ])
        .unwrap();
        let Some(Command::Run(args)) = cli.command else {
            panic!("expected the run command");
        };
        assert_eq!(args.exits, ["direct", "PROXY_A"]);
    }

    #[test]
    fn no_command_is_allowed_so_a_host_cant_crash_loop_it() {
        assert!(Cli::try_parse_from(["engine-bench"]).is_ok());
    }

    #[test]
    fn an_unknown_command_is_still_an_error() {
        assert!(Cli::try_parse_from(["engine-bench", "nonsense"]).is_err());
    }

    #[test]
    fn idle_needs_no_arguments() {
        let cli = Cli::try_parse_from(["engine-bench", "idle"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Idle)));
    }
}
