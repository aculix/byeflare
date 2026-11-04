//! Resolves `--exit` names: `direct`, or an environment variable holding a
//! proxy URL. Logins stay in the environment, out of commands and results.

use crate::proxy::{self, Upstream};
use crate::replay;
use anyhow::{Context, Result, bail};
use std::time::Duration;

pub const DIRECT: &str = "direct";

const TRACE_URL: &str = "https://www.cloudflare.com/cdn-cgi/trace";
const TRACE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
pub struct Exit {
    pub name: String,
    /// The local forwarder's URL, or nothing for a direct exit.
    pub proxy: Option<String>,
}

/// Reads one exit's proxy from its environment variable's value.
pub fn upstream_for(name: &str, value: Option<&str>) -> Result<Option<Upstream>> {
    if name == DIRECT {
        return Ok(None);
    }
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        bail!("environment variable {name} isn't set");
    };
    Upstream::parse(value)
        .map(Some)
        .with_context(|| format!("{name} isn't a usable proxy"))
}

pub async fn resolve(names: &[String]) -> Result<Vec<Exit>> {
    let mut exits = Vec::new();
    for name in names {
        let value = std::env::var(name).ok();
        let proxy = match upstream_for(name, value.as_deref())? {
            Some(upstream) => Some(proxy::start(upstream).await?),
            None => None,
        };
        exits.push(Exit {
            name: name.clone(),
            proxy,
        });
    }
    Ok(exits)
}

/// Shows which IP an exit really leaves from, and proves the proxy works
/// before any attempts are spent on it.
pub async fn describe(exit: &Exit) -> Result<String> {
    let response = replay::fetch(TRACE_URL, &[], None, exit.proxy.as_deref(), TRACE_TIMEOUT)
        .await
        .context("couldn't reach the internet through this exit")?;
    parse_trace(&response.body).context("couldn't read this exit's IP")
}

/// Reads the IP and country from Cloudflare's trace page.
pub fn parse_trace(body: &str) -> Option<String> {
    let field = |key: &str| body.lines().find_map(|line| line.strip_prefix(key));
    let ip = field("ip=")?;
    Some(match field("loc=") {
        Some(country) => format!("{ip} ({country})"),
        None => ip.to_string(),
    })
}

/// Names a row of results after its engine and, for a proxy, its exit.
pub fn label(engine: &str, exit: &str) -> String {
    if exit == DIRECT {
        engine.to_string()
    } else {
        format!("{engine} via {exit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::Scheme;

    #[test]
    fn direct_needs_no_variable() {
        assert!(upstream_for(DIRECT, None).unwrap().is_none());
    }

    #[test]
    fn an_unset_variable_is_an_error_naming_it() {
        let error = format!("{:#}", upstream_for("PROXY_MISSING", None).unwrap_err());
        assert!(error.contains("PROXY_MISSING"), "{error}");
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        let error = format!("{:#}", upstream_for("PROXY_EMPTY", Some("  ")).unwrap_err());
        assert!(error.contains("PROXY_EMPTY"), "{error}");
    }

    #[test]
    fn a_set_variable_is_read_as_a_proxy() {
        let upstream = upstream_for("PROXY_A", Some("socks5://u:p@proxy.example:1080"))
            .unwrap()
            .unwrap();
        assert_eq!(upstream.scheme, Scheme::Socks5);
        assert_eq!(upstream.host, "proxy.example");
    }

    #[test]
    fn a_bad_proxy_names_the_variable_but_not_its_value() {
        let value = "https://user:s3cret-Pass@proxy.example:443";
        let error = format!("{:#}", upstream_for("PROXY_BAD", Some(value)).unwrap_err());
        assert!(error.contains("PROXY_BAD"), "{error}");
        assert!(!error.contains("s3cret-Pass"), "{error}");
    }

    #[test]
    fn reads_the_ip_and_country_from_a_trace() {
        let trace = "fl=1\nip=203.0.113.7\nts=1.0\nloc=NL\nwarp=off\n";
        assert_eq!(parse_trace(trace).unwrap(), "203.0.113.7 (NL)");
    }

    #[test]
    fn a_trace_without_an_ip_is_none() {
        assert_eq!(parse_trace("fl=1\nloc=NL\n"), None);
    }

    #[test]
    fn a_trace_without_a_country_still_shows_the_ip() {
        assert_eq!(parse_trace("ip=2001:db8::1\n").unwrap(), "2001:db8::1");
    }

    #[test]
    fn direct_rows_are_named_after_the_engine() {
        assert_eq!(label("chrome", DIRECT), "chrome");
    }

    #[test]
    fn proxy_rows_also_name_the_exit() {
        assert_eq!(label("chrome", "PROXY_A"), "chrome via PROXY_A");
    }
}
