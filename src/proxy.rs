use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};

pub struct Input {
    pub url: String,
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
    if !ALLOWED.iter().any(|scheme| input.url.starts_with(scheme)) {
        anyhow::bail!("unsupported proxy scheme in {}", input.url);
    }
    let Some(user) = &input.username else {
        return Ok(Some(input.url.clone()));
    };
    let scheme_end = input.url.find("://").unwrap() + 3;
    let (scheme, rest) = input.url.split_at(scheme_end);
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
    fn credentials_are_encoded() {
        let input = Input {
            url: "http://host:8080".into(),
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
            url: "socks4://host:1080".into(),
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
            url: "ftp://host".into(),
            username: None,
            password: None,
        };
        assert!(resolve(Some(&input)).is_err());
    }
}
