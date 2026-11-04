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
        E::Network(error) => SolveError::Network(error.to_string()),
        E::Rejected { error, .. } => SolveError::Rejected(error.to_string()),
        E::Challenged { status } => SolveError::Challenged(status.as_u16()),
        E::Unsupported(error) => SolveError::Unsupported(error.to_string()),
        E::Config(error) => SolveError::Config(error.to_string()),
        other => SolveError::Unsupported(other.to_string()),
    }
}

pub struct CfsolveSolver {
    browser: cfsolve::Browser,
    user_agent: String,
}

impl CfsolveSolver {
    /// Loads the profile and the user agent it impersonates.
    pub fn load(profile: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let path = profile.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|error| anyhow::anyhow!("reading {}: {error}", path.display()))?;
        let json: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| anyhow::anyhow!("parsing {}: {error}", path.display()))?;
        let user_agent = json
            .pointer("/transport/headers/User-Agent")
            .and_then(|value| value.as_str())
            .ok_or_else(|| anyhow::anyhow!("profile has no transport.headers.User-Agent"))?
            .to_string();
        Ok(Self {
            browser: cfsolve::Browser::read(path).map_err(|error| anyhow::anyhow!(error))?,
            user_agent,
        })
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }
}

#[async_trait]
impl Solver for CfsolveSolver {
    async fn clear(&self, target: &Target) -> Result<Cleared, SolveError> {
        let url = cfsolve::Url::parse(&target.url)
            .map_err(|error| SolveError::Config(error.to_string()))?;
        let options = cfsolve::Options {
            proxy: target.proxy.clone(),
            sink: None,
        };
        let clearance = self.browser.clear(&url, &options).await.map_err(map_error)?;
        let cookies = clearance
            .cookies
            .into_iter()
            .map(|cookie| crate::domain::NamedCookie {
                name: cookie.name().to_string(),
                value: cookie.value().to_string(),
                domain: cookie.domain().unwrap_or_default().to_string(),
                path: cookie.path().unwrap_or("/").to_string(),
                expires: cookie
                    .expires()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|since| since.as_secs_f64()),
                http_only: cookie.http_only(),
                secure: cookie.secure(),
            })
            .collect();
        Ok(Cleared {
            url: clearance.page.url.to_string(),
            status: clearance.page.status.as_u16(),
            body: clearance.page.html,
            cookies,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_the_bundled_profile_and_its_user_agent() {
        let solver = CfsolveSolver::load("vendor/cfsolve/profiles/safari-26-ios.json").unwrap();
        assert!(solver.user_agent().contains("iPhone"));
    }

    #[test]
    fn a_missing_profile_is_an_error() {
        assert!(CfsolveSolver::load("no-such-profile.json").is_err());
    }

    #[test]
    fn maps_not_challenged() {
        assert_eq!(
            map_error(cfsolve::Error::NotChallenged),
            SolveError::NotChallenged
        );
    }

    #[test]
    fn maps_blocked_status() {
        let error = cfsolve::Error::Blocked {
            status: cfsolve::StatusCode::FORBIDDEN,
        };
        assert_eq!(map_error(error), SolveError::Blocked(403));
    }

    #[test]
    fn maps_retryable_and_fatal_variants() {
        let network = cfsolve::Error::Network(anyhow::anyhow!("reset"));
        assert!(matches!(map_error(network), SolveError::Network(_)));
        let unsupported = cfsolve::Error::Unsupported(anyhow::anyhow!("new build"));
        assert!(matches!(map_error(unsupported), SolveError::Unsupported(_)));
        let config = cfsolve::Error::Config(anyhow::anyhow!("bad profile"));
        assert!(matches!(map_error(config), SolveError::Config(_)));
    }
}
