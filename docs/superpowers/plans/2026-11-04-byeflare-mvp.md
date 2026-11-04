# byeflare MVP Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a browserless, FlareSolverr-compatible `/v1` service that Prowlarr can use as a drop-in proxy.

**Architecture:** One Rust crate at the repo root. An axum server dispatches FlareSolverr commands into a `Solver` trait backed by `cfsolve`, with a cache and single-flight in front and a separate `wreq` client for non-solve traffic. A workspace member adapter in `engine-bench` reuses the same solver for the VPS validation gate. No browser process.

**Tech Stack:** Rust 1.98.0, axum, tokio, wreq (cfsolve's patched build), cfsolve at `ee4031f`, serde. Tests use axum locally and a fake `Solver`.

Spec: `docs/superpowers/specs/2026-11-04-browserless-solver-design.md`.

**Conventions:** every commit message ends with `Co-Authored-By: Claude Code <noreply@anthropic.com>`. Run commands from the repo root. `vendor/` is gitignored.

---

## File structure

| Path | Responsibility |
|---|---|
| `Cargo.toml` | Workspace + service package, patches, deps |
| `rust-toolchain.toml` | Pin 1.98.0 |
| `scripts/fetch-cfsolve.sh` | Clone cfsolve at `ee4031f` into `vendor/cfsolve` |
| `src/lib.rs` | Module list, re-exports |
| `src/domain.rs` | `Target`, `Cleared`, `NamedCookie`, `SolveError` |
| `src/detect.rs` | Challenge markers in HTML |
| `src/solver.rs` | `Solver` trait, `CfsolveSolver`, error mapping |
| `src/proxy.rs` | Prowlarr proxy DTO to URL |
| `src/cache.rs` | TTL/LRU cache + single-flight |
| `src/fetch.rs` | Non-solve HTTP client |
| `src/api.rs` | `/v1` and `/ping`, envelopes |
| `src/config.rs` | Environment config |
| `src/main.rs` | Wiring, idle |
| `tests/api.rs` | End-to-end API tests with a fake solver |
| `Dockerfile`, `.dockerignore` | Deployment |
| `README.md` | Run and deploy |
| `engine-bench/src/cfsolve_engine.rs` | Gate adapter |

---

## Chunk 1: Buildable skeleton

### Task 1: Workspace, cfsolve, and first compile

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `scripts/fetch-cfsolve.sh`, `.gitignore` (append), `src/lib.rs`, `src/main.rs`
- Modify: `engine-bench/Cargo.toml`

- [ ] **Step 1: Write the fetch script**

```bash
#!/usr/bin/env bash
set -euo pipefail
rev=ee4031fc9eb65702718f7ffbfedb451ad6bba541
dest=vendor/cfsolve
if [ ! -d "$dest/.git" ]; then
  git clone --filter=blob:none https://github.com/chris124567/cfsolve "$dest"
fi
git -C "$dest" fetch --quiet origin "$rev"
git -C "$dest" checkout --quiet "$rev"
```

- [ ] **Step 2: Run it and confirm**

Run: `chmod +x scripts/fetch-cfsolve.sh && ./scripts/fetch-cfsolve.sh && ls vendor/cfsolve/profiles/safari-26-ios.json`
Expected: path printed.

- [ ] **Step 3: Write `Cargo.toml`**

```toml
[workspace]
members = [".", "engine-bench"]
resolver = "3"

[package]
name = "byeflare"
version = "0.1.0"
edition = "2024"

[dependencies]
anyhow = "1"
async-trait = "0.1"
axum = "0.8"
cfsolve = { path = "vendor/cfsolve", default-features = false }
lru = "0.12"
percent-encoding = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
uuid = { version = "1", features = ["v4"] }
wreq = { version = "=0.16.1", features = ["cookies", "gzip", "brotli", "deflate", "zstd", "socks"] }

[dev-dependencies]
tokio = { version = "1", features = ["full", "test-util"] }

[patch.crates-io]
btls-sys = { path = "vendor/cfsolve/vendor/btls-sys" }
wreq = { path = "vendor/cfsolve/vendor/wreq" }
http2 = { path = "vendor/cfsolve/vendor/http2" }
```

Note: remove `byeflare = { path = "." }` if Cargo objects to a package depending on itself; if it does, drop it and use `crate::` internally, adding a `path = "."` only in `engine-bench`.

- [ ] **Step 4: Write `rust-toolchain.toml` and append ignores**

```toml
[toolchain]
channel = "1.98.0"
components = ["clippy", "rustfmt"]
profile = "minimal"
```

Append to `.gitignore`: `vendor/`, `profiles/`, `*.log`.

- [ ] **Step 5: Add the engine-bench dependency**

In `engine-bench/Cargo.toml` add under `[dependencies]`:

```toml
byeflare = { path = ".." }
```

and add the same `[patch.crates-io]` block. Wait — patches are only read from the workspace root; duplicate them only if `cargo` complains.

- [ ] **Step 6: Minimal `src/lib.rs` and `src/main.rs`**

`src/lib.rs`:

```rust
pub fn banner() -> &'static str {
    "byeflare"
}
```

`src/main.rs`:

```rust
fn main() {
    println!("{}", byeflare::banner());
}
```

- [ ] **Step 7: Build**

Run: `./scripts/fetch-cfsolve.sh && cargo build`
Expected: cfsolve and BoringSSL compile. This is the first real gate; expect a few minutes.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml rust-toolchain.toml scripts src .gitignore engine-bench/Cargo.toml
git commit -m "Scaffold byeflare with pinned cfsolve"
```

---

## Chunk 2: Solver and support units

### Task 2: Domain, detection, solver trait

**Files:**
- Create: `src/domain.rs`, `src/detect.rs`, `src/solver.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write failing detection tests** in `src/detect.rs`

Markers: `Just a moment`, `cf-browser-verification`, `challenge-platform`, `cf-challenge`, `cf-turnstile`. Case-insensitive.

```rust
#[cfg(test)]
mod tests {
    use super::is_challenged;

    #[test]
    fn detects_markers() {
        assert!(is_challenged("<title>Just a moment...</title>"));
        assert!(is_challenged("https://challenges.cloudflare.com/cdn-cgi/challenge-platform/x"));
        assert!(is_challenged("<div class=\"cf-turnstile\"></div>"));
    }

    #[test]
    fn ignores_clean_pages() {
        assert!(!is_challenged("<html><body>results</body></html>"));
    }
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test detect -- --nocapture`
Expected: `cannot find function is_challenged`.

- [ ] **Step 3: Implement `detect.rs`**

```rust
const MARKERS: &[&str] = &[
    "just a moment",
    "cf-browser-verification",
    "challenge-platform",
    "cf-challenge",
    "cf-turnstile",
];

pub fn is_challenged(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    MARKERS.iter().any(|marker| lower.contains(marker))
}
```

- [ ] **Step 4: Verify pass**

Run: `cargo test detect`
Expected: PASS.

- [ ] **Step 5: Write `domain.rs`**

```rust
use std::time::Instant;

#[derive(Debug)]
pub struct Target {
    pub url: String,
    pub proxy: Option<String>,
    pub deadline: Instant,
}

#[derive(Debug, Clone)]
pub struct NamedCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub expires: Option<f64>,
    pub http_only: bool,
    pub secure: bool,
}

#[derive(Debug)]
pub struct Cleared {
    pub url: String,
    pub status: u16,
    pub body: String,
    pub cookies: Vec<NamedCookie>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolveError {
    NotChallenged,
    Blocked(u16),
    Network(String),
    Rejected(String),
    Challenged(u16),
    Unsupported(String),
    Config(String),
}
```

- [ ] **Step 6: Write `solver.rs` with the trait and mapping test**

```rust
use crate::domain::{Cleared, SolveError, Target};
use async_trait::async_trait;

#[async_trait]
pub trait Solver: Send + Sync {
    async fn clear(&self, target: &Target) -> Result<Cleared, SolveError>;
}

pub fn map_error(error: cfsolve::Error) -> SolveError {
    use cfsolve::Error as E;
    match error {
        E::NotChallenged => SolveError::NotChallenged,
        E::Blocked { status } => SolveError::Blocked(status.as_u16()),
        E::Network(e) => SolveError::Network(e.to_string()),
        E::Rejected { error, .. } => SolveError::Rejected(error.to_string()),
        E::Challenged { status } => SolveError::Challenged(status.as_u16()),
        E::Unsupported(e) => SolveError::Unsupported(e.to_string()),
        E::Config(e) => SolveError::Config(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_not_challenged() {
        assert_eq!(map_error(cfsolve::Error::NotChallenged), SolveError::NotChallenged);
    }

    #[test]
    fn maps_blocked_status() {
        let error = cfsolve::Error::Blocked { status: cfsolve::StatusCode::FORBIDDEN };
        assert_eq!(map_error(error), SolveError::Blocked(403));
    }
}
```

- [ ] **Step 7: Add modules to `src/lib.rs`, run tests**

`pub mod detect; pub mod domain; pub mod solver;`

Run: `cargo test`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src
git commit -m "Add solver domain and challenge detection"
```

### Task 3: cfsolve-backed solver

**Files:** Modify `src/solver.rs`

- [ ] **Step 1: Add `CfsolveSolver`**

```rust
pub struct CfsolveSolver {
    browser: cfsolve::Browser,
    user_agent: String,
}

impl CfsolveSolver {
    /// Fails if the profile or its user agent is missing.
    pub fn load(profile: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let path = profile.as_ref();
        let text = std::fs::read_to_string(path)?;
        let json: serde_json::Value = serde_json::from_str(&text)?;
        let user_agent = json
            .pointer("/transport/headers/User-Agent")
            .and_then(|value| value.as_str())
            .ok_or_else(|| anyhow::anyhow!("profile has no transport.headers.User-Agent"))?
            .to_string();
        Ok(Self { browser: cfsolve::Browser::read(path).map_err(|e| anyhow::anyhow!(e))?, user_agent })
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }
}

#[async_trait]
impl Solver for CfsolveSolver {
    async fn clear(&self, target: &Target) -> Result<Cleared, SolveError> {
        let url: cfsolve::Url = target.url.parse().map_err(|e: url::ParseError| SolveError::Config(e.to_string()))?;
        let options = cfsolve::Options { proxy: target.proxy.clone(), sink: None };
        let clearance = self.browser.clear(&url, &options).await.map_err(map_error)?;
        Ok(Cleared {
            url: clearance.page.url.to_string(),
            status: clearance.page.status.as_u16(),
            body: clearance.page.html,
            cookies: clearance.cookies.into_iter().map(|cookie| crate::domain::NamedCookie {
                name: cookie.name().to_string(),
                value: cookie.value().to_string(),
                domain: cookie.domain().unwrap_or_default().to_string(),
                path: cookie.path().unwrap_or("/").to_string(),
                expires: cookie.expires().map(|t| t.unix_timestamp() as f64),
                http_only: cookie.http_only().unwrap_or(false),
                secure: cookie.secure().unwrap_or(false),
            }).collect(),
        })
    }
}
```

Add `url = "2"` if `cfsolve::Url` needs the crate for the parse error type; otherwise use `anyhow`.

- [ ] **Step 2: Compile**

Run: `cargo build`
Expected: success; fix cookie accessor names against `vendor/cfsolve/vendor/wreq/src/cookie/cookie.rs` if they differ.

- [ ] **Step 3: Commit**

```bash
git add src/solver.rs Cargo.toml
git commit -m "Add cfsolve-backed solver"
```

### Task 4: Proxy resolution

**Files:** Create `src/proxy.rs`; modify `src/lib.rs`

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_proxy_is_none() { assert_eq!(resolve(None).unwrap(), None); }

    #[test]
    fn credentials_are_encoded() {
        let input = Input { url: "http://host:8080".into(), username: Some("u@x".into()), password: Some("p#1".into()) };
        assert_eq!(resolve(Some(&input)).unwrap().unwrap(), "http://u%40x:p%231@host:8080");
    }

    #[test]
    fn socks4_passes_through() {
        let input = Input { url: "socks4://host:1080".into(), username: None, password: None };
        assert_eq!(resolve(Some(&input)).unwrap().unwrap(), "socks4://host:1080");
    }

    #[test]
    fn unknown_scheme_errors() {
        let input = Input { url: "ftp://host".into(), username: None, password: None };
        assert!(resolve(Some(&input)).is_err());
    }
}
```

- [ ] **Step 2: Run and watch fail** — `cargo test proxy` → missing items.

- [ ] **Step 3: Implement**

```rust
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};

pub struct Input {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

const ALLOWED: &[&str] = &["http://", "https://", "socks4://", "socks4a://", "socks5://", "socks5h://"];

pub fn resolve(input: Option<&Input>) -> anyhow::Result<Option<String>> {
    let Some(input) = input else { return Ok(None) };
    if !ALLOWED.iter().any(|scheme| input.url.starts_with(scheme)) {
        anyhow::bail!("unsupported proxy scheme in {}", input.url);
    }
    let Some(user) = &input.username else { return Ok(Some(input.url.clone())) };
    let scheme_end = input.url.find("://").unwrap() + 3;
    let (scheme, rest) = input.url.split_at(scheme_end);
    let encoded_user = utf8_percent_encode(user, NON_ALPHANUMERIC);
    let encoded_password = utf8_percent_encode(input.password.as_deref().unwrap_or(""), NON_ALPHANUMERIC);
    Ok(Some(format!("{scheme}{encoded_user}:{encoded_password}@{rest}")))
}
```

- [ ] **Step 4: Test, add module, commit** — `cargo test proxy`, then `git commit -m "Add proxy resolution"`.

---

## Chunk 3: Cache, fetch and API

### Task 5: Cache with expiry and single-flight

**Files:** Create `src/cache.rs`; modify `src/lib.rs`

- [ ] **Step 1: Failing tests** covering: insert then get returns entry; expired entry misses; cap evicts LRU; `invalidate` removes; two concurrent `get_or_solve` calls run the fake solver once and both get the same value; a failing leader's error reaches both.

```rust
#[tokio::test]
async fn waiters_share_one_solve() {
    let cache = Cache::new(8);
    let calls = Arc::new(AtomicUsize::new(0));
    let solver = CountingSolver(calls.clone());
    let (a, b) = tokio::join!(
        cache.get_or_solve(&solver, "k", || fake_target()),
        cache.get_or_solve(&solver, "k", || fake_target()),
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(a.unwrap().status, b.unwrap().status);
}
```

- [ ] **Step 2: Run and watch fail** — `cargo test cache`.

- [ ] **Step 3: Implement**

Key type is `String`. Entry holds `Vec<NamedCookie>` and `Instant`. Use `tokio::sync::Mutex<LruCache>` for values and `Mutex<HashMap<String, Arc<Notify>>>` for in-flight keys; the leader stores the result into the cache before notifying. Expiry is passed in by the caller as `Instant`.

Public surface:

```rust
pub struct Cache { /* ... */ }
impl Cache {
    pub fn new(cap: usize) -> Self;
    pub async fn get(&self, key: &str) -> Option<Vec<NamedCookie>>;
    pub async fn insert(&self, key: String, cookies: Vec<NamedCookie>, expires: Instant);
    pub async fn invalidate(&self, key: &str);
    pub async fn get_or_solve(
        &self,
        solver: &dyn Solver,
        key: &str,
        target: Target,
        max_solve: Duration,
    ) -> Result<Cleared, SolveError>;
}
```

`get_or_solve` waits on the notify for a non-leader, then re-reads the cache; on leader failure it stores nothing and returns the error.

- [ ] **Step 4: Test and commit** — `cargo test cache`, commit `"Add clearance cache and single-flight"`.

### Task 6: Fetch unit

**Files:** Create `src/fetch.rs`; modify `src/lib.rs`

- [ ] **Step 1: Failing test** with a local axum server asserting the request carries the user agent, the cookie header and the profile header order, and that POST bodies round-trip.

- [ ] **Step 2: Implement**

```rust
pub struct Fetcher { client: wreq::Client, user_agent: String, header_order: Vec<String> }

pub struct Snapshot { pub url: String, pub status: u16, pub body: String }

impl Fetcher {
    pub fn new(user_agent: String, header_order: Vec<String>, proxy: Option<&str>) -> anyhow::Result<Self>;
    pub async fn get(&self, url: &str, cookies: &[NamedCookie], deadline: Instant) -> anyhow::Result<Snapshot>;
    pub async fn post(&self, url: &str, body: &str, content_type: &str, cookies: &[NamedCookie], deadline: Instant) -> anyhow::Result<Snapshot>;
}
```

Build the `Cookie` header as `name=value; ...`. Order headers by `header_order`, skipping ones set explicitly. `redirect(Policy::limited(5))`, timeout from the deadline.

- [ ] **Step 3: Test and commit** — `cargo test fetch`, commit `"Add fetch client"`.

### Task 7: API

**Files:** Create `src/api.rs`, `tests/api.rs`; modify `src/lib.rs`

- [ ] **Step 1: Failing integration tests** with a `FakeSolver` injected through app state:

  - `/ping` GET and POST → 200 `status: ok`.
  - `request.get` first call → `status: ok`, solution with cookies, user agent, body.
  - second call → FakeSolver call count still 1.
  - cached page that contains a challenge → invalidates and calls the solver again.
  - `NotChallenged` → returns the fetched page with `ok`.
  - `Unsupported` → `status: error`.
  - `request.post` → body reaches the origin and is returned.
  - unknown cmd → `status: error`.
  - `sessions.create` → `status: ok` with a UUID.

- [ ] **Step 2: Implement** handlers returning HTTP 200 always, with the envelope from the spec (`status`, `message`, `version`, `startTimestamp`, `endTimestamp`, optional `solution`/`session`). Dispatch table in one `match`.

- [ ] **Step 3: Test and commit** — `cargo test`, commit `"Add FlareSolverr API"`.

### Task 8: Main and config

**Files:** Create `src/config.rs`, replace `src/main.rs`

- [ ] **Step 1: Config from env with defaults**: `BYEFLARE_BIND` (0.0.0.0:8191), `BYEFLARE_PROFILE` (vendor/cfsolve/profiles/safari-26-ios.json), `BYEFLARE_MAX_TIMEOUT` (60), `BYEFLARE_CACHE` (512), `BYEFLARE_INFLIGHT` (4). Missing profile is fatal.
- [ ] **Step 2: `main` loads the profile, builds state, serves axum, and prints `Waiting idle.`
- [ ] **Step 3: Smoke test** — `cargo run &`, `curl -s localhost:8191/ping`, expect `status":"ok`; `kill %1`.
- [ ] **Step 4: Commit** `"Add service config and wiring"`.

---

## Chunk 4: Deploy and gate

### Task 9: Container

**Files:** Create `Dockerfile`, `.dockerignore`, `README.md`

- [ ] **Step 1: Multi-stage Dockerfile**

```dockerfile
FROM rust:1.98.0-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang libclang-dev git && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN git clone --filter=blob:none https://github.com/chris124567/cfsolve vendor/cfsolve \
 && git -C vendor/cfsolve checkout ee4031fc9eb65702718f7ffbfedb451ad6bba541 \
 && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /src/target/release/byeflare /app/byeflare
COPY --from=build /src/vendor/cfsolve/profiles/safari-26-ios.json /app/profiles/safari-26-ios.json
COPY --from=build /src/vendor/cfsolve/profiles/safari-26-ios /app/profiles/safari-26-ios
ENV BYEFLARE_PROFILE=/app/profiles/safari-26-ios.json
CMD ["/app/byeflare"]
```

- [ ] **Step 2: `.dockerignore`**: `target`, `.git`, `engine-bench/target`, `docs`, `vendor`, `*.zip`.
- [ ] **Step 3: Build** — `docker build -t byeflare .` (on the Mac this is slow; the VPS build is the real one).
- [ ] **Step 4: README** with Easypanel steps, env table, Prowlarr setup (Proxy → FlareSolverr, tag indexer), and the `/ping` check.
- [ ] **Step 5: Commit.**

### Task 10: engine-bench gate adapter

**Files:** Create `engine-bench/src/cfsolve_engine.rs`; modify `engine-bench/src/main.rs`, `engine-bench/src/engine.rs`, `engine-bench/src/bench.rs`, `engine-bench/Dockerfile`

- [ ] **Step 1: `EngineSpec` accepts kind `cfsolve`, path is the profile.**
- [ ] **Step 2: `run` branches on kind before `engine_attempt`, calling `bench::cfsolve_attempt`, which uses `byeflare::solver::CfsolveSolver` and `bench::apply_replay` (make it `pub(crate)`), records the same `Record`, and samples RSS with `Sampler::start(libc::getpgrp())`.
- [ ] **Step 3: `selftest`/`probe` reject the kind with a clear message.**
- [ ] **Step 4: Dockerfile gains the profile copy; README gains the gate command.**
- [ ] **Step 5: `cargo test` in `engine-bench`, commit.**

---

## Manual gate (user, on the VPS)

```bash
nohup engine-bench run --targets /tmp/targets.txt --out /tmp/results \
  --engine cfsolve:/usr/local/share/cfsolve/profiles/safari-26-ios.json > /tmp/gate.log 2>&1 &
```

Pass: at least one `cleared` with a successful replay per target, RSS under 150 MB, warm solves under 10 s. Below that, record the failures and revisit the profile before touching the service design.
