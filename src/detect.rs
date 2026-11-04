const MARKERS: &[&str] = &[
    "just a moment",
    "cf-browser-verification",
    "challenge-platform",
    "cf-challenge",
    "cf-turnstile",
];

/// Whether a page is a Cloudflare interstitial or widget rather than the real content.
pub fn is_challenged(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    MARKERS.iter().any(|marker| lower.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::is_challenged;

    #[test]
    fn detects_markers() {
        assert!(is_challenged("<title>Just a moment...</title>"));
        assert!(is_challenged(
            "https://challenges.cloudflare.com/cdn-cgi/challenge-platform/x"
        ));
        assert!(is_challenged("<div class=\"cf-turnstile\"></div>"));
        assert!(is_challenged("<script src=\"/cdn-cgi/challenge-platform/x\"></script>"));
    }

    #[test]
    fn ignores_clean_pages() {
        assert!(!is_challenged("<html><body>results</body></html>"));
    }
}
