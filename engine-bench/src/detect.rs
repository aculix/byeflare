//! Prowlarr's challenge detection, ported from `CloudFlareDetectionService.cs`,
//! so a replay is judged the same way Prowlarr would judge it.

#[derive(Debug, Default, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub server: Option<String>,
    pub vary: Option<String>,
    pub content_encoding: Option<String>,
    pub body: String,
}

const PROTECTED_SERVERS: [&str; 3] = ["cloudflare", "cloudflare-nginx", "ddos-guard"];

pub fn is_protected(response: &HttpResponse) -> bool {
    let server = response.server.as_deref().unwrap_or("").to_lowercase();
    if !PROTECTED_SERVERS.contains(&server.as_str()) {
        return false;
    }

    let body = &response.body;
    if matches!(response.status, 403 | 503)
        && (body.contains("<title>Just a moment...</title>")
            || body.contains("<title>Access denied</title>")
            || body.contains("<title>Attention Required! | Cloudflare</title>")
            || body.trim() == "error code: 1020"
            || body.to_lowercase().contains("<title>ddos-guard</title>"))
    {
        return true;
    }

    // Prowlarr's rule for a few Dutch sites that use a custom protection page.
    response.vary.as_deref() == Some("Accept-Encoding,User-Agent")
        && response
            .content_encoding
            .as_deref()
            .is_none_or(|encoding| encoding.trim().is_empty())
        && body.to_lowercase().contains("ddos")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cloudflare(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            server: Some("cloudflare".into()),
            body: body.into(),
            ..Default::default()
        }
    }

    #[test]
    fn challenge_title_on_403_is_protected() {
        assert!(is_protected(&cloudflare(
            403,
            "<html><title>Just a moment...</title></html>"
        )));
    }

    #[test]
    fn challenge_title_on_503_is_protected() {
        assert!(is_protected(&cloudflare(
            503,
            "<title>Just a moment...</title>"
        )));
    }

    #[test]
    fn block_pages_are_protected() {
        for body in [
            "<title>Access denied</title>",
            "<title>Attention Required! | Cloudflare</title>",
            "  error code: 1020\n",
            "<title>ddos-guard</title>",
        ] {
            assert!(is_protected(&cloudflare(403, body)), "{body}");
        }
    }

    #[test]
    fn server_name_matches_case_insensitively() {
        let mut response = cloudflare(403, "<title>Just a moment...</title>");
        response.server = Some("CloudFlare".into());
        assert!(is_protected(&response));
    }

    #[test]
    fn non_cloudflare_server_is_never_protected() {
        let mut response = cloudflare(403, "<title>Just a moment...</title>");
        response.server = Some("nginx".into());
        assert!(!is_protected(&response));
    }

    #[test]
    fn ordinary_403_from_cloudflare_is_not_protected() {
        assert!(!is_protected(&cloudflare(403, "<title>Forbidden</title>")));
    }

    #[test]
    fn challenge_text_on_200_is_not_protected() {
        assert!(!is_protected(&cloudflare(
            200,
            "<title>Just a moment...</title>"
        )));
    }

    #[test]
    fn uncompressed_ddos_page_with_vary_header_is_protected() {
        let mut response = cloudflare(200, "Checking your browser for DDoS protection");
        response.vary = Some("Accept-Encoding,User-Agent".into());
        assert!(is_protected(&response));
    }

    #[test]
    fn compressed_ddos_page_with_vary_header_is_not_protected() {
        let mut response = cloudflare(200, "ddos");
        response.vary = Some("Accept-Encoding,User-Agent".into());
        response.content_encoding = Some("gzip".into());
        assert!(!is_protected(&response));
    }
}
