use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};

pub struct Input {
    /// Absent when Prowlarr has no proxy configured; it still sends `proxy: {}`.
    pub url: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

const ALLOWED: &[&str] = &[
    "http://",
    "https://",
    "socks4://",
    "socks4a://",
    "socks5://",
    "socks5h://",
];

pub fn resolve(input: Option<&Input>) -> anyhow::Result<Option<String>> {
    let Some(input) = input else {
        return Ok(None);
    };
    let Some(url) = input.url.as_deref().filter(|url| !url.is_empty()) else {
        return Ok(None);
    };
    if !ALLOWED.iter().any(|scheme| url.starts_with(scheme)) {
        anyhow::bail!("unsupported proxy scheme in {url}");
    }
    let Some(user) = &input.username else {
        return Ok(Some(url.to_string()));
    };
    let scheme_end = url.find("://").unwrap() + 3;
    let (scheme, rest) = url.split_at(scheme_end);
    let user = utf8_percent_encode(user, NON_ALPHANUMERIC);
    let password = utf8_percent_encode(input.password.as_deref().unwrap_or(""), NON_ALPHANUMERIC);
    Ok(Some(format!("{scheme}{user}:{password}@{rest}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_proxy_is_none() {
        assert_eq!(resolve(None).unwrap(), None);
    }

    #[test]
    fn an_empty_object_is_no_proxy() {
        let input = Input {
            url: None,
            username: None,
            password: None,
        };
        assert_eq!(resolve(Some(&input)).unwrap(), None);
    }

    #[test]
    fn credentials_are_encoded() {
        let input = Input {
            url: Some("http://host:8080".into()),
            username: Some("u@x".into()),
            password: Some("p#1".into()),
        };
        assert_eq!(
            resolve(Some(&input)).unwrap().unwrap(),
            "http://u%40x:p%231@host:8080"
        );
    }

    #[test]
    fn socks4_passes_through() {
        let input = Input {
            url: Some("socks4://host:1080".into()),
            username: None,
            password: None,
        };
        assert_eq!(
            resolve(Some(&input)).unwrap().unwrap(),
            "socks4://host:1080"
        );
    }

    #[test]
    fn unknown_scheme_errors() {
        let input = Input {
            url: Some("ftp://host".into()),
            username: None,
            password: None,
        };
        assert!(resolve(Some(&input)).is_err());
    }
}
