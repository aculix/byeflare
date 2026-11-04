# byeflare

A browserless, FlareSolverr-compatible challenge solver. Drop-in replacement in
Prowlarr's FlareSolverr settings — same `/v1` protocol, no Chromium, a few MB of
RAM idle.

The solving is done by [`cfsolve`](https://github.com/chris124567/cfsolve)
(MIT), vendored at a pinned revision. byeflare wraps it in the FlareSolverr API,
caches clearances per host + proxy, coalesces concurrent solves, and replays the
page itself with the solver's own cookies and user agent.

**Status:** MVP. The protocol layer is tested; whether a given indexer accepts a
replayed clearance must be proven on your own egress IP. Run the
[gate](#gate-before-trusting-it) first.

## Scope

- Personal, single-egress-IP use. One proxy, no rotation, no bulk scraping.
- Cloudflare managed challenges and Turnstile as implemented by cfsolve.
- No `sessions.*` state, no browser actions, no screenshots.
- `/v1` only (`request.get`, `request.post`, `sessions.*` no-ops) and `/ping`.

## Build locally

The pinned cfsolve tree is not committed:

```bash
./scripts/fetch-cfsolve.sh
cargo run
```

Requires Rust 1.98.0 (see `rust-toolchain.toml`), CMake, clang/libclang, and git.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `BYEFLARE_BIND` | `0.0.0.0:8191` | Listen address |
| `BYEFLARE_PROFILE` | `vendor/cfsolve/profiles/safari-26-ios.json` | Browser profile JSON |
| `BYEFLARE_DEFAULT_TIMEOUT` | `60` | Seconds used when `maxTimeout` is absent |
| `BYEFLARE_MAX_TIMEOUT` | `60` | Upper bound for a solve |
| `BYEFLARE_CACHE` | `512` | Cached clearances held |
| `BYEFLARE_INFLIGHT` | `4` | Concurrent solves |

A missing or unreadable profile is fatal at startup.

## Container

```bash
docker build -t byeflare .
docker run -p 8191:8191 byeflare
curl http://localhost:8191/ping
```

For Easypanel, zip this repository (without `target/`) and deploy as the Dockerfile
app; there is nothing to install. The container is idle by default and uses
memory only while solving.

## Prowlarr

1. Settings → Indexers → add **FlareSolverr** proxy: `http://<host>:8191`.
2. Apply the `flaresolverr` tag to the indexers Cloudflare blocks.

Prowlarr sends `request.get`, reads the returned cookies and `userAgent`, and
replays the request itself. Both sides therefore need the same egress IP — pass
Prowlarr's proxy (`proxy.url`, `proxy.username`, `proxy.password`) and byeflare
uses it for the solve too. Do not run Prowlarr through a different egress.

## Gate before trusting it

On the target VPS, against real blocked indexers:

```bash
nohup engine-bench run --targets /tmp/targets.txt --out /tmp/results \
  --engine cfsolve:/usr/local/share/cfsolve/profiles/safari-26-ios.json > /tmp/gate.log 2>&1 &
```

Pass: at least one `cleared` with a successful replay per target, RSS under
150 MB, warm solves under 10 s. Below that, record the failures and revisit the
profile before touching the service design.

## Endpoints

- `GET|POST /ping` → `{"status":"ok"}`
- `POST /v1` → FlareSolverr envelope with `solution.cookies`, `solution.userAgent`,
  `solution.response`

Errors come back as `{"status":"error","message":"..."}` with HTTP 200. Prowlarr
only shows a generic message, so every failure is also printed to the container
log as `byeflare: <url>: <reason>` — check there with `docker logs`.

Cookies set by the page are passed through, and a solve always carries at least
one cookie, because Prowlarr rejects an empty set outright.
