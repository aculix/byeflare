//! A local forward proxy that adds an upstream proxy's login. Browsers ignore
//! logins in their proxy flags, so every engine connects here instead, and
//! credentials never reach a command line.

use anyhow::{Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use percent_encoding::percent_decode_str;
use reqwest::Url;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_socks::tcp::Socks5Stream;

const MAX_HEAD_BYTES: usize = 64 * 1024;
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);
const BAD_REQUEST: &str = "400 Bad Request";
const BAD_GATEWAY: &str = "502 Bad Gateway";
const GATEWAY_TIMEOUT: &str = "504 Gateway Timeout";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Socks5,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Login {
    pub username: String,
    pub password: String,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Upstream {
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
    pub login: Option<Login>,
}

/// Printed without the login, so a stray debug line can't leak the password.
impl fmt::Debug for Upstream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?} {}:{}", self.scheme, self.host, self.port)
    }
}

impl Upstream {
    /// Accepts `http://user:pass@host:port`, `socks5://...`, or the first form
    /// without its scheme. Errors never repeat the text, as it holds a password.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim().trim_matches(['"', '\'']);
        let full = if text.contains("://") {
            text.to_string()
        } else {
            format!("http://{text}")
        };
        let Ok(url) = Url::parse(&full) else {
            bail!("expected http://user:pass@host:port, with special characters percent-encoded");
        };

        let scheme = match url.scheme() {
            "http" => Scheme::Http,
            "socks5" | "socks5h" => Scheme::Socks5,
            _ => bail!("only http:// and socks5:// proxies are supported"),
        };
        let Some(host) = url.host_str().filter(|host| !host.is_empty()) else {
            bail!("the proxy needs a host");
        };
        let Some(port) = url.port_or_known_default() else {
            bail!("the proxy needs a port");
        };
        let login = match url.username() {
            "" => None,
            username => Some(Login {
                username: decoded(username)?,
                password: decoded(url.password().unwrap_or(""))?,
            }),
        };

        Ok(Self {
            scheme,
            host: host.trim_matches(['[', ']']).to_string(),
            port,
            login,
        })
    }
}

fn decoded(text: &str) -> Result<String> {
    match percent_decode_str(text).decode_utf8() {
        Ok(decoded) => Ok(decoded.into_owned()),
        Err(_) => bail!("the proxy login isn't valid text"),
    }
}

/// Starts the forwarder and returns the proxy URL engines should use.
pub async fn start(upstream: Upstream) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let upstream = Arc::new(upstream);
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            tokio::spawn(serve(client, upstream.clone()));
        }
    });
    Ok(format!("http://{address}"))
}

async fn serve(mut client: TcpStream, upstream: Arc<Upstream>) {
    if let Err(status) = forward(&mut client, &upstream).await {
        let reply = format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
        let _ = client.write_all(reply.as_bytes()).await;
    }
}

/// Returns the HTTP status to send back when the request can't be forwarded.
async fn forward(client: &mut TcpStream, upstream: &Upstream) -> std::result::Result<(), String> {
    let (head, early_bytes) = read_head(client).await.ok_or(BAD_REQUEST)?;
    let request = parse_request(&head).ok_or(BAD_REQUEST)?;

    let tunnel = open_tunnel(upstream, &request.host, request.port);
    let mut tunnel = timeout(UPSTREAM_TIMEOUT, tunnel)
        .await
        .map_err(|_| GATEWAY_TIMEOUT)??;

    let write_failed = |_| BAD_GATEWAY.to_string();
    if request.is_connect {
        let established = b"HTTP/1.1 200 Connection established\r\n\r\n";
        client.write_all(established).await.map_err(write_failed)?;
    } else {
        let origin_head = &request.origin_head;
        tunnel.write_all(origin_head).await.map_err(write_failed)?;
    }
    tunnel.write_all(&early_bytes).await.map_err(write_failed)?;

    let _ = copy_bidirectional(client, &mut tunnel).await;
    Ok(())
}

async fn open_tunnel(
    upstream: &Upstream,
    host: &str,
    port: u16,
) -> std::result::Result<TcpStream, String> {
    match upstream.scheme {
        Scheme::Http => http_tunnel(upstream, host, port).await,
        Scheme::Socks5 => socks_tunnel(upstream, host, port).await,
    }
}

async fn http_tunnel(
    upstream: &Upstream,
    host: &str,
    port: u16,
) -> std::result::Result<TcpStream, String> {
    let mut stream = TcpStream::connect((upstream.host.as_str(), upstream.port))
        .await
        .map_err(|_| BAD_GATEWAY)?;

    let address = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut request = format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\n");
    if let Some(login) = &upstream.login {
        let credentials = basic_credentials(&format!("{}:{}", login.username, login.password));
        request.push_str(&format!("Proxy-Authorization: {credentials}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|_| BAD_GATEWAY)?;

    let (head, _) = read_head(&mut stream).await.ok_or(BAD_GATEWAY)?;
    let head = String::from_utf8_lossy(&head);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_once(' '))
        .map(|(_, status)| status.to_string())
        .ok_or(BAD_GATEWAY)?;
    if status.starts_with('2') {
        Ok(stream)
    } else {
        Err(status)
    }
}

async fn socks_tunnel(
    upstream: &Upstream,
    host: &str,
    port: u16,
) -> std::result::Result<TcpStream, String> {
    let proxy = (upstream.host.as_str(), upstream.port);
    let stream = match &upstream.login {
        Some(login) => {
            let (username, password) = (login.username.as_str(), login.password.as_str());
            Socks5Stream::connect_with_password(proxy, (host, port), username, password).await
        }
        None => Socks5Stream::connect(proxy, (host, port)).await,
    };
    stream
        .map(Socks5Stream::into_inner)
        .map_err(|_| BAD_GATEWAY.to_string())
}

/// The `Proxy-Authorization` value for a `user:pass` login.
pub fn basic_credentials(login: &str) -> String {
    format!("Basic {}", STANDARD.encode(login))
}

/// Reads up to the blank line ending an HTTP head, and returns the head along
/// with any bytes that arrived after it.
pub async fn read_head(stream: &mut TcpStream) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        if let Some(end) = buffer.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let rest = buffer.split_off(end + 4);
            return Some((buffer, rest));
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return None;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ClientRequest {
    host: String,
    port: u16,
    is_connect: bool,
    /// The same request addressed to the origin server, for plain HTTP.
    origin_head: Vec<u8>,
}

fn parse_request(head: &[u8]) -> Option<ClientRequest> {
    let text = std::str::from_utf8(head).ok()?;
    let (line, rest) = text.split_once("\r\n")?;
    let mut parts = line.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);

    let is_connect = method == "CONNECT";
    let url = if is_connect {
        Url::parse(&format!("http://{target}")).ok()?
    } else {
        Url::parse(target).ok()?
    };
    if url.scheme() != "http" {
        return None;
    }

    let query = url
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    let path = url.path();
    Some(ClientRequest {
        host: url.host_str()?.trim_matches(['[', ']']).to_string(),
        port: url.port_or_known_default()?,
        is_connect,
        origin_head: format!("{method} {path}{query} {version}\r\n{rest}").into_bytes(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SECRET: &str = "s3cret-Pass";

    fn parsed(text: &str) -> Upstream {
        Upstream::parse(text).unwrap()
    }

    fn login(username: &str, password: &str) -> Option<Login> {
        Some(Login {
            username: username.into(),
            password: password.into(),
        })
    }

    fn upstream_for(proxy: &mock::MockProxy, credentials: Option<Login>) -> Upstream {
        Upstream {
            scheme: Scheme::Http,
            host: proxy.address.ip().to_string(),
            port: proxy.address.port(),
            login: credentials,
        }
    }

    async fn get_through(forwarder: &str, url: &str) -> reqwest::Response {
        let client = reqwest::Client::builder()
            .no_proxy()
            .proxy(reqwest::Proxy::all(forwarder).unwrap())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        client.get(url).send().await.unwrap()
    }

    #[test]
    fn parses_an_http_proxy_with_a_login() {
        let upstream = parsed("http://user:pass@proxy.example:1338");
        assert_eq!(upstream.scheme, Scheme::Http);
        assert_eq!(upstream.host, "proxy.example");
        assert_eq!(upstream.port, 1338);
        assert!(upstream.login == login("user", "pass"));
    }

    #[test]
    fn a_missing_scheme_means_http() {
        let upstream = parsed("user-session-1-ttl-60:pass@proxy.example:1338");
        assert_eq!(upstream.scheme, Scheme::Http);
        assert_eq!(upstream.host, "proxy.example");
        assert!(upstream.login == login("user-session-1-ttl-60", "pass"));
    }

    #[test]
    fn parses_socks5_proxies() {
        assert_eq!(
            parsed("socks5://u:p@proxy.example:1080").scheme,
            Scheme::Socks5
        );
        assert_eq!(
            parsed("socks5h://u:p@proxy.example:1080").scheme,
            Scheme::Socks5
        );
    }

    #[test]
    fn a_proxy_without_a_login_is_allowed() {
        assert!(parsed("http://proxy.example:3128").login.is_none());
    }

    #[test]
    fn parses_an_ipv6_literal_host() {
        let upstream = parsed("http://u:p@[2001:db8::1]:8080");
        assert_eq!(upstream.host, "2001:db8::1");
        assert_eq!(upstream.port, 8080);
    }

    #[test]
    fn decodes_percent_encoded_logins() {
        let upstream = parsed("http://user%40mail:p%40ss%23word@proxy.example:1338");
        assert!(upstream.login == login("user@mail", "p@ss#word"));
    }

    #[test]
    fn surrounding_quotes_and_spaces_are_ignored() {
        assert_eq!(parsed("  \"http://u:p@proxy.example:1338\"\n").port, 1338);
    }

    #[test]
    fn rejects_https_proxies() {
        assert!(Upstream::parse("https://u:p@proxy.example:443").is_err());
    }

    #[test]
    fn rejects_a_socks_proxy_without_a_port() {
        assert!(Upstream::parse("socks5://u:p@proxy.example").is_err());
    }

    #[test]
    fn errors_never_repeat_the_password() {
        for text in [
            format!("http://user:{SECRET}@"),
            format!("https://user:{SECRET}@proxy.example:443"),
            format!("socks5://user:{SECRET}@proxy.example"),
        ] {
            let error = format!("{:#}", Upstream::parse(&text).unwrap_err());
            assert!(!error.contains(SECRET), "{error}");
        }
    }

    #[test]
    fn debug_output_hides_the_login() {
        let upstream = parsed(&format!("http://user:{SECRET}@proxy.example:1338"));
        let printed = format!("{upstream:?}");
        assert!(!printed.contains(SECRET), "{printed}");
        assert!(!printed.contains("user"), "{printed}");
    }

    #[test]
    fn reads_a_connect_request() {
        let request =
            parse_request(b"CONNECT indexer.test:443 HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(request.host, "indexer.test");
        assert_eq!(request.port, 443);
        assert!(request.is_connect);
    }

    #[test]
    fn turns_a_plain_request_into_origin_form() {
        let head =
            b"GET http://indexer.test:8080/search?q=a HTTP/1.1\r\nHost: indexer.test\r\n\r\n";
        let request = parse_request(head).unwrap();
        assert_eq!(request.host, "indexer.test");
        assert_eq!(request.port, 8080);
        assert!(!request.is_connect);
        assert_eq!(
            request.origin_head,
            b"GET /search?q=a HTTP/1.1\r\nHost: indexer.test\r\n\r\n"
        );
    }

    #[test]
    fn plain_requests_default_to_port_80() {
        let head = b"GET http://indexer.test/ HTTP/1.1\r\n\r\n";
        assert_eq!(parse_request(head).unwrap().port, 80);
    }

    #[test]
    fn malformed_requests_are_rejected() {
        assert!(parse_request(b"nonsense\r\n\r\n").is_none());
    }

    #[tokio::test]
    async fn the_upstream_receives_the_login() {
        let target = mock::start().await.unwrap();
        let proxy = mock::start_proxy(Some("user:pass")).await.unwrap();
        let upstream = upstream_for(&proxy, login("user", "pass"));
        let forwarder = start(upstream).await.unwrap();

        let response = get_through(&forwarder, &format!("http://{target}/open")).await;

        assert_eq!(response.status(), 200);
        assert_eq!(proxy.tunnels(), 1);
        assert_eq!(proxy.auth_seen(), Some(basic_credentials("user:pass")));
    }

    #[tokio::test]
    async fn a_rejected_login_comes_back_as_407() {
        let target = mock::start().await.unwrap();
        let proxy = mock::start_proxy(Some("user:pass")).await.unwrap();
        let upstream = upstream_for(&proxy, login("user", "wrong"));
        let forwarder = start(upstream).await.unwrap();

        let response = get_through(&forwarder, &format!("http://{target}/open")).await;

        assert_eq!(response.status(), 407);
        assert_eq!(proxy.tunnels(), 0);
    }

    #[tokio::test]
    async fn no_login_sends_no_credentials() {
        let target = mock::start().await.unwrap();
        let proxy = mock::start_proxy(None).await.unwrap();
        let forwarder = start(upstream_for(&proxy, None)).await.unwrap();

        let response = get_through(&forwarder, &format!("http://{target}/open")).await;

        assert_eq!(response.status(), 200);
        assert_eq!(proxy.auth_seen(), None);
    }

    #[tokio::test]
    async fn connect_requests_are_tunnelled() {
        let target = mock::start().await.unwrap();
        let proxy = mock::start_proxy(Some("user:pass")).await.unwrap();
        let forwarder = start(upstream_for(&proxy, login("user", "pass")))
            .await
            .unwrap();
        let address = forwarder.trim_start_matches("http://");

        let mut client = TcpStream::connect(address).await.unwrap();
        let connect = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
        client.write_all(connect.as_bytes()).await.unwrap();
        let (head, _) = read_head(&mut client).await.unwrap();
        assert!(head.starts_with(b"HTTP/1.1 200"), "{head:?}");

        let request = format!("GET /open HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n");
        client.write_all(request.as_bytes()).await.unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).await.unwrap();

        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(reply.contains("Search results"), "{reply}");
    }

    #[tokio::test]
    async fn an_unreachable_upstream_is_a_502() {
        let target = mock::start().await.unwrap();
        let upstream = Upstream {
            scheme: Scheme::Http,
            host: "127.0.0.1".into(),
            port: 1,
            login: None,
        };
        let forwarder = start(upstream).await.unwrap();

        let response = get_through(&forwarder, &format!("http://{target}/open")).await;

        assert_eq!(response.status(), 502);
    }

    /// A SOCKS5 proxy that only accepts `user`/`pass`, and counts its tunnels.
    async fn fake_socks() -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let tunnels = Arc::new(AtomicUsize::new(0));
        let counter = tunnels.clone();
        tokio::spawn(async move {
            let (mut client, _) = listener.accept().await.unwrap();
            let mut greeting = [0; 2];
            client.read_exact(&mut greeting).await.unwrap();
            let mut methods = vec![0; greeting[1] as usize];
            client.read_exact(&mut methods).await.unwrap();
            client.write_all(&[5, 2]).await.unwrap();

            let mut version = [0; 2];
            client.read_exact(&mut version).await.unwrap();
            let mut username = vec![0; version[1] as usize];
            client.read_exact(&mut username).await.unwrap();
            let mut length = [0; 1];
            client.read_exact(&mut length).await.unwrap();
            let mut password = vec![0; length[0] as usize];
            client.read_exact(&mut password).await.unwrap();
            let accepted = username == b"user" && password == b"pass";
            client.write_all(&[1, u8::from(!accepted)]).await.unwrap();
            if !accepted {
                return;
            }

            let mut request = [0; 4];
            client.read_exact(&mut request).await.unwrap();
            let target = match request[3] {
                1 => {
                    let mut ip = [0; 4];
                    client.read_exact(&mut ip).await.unwrap();
                    std::net::Ipv4Addr::from(ip).to_string()
                }
                _ => {
                    let mut size = [0; 1];
                    client.read_exact(&mut size).await.unwrap();
                    let mut name = vec![0; size[0] as usize];
                    client.read_exact(&mut name).await.unwrap();
                    String::from_utf8(name).unwrap()
                }
            };
            let mut port_bytes = [0; 2];
            client.read_exact(&mut port_bytes).await.unwrap();
            let port = u16::from_be_bytes(port_bytes);

            let mut origin = TcpStream::connect((target.as_str(), port)).await.unwrap();
            counter.fetch_add(1, Ordering::SeqCst);
            client
                .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            let _ = copy_bidirectional(&mut client, &mut origin).await;
        });
        (port, tunnels)
    }

    #[tokio::test]
    async fn socks5_upstreams_work_with_a_login() {
        let target = mock::start().await.unwrap();
        let (port, tunnels) = fake_socks().await;
        let upstream = Upstream {
            scheme: Scheme::Socks5,
            host: "127.0.0.1".into(),
            port,
            login: login("user", "pass"),
        };
        let forwarder = start(upstream).await.unwrap();

        let response = get_through(&forwarder, &format!("http://{target}/open")).await;

        assert_eq!(response.status(), 200);
        assert_eq!(tunnels.load(Ordering::SeqCst), 1);
    }
}
