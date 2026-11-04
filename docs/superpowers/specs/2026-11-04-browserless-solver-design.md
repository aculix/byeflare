# byeflare — browserless FlareSolverr-compatible solver

Date: 2026-11-04
Status: design, pending validation spike

## Context

Byparr solves Cloudflare but needs a browser and is too heavy and slow for the user's seedbox. The user wants a drop-in FlareSolverr replacement for Prowlarr with no browser, no custom indexer definitions and no other operational hassle. Everything must run locally on the server; no remote solving service.

The previous diagnosis matters: Byparr solved `eztvx.to`, but Prowlarr's own replay was then blocked. Both used the same rotating proxy pool, so they most likely left from different IPs and `cf_clearance` did not match. Prowlarr replays with its own `.NET` `HttpClient`, reads only the returned cookies and user agent, and accepts `request.get` and URL-encoded `request.post` with no sessions (`src/NzbDrone.Core/IndexerProxies/FlareSolverr/FlareSolverr.cs`). It requires a non-null, non-empty cookie list and rejects solver statuses other than 200/500.

## Goal

A small Rust service that speaks enough of the FlareSolverr `/v1` API to be drop-in in Prowlarr, solves Cloudflare challenges through [`cfsolve`](https://github.com/chris124567/cfsolve) with no browser process, and is fast enough on a 1 vCPU seedbox that Prowlarr does not time out.

## Non-goals

- Session state. Commands are accepted and no-oped; Prowlarr sends none.
- Interactive CAPTCHA clicking beyond what cfsolve does.
- Other challenge vendors (PerimeterX, DataDome, Akamai).
- Persistence, UI, auth, Jackett support.
- Custom Cardigann definitions or Prowlarr Base URL rewriting.

## Architecture

```
Prowlarr ──POST /v1──> api ──> cache ──miss──> solver (trait) ──> CfsolveSolver
                         │                          │
                         └────────── hit ───────────┴──> solution
```

Units, each independently testable:

- **`api`** — request/response types, command dispatch, envelope construction. Knows nothing about cfsolve.
- **`solver`** — `trait Solver { async fn clear(&self, target: &Target) -> Result<Clearance> }`. `CfsolveSolver` holds one `cfsolve::Browser`, loaded once at startup from a bundled profile. Swapping solvers means adding an impl.
- **`cache`** — clearance cookies and the user agent that earned them, keyed by host plus proxy. Single-flight: one solve per key, waiters share the result. Expires at the cookie's own expiry minus a small skew, capped at 30 minutes.
- **`proxy`** — converts Prowlarr's `{url, username, password}` into one URL, percent-encoding the credentials.
- **`config`** — bind address, default and maximum timeout, cache cap, profile path.

`Clearance` is cfsolve's own type narrowed for us: cookies, user agent, the cleared page, and its status. `Target` is the URL, optional proxy and deadline. No unit needs cfsolve's internals to be understood.

## Data flow

`request.get`:

1. Parse, clamp `maxTimeout` to a ceiling; missing uses the default.
2. Key the cache on host + proxy.
3. Fresh hit: fetch the target with the cached cookie and user agent, return.
4. Miss: single-flight leader solves through the trait; waiters await the same result.
5. Return `solution` with the real status, headers, body, every cookie in play, and the profile's user agent.

`request.post` (URL-encoded only):

1. Obtain a clearance for the URL as above.
2. POST `postData` with the request's content type, cookie and user agent.
3. Return that response with the clearance cookies and user agent.

`/ping` returns `status: ok`. `sessions.create`, `sessions.list` and `sessions.destroy` return fixed successful envelopes, `create` generating a UUID. Unknown commands return an error envelope.

## Error handling

Every reply is HTTP 200 with `status: ok|error`, as FlareSolverr does.

| Case | Behaviour |
|---|---|
| `NotChallenged` | `ok`, page unchanged |
| Challenge still detected after a solve | retry once inside the deadline, then `error` |
| `Unsupported`, `Blocked`, `Config` | `error`, cause in `message` |
| Deadline exceeded | `error`; solve abandoned, no orphan work |
| Upstream 5xx | passed through with its real status |
| Zero cookies | returned as-is; Prowlarr raises "empty cookies", same as FlareSolverr |
| Leader solve fails | waiters receive the same error; next request retries |

Only the deadline cancels work. Panics in a solve task are caught and turned into an error reply, not a dropped connection.

## Validation gate

Before service code, extend `engine-bench` with a cfsolve adapter that:

1. Solves a target through `cfsolve` and obtains `cf_clearance` plus a user agent.
2. Replays the request with a **different** TLS stack, exactly as Prowlarr does, using those cookies and UA.
3. Records `cleared`/`no_challenge`/failure, seconds per solve, and peak RSS from the existing memory module.

The gate is: two targets (`1337x.to`, `eztvx.to`), from the VPS's own IP, three attempts each, one confirmed successful replay each. If the replay fails, cfsolve's clearance does not survive Prowlarr either and this design is reconsidered before any service code is written.

The gate is run on the server by the user, as all real attempts are.

Budget: under 150 MB peak RSS and under 10 s per warm solve. The spike revises either figure if the measurement disagrees, and the revision is recorded before the plan proceeds.

## Deployment

Easypanel App service, created from a zip whose top-level `Dockerfile` Easypanel picks up. Multi-stage build: pinned Rust toolchain plus CMake, a C/C++ compiler, libclang and Git, because cfsolve patches BoringSSL; runtime image carries only the binary, profile and CA roots. With no command override the service listens and idles instead of exiting, so Easypanel restarts do not fire attempts at indexers.

Health is `/ping`. The profile ships inside the image. Proxy credentials never appear in commands, logs or results; Prowlarr supplies them per request.

## Risks

| Risk | Mitigation |
|---|---|
| `cf_clearance` bound to more than IP + UA, so Prowlarr's replay is rejected | The validation gate tests exactly this |
| cfsolve is a week old and may rot | Solver behind a trait; profile and UA overridable |
| Cloudflare serves different challenge builds by region | Spike runs from the real server; `Unsupported` is reported, not hidden |
| BoringSSL patch breaks on a toolchain update | Pinned toolchain in the image |
| Solve storms from Prowlarr retries | Single-flight, cache, small global concurrency cap |

## Open questions

- Which profile to ship if cfsolve's bundled Safari-on-iPhone profile clears worse than a Chrome profile on these indexers. Decided by the spike.
- Cache cap and TTL once real expiry behaviour is observed.
