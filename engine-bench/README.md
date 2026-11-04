# engine-bench

Compares browser engines, and your current solver, on the one job Prowlarr needs: get past a challenge and hand back cookies that work. An attempt only counts as usable if a plain HTTP replay with those cookies and that user agent gets through, because that is what Prowlarr does next.

Run it on the server Prowlarr uses. Cloudflare's decision depends on the IP address, so results from anywhere else don't answer the question.

## Run it on your server

Copy this folder to the server, then run these from inside it.

Build the image. It builds natively on `linux/amd64` or `linux/arm64`, and installs Chromium, `chromium-headless-shell` and Lightpanda:

```bash
docker build -t engine-bench .
```

Check that everything works on the built-in mock. This touches no real site:

```bash
docker run --rm --shm-size=1g engine-bench selftest --no-sandbox \
  --engine chrome:/usr/bin/chromium \
  --engine chrome-xvfb:/usr/bin/chromium \
  --engine headless-shell:/usr/lib/chromium/chromium-headless-shell \
  --engine lightpanda:/usr/local/bin/lightpanda
```

Create `run/targets.txt` with two or three indexer URLs that fail in Prowlarr today, using `targets.example.txt` as a guide:

```bash
mkdir -p run && cp targets.example.txt run/targets.txt
```

In a second terminal, log Byparr's memory for the length of the test. Change `byparr` if your container has another name:

```bash
while true; do docker stats --no-stream --format '{{.MemUsage}}' byparr; sleep 1; done | tee run/byparr-memory.log
```

Run the comparison. `--network host` keeps the traffic on the server's own IP:

```bash
docker run --rm --network host --shm-size=1g --user "$(id -u):$(id -g)" -v "$PWD/run:/data" \
  engine-bench run --no-sandbox --targets /data/targets.txt --out /data/results \
  --engine chrome:/usr/bin/chromium \
  --engine chrome-xvfb:/usr/bin/chromium \
  --engine headless-shell:/usr/lib/chromium/chromium-headless-shell \
  --engine lightpanda:/usr/local/bin/lightpanda \
  --baseline http://127.0.0.1:8191
```

If Prowlarr reaches the internet through a VPN container, swap `--network host` for `--network container:<vpn-container>`, so the test leaves through the same IP. Then point `--baseline` at an address Byparr can be reached on from that network.

## Run it on Easypanel

1. Create an **App** service, choose the **Upload** source and upload `engine-bench.zip`. The `Dockerfile` sits at the top of the archive, so Easypanel should pick the Dockerfile builder by itself.
2. Deploy it with the command override left empty. The container just idles and touches no site until you start a test, and its logs show `Waiting idle`.

   Don't set `run` as the command. Easypanel restarts services that exit, so it would hit your indexers over and over from Prowlarr's IP.
3. Open **Shell** on the service's overview, choose `bash`, and check the engines:

   ```bash
   engine-bench selftest --no-sandbox \
     --engine chrome:/usr/bin/chromium \
     --engine chrome-xvfb:/usr/bin/chromium \
     --engine headless-shell:/usr/lib/chromium/chromium-headless-shell \
     --engine lightpanda:/usr/local/bin/lightpanda
   ```

4. List two or three indexer URLs that fail in Prowlarr today, one per line, replacing the examples:

   ```bash
   cat > /tmp/targets.txt <<'EOF'
   https://first-indexer.example/
   https://second-indexer.example/
   EOF
   ```

5. Start the comparison in the background, so closing the shell doesn't stop it. Change `project_byparr` to your Byparr service's address. Easypanel's docs show the form `project_service`:

   ```bash
   nohup engine-bench run --no-sandbox --targets /tmp/targets.txt --out /tmp/results \
     --engine chrome:/usr/bin/chromium \
     --engine chrome-xvfb:/usr/bin/chromium \
     --engine headless-shell:/usr/lib/chromium/chromium-headless-shell \
     --engine lightpanda:/usr/local/bin/lightpanda \
     --baseline http://project_byparr:8191 > /tmp/run.log 2>&1 &
   ```

6. Follow it from any shell. The results table prints at the end:

   ```bash
   tail -f /tmp/run.log
   ```

While it runs, note Byparr's peak memory from its service page in Easypanel. Results in `/tmp` disappear on redeploy, so copy the table out first. Stop the service when you're done.

If Prowlarr reaches the internet through a VPN, this container won't share its IP, and the results won't match what Prowlarr sees.

## Reading the results

The table prints at the end, and the full detail goes to `run/results/`. Cookie values are never saved.

Each target first gets one plain request. If it says `not protected`, that site isn't challenging this IP, so it can't tell the engines apart.

| Outcome | Meaning |
|---|---|
| `cleared` | The challenge finished on its own and `cf_clearance` was issued |
| `no_challenge` | The page loaded with no challenge |
| `blocked` | A block page appeared, which waiting won't clear |
| `challenge_timeout` | The challenge never finished |
| `interactive_timeout` | The challenge showed a checkbox widget, which this tool never clicks |
| `load_timeout` | Nothing usable loaded in time |
| `engine_error` | The engine crashed or doesn't support a command the tool needs |

Keep the attempt count low. Failed attempts come from the same IP as Prowlarr, so hammering a site could make things worse for Prowlarr too. Attempts run one at a time with a 10-second gap. With five entries and three targets, the default three rounds is 45 attempts, which takes up to about an hour.

## Testing proxies

Store each proxy in its own environment variable, then name the variables with `--exit`. Logins stay out of commands, logs and results.

On Easypanel, add the variables in the service's **Environment** tab and deploy again. Redeploying clears `/tmp`, so create `targets.txt` again afterwards. With plain Docker, copy `.env.example` to `.env`, fill it in and add `--env-file .env` to `docker run`.

```bash
PROXY_DATACENTER=http://username:password@host:port
PROXY_RESIDENTIAL=http://username:password@host:port
PROXY_IPV6=http://username:password@host:port
```

`socks5://` works too. Percent-encode any special characters in the login, for example `@` as `%40`, `#` as `%23`, `/` as `%2F` and `?` as `%3F`.

Repeat `--exit` to compare routes in one table, with `direct` as your server's own IP. This run tries two engines on each route:

```bash
nohup engine-bench run --no-sandbox --rounds 1 --pause 5 --targets /tmp/targets.txt --out /tmp/results \
  --exit direct --exit PROXY_DATACENTER --exit PROXY_RESIDENTIAL --exit PROXY_IPV6 \
  --engine chrome-xvfb:/usr/bin/chromium \
  --engine headless-shell:/usr/lib/chromium/chromium-headless-shell > /tmp/run.log 2>&1 &
```

The log starts with one `exit` line per route, showing the IP and country it really leaves from. A proxy that doesn't work is skipped there, before any attempts are spent on it. Results then appear as rows like `chrome-xvfb via PROXY_RESIDENTIAL`. Byparr's `--baseline` only runs on the `direct` route, because Byparr sets its own proxy.

- **Use a sticky session.** `cf_clearance` is tied to the IP, so a proxy that changes IP between requests can show `cleared` with 0 usable solves, because the replay arrives from a different address.
- **Prowlarr has to use the same proxy.** It replays requests itself, so a solve only helps if Prowlarr leaves from the same IP.
- **IPv6 proxies need one check.** The `exit` line shows whether traffic really leaves over IPv6. If the proxy's own address is IPv6, the container needs IPv6 to reach it, and the exit will be skipped if it doesn't.

## Obscura

Obscura isn't bundled. Its binaries come from a pseudonymous personal account, so installing it is your call. If you want it in the test, download it yourself and mount the binary read-only so it stays inside the container:

```bash
-v /path/to/obscura:/opt/obscura:ro --engine obscura:/opt/obscura --engine obscura-stealth:/opt/obscura
```

`obscura-stealth` needs a `-stealth` release. Obscura has never been run through this tool, so an `engine_error` may be a compatibility gap rather than a verdict on the engine.

## What it doesn't do

- Engines run with stock settings. There are no stealth patches, and nothing clicks a widget.
- The replay uses Rust's TLS stack, while Prowlarr uses .NET's. Cloudflare could treat them differently, so confirm any winner in Prowlarr itself.
- Memory is PSS on Linux. On macOS it is summed RSS, which counts shared pages more than once and overstates multi-process browsers.
- Each attempt starts a fresh engine, so these are cold-start numbers.
