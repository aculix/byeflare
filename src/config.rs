use std::path::PathBuf;
use std::time::Duration;

pub struct Config {
    pub bind: String,
    pub profile: PathBuf,
    pub default_timeout: Duration,
    pub max_timeout: Duration,
    pub cache_capacity: usize,
    pub inflight: usize,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            bind: std::env::var("BYEFLARE_BIND").unwrap_or_else(|_| "0.0.0.0:8191".into()),
            profile: std::env::var("BYEFLARE_PROFILE")
                .unwrap_or_else(|_| "vendor/cfsolve/profiles/safari-26-ios.json".into())
                .into(),
            default_timeout: secs("BYEFLARE_DEFAULT_TIMEOUT", 60)?,
            max_timeout: secs("BYEFLARE_MAX_TIMEOUT", 60)?,
            cache_capacity: number("BYEFLARE_CACHE", 512)?,
            inflight: number("BYEFLARE_INFLIGHT", 4)?,
        })
    }
}

fn secs(name: &str, default: u64) -> anyhow::Result<Duration> {
    Ok(Duration::from_secs(number(name, default)?))
}

fn number<T: std::str::FromStr>(name: &str, default: T) -> anyhow::Result<T>
where
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|error| anyhow::anyhow!("{name} is not a number: {error}")),
        Err(_) => Ok(default),
    }
}

/// The navigation header order the profile impersonates.
pub fn header_order(profile: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let text = std::fs::read_to_string(profile)?;
    let json: serde_json::Value = serde_json::from_str(&text)?;
    let entries = json
        .pointer("/transport/requests/headers/navigation")
        .and_then(|value| value.as_array())
        .ok_or_else(|| anyhow::anyhow!("profile has no navigation header order"))?;
    Ok(entries
        .iter()
        .filter_map(|entry| entry.as_str().map(str::to_string))
        .collect())
}
