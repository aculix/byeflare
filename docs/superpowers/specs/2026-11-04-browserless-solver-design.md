# byeflare — browserless FlareSolverr-compatible solver

Date: 2026-11-04
Status: design, pending validation spike

## Context

Byparr solves Cloudflare but needs a browser and is too heavy and slow for the user's seedbox. The user wants a drop-in FlareSolverr replacement for Prowlarr with no browser, no custom indexer definitions and no other operational hassle. Everything runs locally; no remote solving service.

Prowlarr replays with its own `.NET` `HttpClient`, reads only the returned cookies and user agent, and calls `request.get` or URL-encoded `request.post` with no sessions (`src/NzbDrone.Core/IndexerProxies/FlareSolverr/FlareSolverr.cs`). It requires a non-empty cookie list and rejects solver statuses other than 200/500. The earlier Byparr failure most likely came from solve and replay leaving on different rotating-proxy IPs.

## Goal

A small Rust service that speaks enough of the FlareSolverr `/v1` API to be drop-in in Prowlarr and solves Cloudflare challenges through [`cfsolve`](https://github.com/chris124567/cfsolve) with no browser process.

## Non-goals

- Session state. Commands are accepted and no-oped; Prowlarr sends none.
- Interactive CAPTCHA clicking beyond what cfsolve does.
- Other challenge vendors (PerimeterX, DataDome, Akamai).
- Persistence, UI, auth, Jackett support.
- Custom Cardigann definitions or Prowlarr Base URL rewriting.

These are pinned rather than configurable until a spike disagrees.

## Architecture

```
Prowlarr ──POST /v1──> api ──> cache ──miss──> single-flight ──> solver ──> CfsolveSolver
                         │         │                                (cfsolve)
                         │         └──hit──┐
                         └──> fetch <──────┴──> response
```

Units, each independently testable:

- **`api`** — FlareSolverr request/response types, dispatch, envelope. Knows only the domain types below.
- **`domain`** — `Target { url, proxy, deadline }`, `Cleared { url, status, body, cookies }`, `NamedCookie`, `SolveError`. No cfsolve types escape `solver`.
- **`solver`** — `#[async_trait] trait Solver { async fn clear(&self, target: &Target) -> Result<Cleared, SolveError> }` (async-trait for dyn compatibility). `CfsolveSolver` owns one `cfsolve::Browser` loaded at startup and maps `cfsolve::Error` into `SolveError`.
- **`fetch`** — the service's own `wreq` client, used for cache hits, `request.post` bodies and the fallback below. It sends the profile's user agent and supplied cookies, and reimplements the navigation header order from `transport.requests.headers.navigation` (cfsolve's parser is `pub(crate)`). It does not emulate the transport profile, so its ClientHello differs from the profile's; a cached hit can therefore be rejected and re-solved, which is acceptable and measured by the gate.
- **`cache`** — `Key(host, proxy)` → `Entry { cookies, expires_at }`. Interface: `get`, `insert`, `invalidate`. 512 entries, LRU, lock-free reads.
- **`single-flight`** — one in-progress solve per `Key`; waiters share the leader's task and leave at their own deadlines.
- **`proxy`** — Prowlarr's `{url, username, password}` to one URL, credentials percent-encoded.
- **`config`** — bind, default/max timeout, `max_solve_seconds`, `max_inflight` (default 4), cache cap, profile path. `max_inflight` gates solves only; a waiter joins an in-progress solve for its key regardless of the cap.

The user agent is the profile's, parsed once from `transport.headers.User-Agent` and held in `config`; cfsolve exposes no getter and has no UA option, so an override would desync it and require patching the fingerprint through `Browser::try_new`. A missing profile, missing UA or answer file is a startup failure.

### Why the solver navigates rather than using `clear_fetched`

`clear(url)` lets cfsolve navigate with the profile's own transport, which is what the recorded answers and TLS profile exist to satisfy. Prefetching and calling `clear_fetched` would put the service's client on the first, challenge-issuing request, so the challenge build could differ from the one the profile answers for. The cost is that `NotChallenged` and `Blocked` carry no page; `fetch` refetches in those cases. Cache hits and POSTs already need `fetch` for the same reason, so this adds no new unit.

## Data flow

Both commands share steps 1–5, which yield either a clearance or a page.

1. Parse, derive the deadline from `maxTimeout` (clamped, default 60 s).
2. Key on host + proxy.
3. Fresh hit: `fetch` with the cached cookies and UA. Detect a challenge in the body; if present, `invalidate` and continue to 4.
4. Miss: join or lead the single-flight solve. The shared solve is bounded by `max_solve_seconds`, not the leader's deadline.
5. `NotChallenged` or `Blocked`: neither carries a page, so `fetch` the target. If the refetch is challenged, solve once and use that solve's status, body and cookies; otherwise keep the refetch and its own status.
6. On a clearance, whether from step 4 or 5: re-detect a challenge in the body, retry once if present, then `SolveError::Challenged`; insert into the cache at the earliest of cookie expiry minus 60 s and 30 minutes.
7. GET returns `solution` with `solution.status` and `solution.url`, the body, every cookie, and the profile UA. `solution.headers` is an empty object: neither cfsolve nor the domain type retains response headers, and Prowlarr reads none of it.
8. POST (URL-encoded only) instead sends `postData` through `fetch` with the request's content type, the clearance cookies and UA, and returns that response.

`/ping` answers GET and POST with `status: ok`. `sessions.create` returns a UUID in a success envelope; `sessions.list` and `sessions.destroy` return fixed successes. Unknown commands return an error envelope.

The envelope also carries `version`, `startTimestamp` and `endTimestamp` (epoch ms, as FlareSolverr does), even though Prowlarr reads none of them. Every solution carries cookies shaped as `name`, `value`, `domain`, `path`, `expires` (epoch seconds), `size`, `httpOnly`, `secure`, `session`, `sameSite`, plus `solution.url`.

## Error handling

Every reply is HTTP 200 with `status: ok|error`.

| cfsolve `Error` | Behaviour |
|---|---|
| `NotChallenged` | `fetch` and return it; if the refetch is challenged, solve once |
| `Blocked { .. }` | same refetch path, keeping the refetch's status |
| `Network(_)` | retry once, then `error` |
| `Rejected { .. }` | new solve once, then `error` |
| `Challenged { status }` | retry once, then `error` with the status |
| `Unsupported(_)`, `Config(_)` | `error`, cause in `message` |

Also: a challenge still detected after a successful solve → one retry, then `error`. Deadline exceeded → `error`, waiter leaves without cancelling the shared solve. A panicking solve task becomes an `error` reply. Zero cookies are returned as-is; Prowlarr raises its own "empty cookies", as with FlareSolverr.

## Proxy

Prowlarr emits `socks4://`, which cfsolve does not document; the patched wreq maps `socks4`/`socks4a`, so `proxy` passes `http`, `https`, `socks4`, `socks5` and `socks5h` through and errors on any other scheme rather than silently going direct. Credentials are percent-encoded into the URL; socks4 cannot carry them, so a socks4 proxy goes unauthenticated. No global proxy is configured; Prowlarr supplies one per request, so solve and replay share an IP.

## Deployment

Easypanel App service from a zip whose top-level `Dockerfile` Easypanel picks up. Multi-stage build on Rust 1.98.0 (cfsolve's pinned channel) with CMake, a C/C++ compiler, libclang and Git. The build stage clones cfsolve at revision `ee4031f` into `vendor/cfsolve` before any Cargo command (patches need real paths, so a git dependency is not enough, and cloning keeps the zip small; a `scripts/fetch-cfsolve.sh` does the same for local builds). The clone supplies cfsolve's whole `vendor/` tree, about 31 MB including BoringSSL. cfsolve is depended on by path with `default-features = false`, and both the service manifest and `engine-bench`'s copy its `[patch.crates-io]` entries for `btls-sys`, `wreq` and `http2`, pointing into that tree. `fetch` must use the same patched `wreq` at the same version (`=0.16.1`, with `socks`), or its cookies and status types are not cfsolve's. `vendor/` is gitignored. cfsolve is MIT (© 2026 Christopher Tarry) and its licence text ships in the image notices.

The runtime image carries the binary, the profile JSON **and its sibling `answers.json` / `challenge-page-answers.json`** (`Browser::read` resolves them relative to the profile), and CA roots. With no command override the service listens and idles, so Easypanel restarts do not fire attempts.

## Validation gate

Before service code, extend `engine-bench` with an in-process cfsolve adapter, selected as `--engine cfsolve:<profile.json>` and living beside `baseline_attempt`, reusing `apply_replay`. It:

1. Solves a target, taking the UA from the profile file.
2. Replays with those cookies and UA from a **different TLS stack, exactly as `engine-bench` already replays** (not "as Prowlarr does" — `engine-bench` uses reqwest/rustls, Prowlarr uses .NET).
3. Records outcome, seconds per warm solve (profile loaded, `Browser` reused; cold start measured separately) and peak RSS.

`engine-bench`'s Dockerfile gains the same clone step, CMake, libclang and Rust 1.98.0, since the adapter builds cfsolve there too. The adapter bypasses `engine.rs`'s process/CDP path: the path after `cfsolve:` is a profile, not a binary, `EngineSpec::from_str` still parses it, and `run` branches on the kind before `engine_attempt`. `selftest` and `probe` call `Running::start` unconditionally and so do not support the kind. RSS sampling passes `libc::getpgrp()` to `Sampler::start`, since nothing forks. The run uses Prowlarr's real egress, including the VPN-container caveat from the README.

Gate: `1337x.to` and `eztvx.to`, Prowlarr's egress IP, three attempts each, one confirmed successful replay each. If replay fails, the design is reconsidered before service code.

Budget: under 150 MB peak RSS and under 10 s per warm solve, with the first cold solve excluded from the timing figure and measured separately. The spike revises either number and records why.

## Risks

| Risk | Mitigation |
|---|---|
| Clearance is bound to more than IP + UA, so replay is rejected | The gate measures exactly this |
| cfsolve is days old and may rot | Pinned revision; solver behind a trait; profile is data, and the UA follows it |
| New challenge build returns `Unsupported` | Gate runs from the real server; failure is reported, not hidden |
| BoringSSL patch breaks on toolchain drift | Pinned toolchain and revision |
| Solve storms | Single-flight, cache, `max_inflight` |
| Stale cookie served as `ok` | Challenge re-detection on every hit and every solve |
| Profile/UA becomes wrong for these indexers | Only Safari ships; a Chrome profile is future data work, not a code change |

## Fixed decisions

- Profile: cfsolve's bundled `profiles/safari-26-ios` (with its answers directory), copied into the image.
- Cache: 512 entries, LRU, expiry at cookie expiry minus 60 s capped at 30 minutes.
- Shared solves bounded by `max_solve_seconds` (60); waiter deadlines are independent.
- HTTP 200 envelopes for every reply; `/ping` on GET and POST.
