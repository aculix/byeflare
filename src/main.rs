use anyhow::Context;
use byeflare::api::{AppState, Limits, app};
use byeflare::cache::Cache;
use byeflare::config::{self, Config};
use byeflare::solver::CfsolveSolver;
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let solver = Arc::new(
        CfsolveSolver::load(&config.profile)
            .with_context(|| format!("loading profile {}", config.profile.display()))?,
    );
    let user_agent = solver.user_agent().to_string();
    let header_order = config::header_order(&config.profile)?;

    let state = Arc::new(AppState {
        solver,
        cache: Cache::new(config.cache_capacity),
        user_agent,
        header_order,
        limits: Limits {
            default_timeout: config.default_timeout,
            max_timeout: config.max_timeout,
            max_solve: config.max_timeout + Duration::from_secs(10),
        },
        version: env!("CARGO_PKG_VERSION").into(),
        inflight: Arc::new(tokio::sync::Semaphore::new(config.inflight)),
    });

    let listener = tokio::net::TcpListener::bind(&config.bind)
        .await
        .with_context(|| format!("binding {}", config.bind))?;
    println!("Waiting idle. byeflare on http://{}", config.bind);
    axum::serve(listener, app(state)).await?;
    Ok(())
}
